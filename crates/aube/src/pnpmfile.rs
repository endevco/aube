//! pnpmfile.js hook support.
//!
//! Shells out to `node` to run hooks from a project's `.pnpmfile.mjs`
//! or `.pnpmfile.cjs`,
//! matching pnpm's hook interface closely enough that existing
//! pnpmfiles that use `hooks.afterAllResolved` work unchanged. We
//! pipe a JSON representation of the resolved lockfile to a small
//! node shim, invoke the hook, and apply the narrow set of
//! mutations we understand back to the `LockfileGraph`.
//!
//! `afterAllResolved` is shelled out to a one-shot node child per
//! install. `readPackage` runs inside the resolver's hot loop and is
//! served by a single long-lived node child (see [`ReadPackageHost`])
//! that exchanges newline-delimited JSON messages — one request per
//! version-picked package. Keeping the child resident avoids
//! spawning a fresh `node` per hook (which, on macOS especially,
//! costs tens of milliseconds each and would dominate the resolver
//! budget) and lets the resolver `await` each call in sequence, so
//! its own loop still looks synchronous from its point of view.
//!
//! TODO: once the ecosystem settles, consider replacing the node
//! shellout with an embedded JS runtime or Wasm sandbox so we can
//! drop the hard dependency on `node` at resolve time and cut the
//! per-process overhead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use aube_lockfile::LockfileGraph;
use aube_registry::VersionMetadata;
use aube_resolver::ReadPackageHook;
use miette::{IntoDiagnostic, Result, WrapErr, miette};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

pub const PNPMFILE_MJS_NAME: &str = ".pnpmfile.mjs";
pub const PNPMFILE_CJS_NAME: &str = ".pnpmfile.cjs";

/// Return the path to the project's pnpmfile if one exists.
///
/// `workspace_pnpmfile_path` is the `pnpmfilePath` override from
/// `pnpm-workspace.yaml` (pnpm v10 lets users keep the hook file
/// outside the project root). When set, it wins over the default
/// `cwd/.pnpmfile.mjs` (preferred) or `cwd/.pnpmfile.cjs`; relative paths
/// resolve against `cwd`. An
/// override that points at a missing file is a hard miss (returns
/// `None`) rather than silently falling back, and emits a warning —
/// without the log the user can't tell their typo from "no pnpmfile
/// configured at all". The missing-default case stays silent because
/// "no pnpmfile" is the common case, not a misconfiguration.
pub fn detect(cwd: &Path, workspace_pnpmfile_path: Option<&str>) -> Option<PathBuf> {
    if let Some(rel) = workspace_pnpmfile_path {
        let p = cwd.join(rel);
        if !p.is_file() {
            tracing::warn!(
                "pnpmfilePath override {:?} (from pnpm-workspace.yaml) points at a missing file — hooks will not run",
                p.display().to_string(),
            );
            return None;
        }
        return Some(p);
    }
    for name in [PNPMFILE_MJS_NAME, PNPMFILE_CJS_NAME] {
        let p = cwd.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct LockfileWire {
    importers: BTreeMap<String, Vec<DirectDepWire>>,
    packages: BTreeMap<String, PackageWire>,
}

#[derive(Serialize, Deserialize, Clone)]
struct DirectDepWire {
    name: String,
    version: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct PackageWire {
    name: String,
    version: String,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

fn to_wire(graph: &LockfileGraph) -> LockfileWire {
    let importers = graph
        .importers
        .iter()
        .map(|(path, deps)| {
            let wire = deps
                .iter()
                .map(|d| DirectDepWire {
                    name: d.name.clone(),
                    version: d.dep_path.clone(),
                })
                .collect();
            (path.clone(), wire)
        })
        .collect();
    let packages = graph
        .packages
        .iter()
        .map(|(key, pkg)| {
            (
                key.clone(),
                PackageWire {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    dependencies: pkg.dependencies.clone(),
                    peer_dependencies: pkg.peer_dependencies.clone(),
                },
            )
        })
        .collect();
    LockfileWire {
        importers,
        packages,
    }
}

fn apply(wire: LockfileWire, graph: &mut LockfileGraph) {
    // Only packages[].dependencies and packages[].peerDependencies
    // are honored. Mutations to importers or to a package's
    // name/version are ignored because they would require re-running
    // the resolver to stay consistent; warn about them so the
    // pnpmfile author knows the edit was a no-op.
    for (path, wire_deps) in &wire.importers {
        if let Some(graph_deps) = graph.importers.get(path) {
            let same = graph_deps.len() == wire_deps.len()
                && graph_deps
                    .iter()
                    .zip(wire_deps.iter())
                    .all(|(g, w)| g.name == w.name && g.dep_path == w.version);
            if !same {
                tracing::warn!(
                    "[pnpmfile] afterAllResolved mutated importers[{path}]; \
                     aube ignores importer edits because they would require \
                     re-running the resolver",
                );
            }
        } else {
            tracing::warn!(
                "[pnpmfile] afterAllResolved added importers[{path}]; \
                 aube ignores new importer entries",
            );
        }
    }
    for (key, pkg) in wire.packages {
        if let Some(locked) = graph.packages.get_mut(&key) {
            if pkg.name != locked.name || pkg.version != locked.version {
                tracing::warn!(
                    "[pnpmfile] afterAllResolved rewrote name/version for {key} \
                     (to {}@{}); aube ignores identity edits on existing packages",
                    pkg.name,
                    pkg.version,
                );
            }
            if locked.dependencies != pkg.dependencies {
                locked.dependencies = pkg.dependencies;
            }
            if locked.peer_dependencies != pkg.peer_dependencies {
                locked.peer_dependencies = pkg.peer_dependencies;
            }
        } else {
            tracing::warn!(
                "[pnpmfile] afterAllResolved added a new package entry {key}; \
                 aube ignores newly-introduced packages from the hook",
            );
        }
    }
}

const LOAD_PNPMFILE_JS: &str = r#"
const path = require('path');
const { pathToFileURL } = require('url');
async function loadPnpmfile(file) {
  const resolved = path.resolve(file);
  const mod = resolved.endsWith('.mjs')
    ? await import(pathToFileURL(resolved).href)
    : require(resolved);
  if (mod && mod.default && !mod.default.hooks && mod.hooks) {
    console.error('[pnpmfile] default export has no hooks; using named hooks export');
    return mod;
  }
  return (mod && (mod.default || mod)) || {};
}
"#;

const SHIM: &str = r#"
const pnpmfile = process.env.AUBE_PNPMFILE;
const hookName = process.env.AUBE_HOOK;
let chunks = [];
process.stdin.on('data', (c) => chunks.push(c));
process.stdin.on('end', async () => {
  try {
    const input = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    const mod = await loadPnpmfile(pnpmfile);
    const hooks = (mod && mod.hooks) || {};
    const fn = hooks[hookName];
    let result = input;
    if (typeof fn === 'function') {
      const ctx = {
        log: (...args) => console.error('[pnpmfile]', ...args),
      };
      const out = await fn(input, ctx);
      if (out && typeof out === 'object') result = out;
    }
    process.stdout.write(JSON.stringify(result));
  } catch (err) {
    console.error('[pnpmfile] hook failed:', (err && err.stack) || err);
    process.exit(1);
  }
});
"#;

/// Generic shim used for any one-shot hook (`afterAllResolved`,
/// `preResolution`, …). Dispatches on `process.env.AUBE_HOOK` so a new
/// one-shot hook only needs a `run_one_shot_hook(.., name, ..)` call —
/// don't add a parallel shim.
fn one_shot_hook_shim() -> String {
    format!("{LOAD_PNPMFILE_JS}{SHIM}")
}

/// Spawn a one-shot `node` child running the shared shim for `hook_name`,
/// pipe `input_json` in on stdin, and return the captured stdout. Shared
/// scaffolding for `afterAllResolved` (which round-trips a lockfile) and
/// `preResolution` (which fires-and-forgets a context object).
async fn run_one_shot_hook(pnpmfile: &Path, hook_name: &str, input_json: &[u8]) -> Result<Vec<u8>> {
    tracing::debug!("running pnpmfile hook {hook_name} ({})", pnpmfile.display());

    let mut cmd = tokio::process::Command::new("node");
    cmd.arg("-e")
        .arg(one_shot_hook_shim())
        .env("AUBE_PNPMFILE", pnpmfile)
        .env("AUBE_HOOK", hook_name)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        // Match ReadPackageHost::spawn below. Without kill_on_drop the
        // Node child keeps running when the parent future is cancelled
        // (install panics, user Ctrl-C's, etc) and the hook body races
        // on past stdin close. Unlikely to bite in practice but
        // zero-cost to guard.
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .into_diagnostic()
        .wrap_err("failed to spawn `node` for pnpmfile hook — is node installed and on PATH?")?;

    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| miette!("failed to open stdin for pnpmfile node child"))?;
        stdin
            .write_all(input_json)
            .await
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write JSON to pnpmfile {hook_name} hook"))?;
        stdin
            .shutdown()
            .await
            .into_diagnostic()
            .wrap_err("failed to close stdin for pnpmfile hook")?;
    }

    let output = child
        .wait_with_output()
        .await
        .into_diagnostic()
        .wrap_err("pnpmfile hook child process failed")?;
    if !output.status.success() {
        return Err(miette!(
            "pnpmfile hook `{hook_name}` exited with status {}",
            output.status
        ));
    }
    Ok(output.stdout)
}

/// Run the `afterAllResolved` hook against a resolved lockfile graph.
/// Mutations to `packages[].dependencies` and `packages[].peerDependencies`
/// are applied in place. All other fields are round-tripped but
/// ignored on the way back.
pub async fn run_after_all_resolved(pnpmfile: &Path, graph: &mut LockfileGraph) -> Result<()> {
    let input = to_wire(graph);
    let input_json = serde_json::to_vec(&input)
        .into_diagnostic()
        .wrap_err("failed to serialize lockfile for pnpmfile hook")?;
    let stdout = run_one_shot_hook(pnpmfile, "afterAllResolved", &input_json).await?;
    let wire: LockfileWire = serde_json::from_slice(&stdout)
        .into_diagnostic()
        .wrap_err("pnpmfile hook returned invalid JSON from afterAllResolved")?;
    apply(wire, graph);
    Ok(())
}

/// Snapshot passed to the `preResolution` hook before resolve starts.
/// Mirrors pnpm's context shape (camelCase on the wire) so existing
/// pnpmfiles can read the fields they expect.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreResolutionContext<'a> {
    pub lockfile_dir: &'a Path,
    pub store_dir: Option<&'a Path>,
    pub current_lockfile: Option<LockfileWire>,
    pub wanted_lockfile: Option<LockfileWire>,
    pub exists_current_lockfile: bool,
    pub exists_non_empty_wanted_lockfile: bool,
    pub registries: BTreeMap<String, String>,
}

impl<'a> PreResolutionContext<'a> {
    /// Build the snapshot for `lockfile_dir`. `existing` is the on-disk
    /// lockfile graph (or `None` when there isn't one); both
    /// `currentLockfile` and `wantedLockfile` are derived from it
    /// because at preResolution time they're identical — pnpm only
    /// diverges them after resolve has produced the wanted graph.
    pub fn from_existing(
        lockfile_dir: &'a Path,
        store_dir: Option<&'a Path>,
        existing: Option<&LockfileGraph>,
        registries: BTreeMap<String, String>,
    ) -> Self {
        let wire = existing.map(to_wire);
        let exists_current_lockfile = existing.is_some();
        let exists_non_empty_wanted_lockfile = wire
            .as_ref()
            .is_some_and(|w| !w.importers.is_empty() || !w.packages.is_empty());
        Self {
            lockfile_dir,
            store_dir,
            current_lockfile: wire.clone(),
            wanted_lockfile: wire,
            exists_current_lockfile,
            exists_non_empty_wanted_lockfile,
            registries,
        }
    }
}

/// Run the `preResolution` hook before the resolver walks the graph.
/// Fire-and-forget — the hook's return value is discarded by pnpm and
/// by aube. Skips spawning `node` when the pnpmfile doesn't reference
/// `preResolution` so a hook-less pnpmfile doesn't pay the per-install
/// node-startup cost on every command.
pub async fn run_pre_resolution(pnpmfile: &Path, ctx: &PreResolutionContext<'_>) -> Result<()> {
    if !has_hook(pnpmfile, "preResolution").await? {
        return Ok(());
    }
    let input_json = serde_json::to_vec(ctx)
        .into_diagnostic()
        .wrap_err("failed to serialize preResolution context")?;
    run_one_shot_hook(pnpmfile, "preResolution", &input_json).await?;
    Ok(())
}

/// Node shim for the long-lived `readPackage` host. Reads NDJSON
/// requests of the form `{"id":N,"pkg":{...}}` on stdin and writes
/// one response per line on stdout: either `{"id":N,"pkg":{...}}` or
/// `{"id":N,"error":"..."}`. The hook module is `require`d exactly
/// once at startup, so filesystem I/O and V8 compilation aren't
/// repeated per call. Calls are processed sequentially — the
/// resolver already serializes them, and a sequential loop sidesteps
/// the interleaving hazards you'd otherwise get from async readline
/// callbacks.
const READ_PACKAGE_SHIM: &str = r#"
const readline = require('readline');
const pnpmfile = process.env.AUBE_PNPMFILE;
const ctx = {
  log: (...args) => console.error('[pnpmfile]', ...args),
};
process.stdout.on('error', (e) => {
  if (e && e.code === 'EPIPE') process.exit(0);
});
async function main() {
  const mod = await loadPnpmfile(pnpmfile);
  const hooks = (mod && mod.hooks) || {};
  const readPackage = hooks.readPackage;
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  for await (const line of rl) {
    if (!line) continue;
    let id = null;
    try {
      const req = JSON.parse(line);
      id = req.id;
      let result = req.pkg;
      if (typeof readPackage === 'function') {
        const out = await readPackage(req.pkg, ctx);
        if (out && typeof out === 'object') result = out;
      }
      process.stdout.write(JSON.stringify({ id, pkg: result }) + '\n');
    } catch (err) {
      const msg = (err && err.stack) || String(err);
      process.stdout.write(JSON.stringify({ id, error: String(msg) }) + '\n');
    }
  }
}
main().catch((err) => {
  console.error('[pnpmfile] readPackage host crashed:', (err && err.stack) || err);
  process.exit(1);
});
"#;

fn read_package_shim() -> String {
    format!("{LOAD_PNPMFILE_JS}{READ_PACKAGE_SHIM}")
}

/// Long-lived node child that answers `readPackage` calls one at a
/// time. Owned by the install command for the span of a single
/// resolve, then dropped (which kills the child). Implements
/// [`ReadPackageHook`] so the resolver can call it directly from its
/// hot loop.
pub struct ReadPackageHost {
    // Held only so Drop kills the child when the host is torn down;
    // `kill_on_drop(true)` above wires the actual kill.
    #[allow(dead_code)]
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    line_buf: String,
}

#[derive(Serialize)]
struct ReadPackageRequest<'a> {
    id: u64,
    pkg: &'a VersionMetadata,
}

#[derive(Deserialize)]
struct ReadPackageResponse {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    pkg: Option<VersionMetadata>,
    #[serde(default)]
    error: Option<String>,
}

impl ReadPackageHost {
    /// Spawn the node child for `pnpmfile`. Returns `Ok(None)` if the
    /// pnpmfile does not declare a `readPackage` hook (callers can
    /// skip attaching a hook entirely in that case and save the
    /// per-call JSON round-trip), otherwise the live host.
    pub async fn spawn(pnpmfile: &Path) -> Result<Option<Self>> {
        if !has_hook(pnpmfile, "readPackage").await? {
            return Ok(None);
        }
        tracing::debug!(
            "spawning pnpmfile readPackage host ({})",
            pnpmfile.display()
        );
        let mut cmd = tokio::process::Command::new("node");
        cmd.arg("-e")
            .arg(read_package_shim())
            .env("AUBE_PNPMFILE", pnpmfile)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().into_diagnostic().wrap_err(
            "failed to spawn `node` for pnpmfile readPackage hook — is node installed and on PATH?",
        )?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| miette!("failed to open stdin for pnpmfile readPackage host"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| miette!("failed to open stdout for pnpmfile readPackage host"))?;
        Ok(Some(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            line_buf: String::new(),
        }))
    }

    async fn call(&mut self, pkg: VersionMetadata) -> Result<VersionMetadata, String> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let req = ReadPackageRequest { id, pkg: &pkg };
        let mut line = serde_json::to_string(&req)
            .map_err(|e| format!("serialize readPackage request: {e}"))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to readPackage host: {e}"))?;
        self.line_buf.clear();
        let n = self
            .stdout
            .read_line(&mut self.line_buf)
            .await
            .map_err(|e| format!("read from readPackage host: {e}"))?;
        if n == 0 {
            return Err(
                "readPackage host closed stdout unexpectedly (check stderr for the hook stack trace)"
                    .to_string(),
            );
        }
        let resp: ReadPackageResponse = serde_json::from_str(self.line_buf.trim_end())
            .map_err(|e| format!("parse readPackage response: {e}"))?;
        // Protocol sanity check. The resolver calls us strictly in
        // lockstep, so a mismatch here means the node shim printed an
        // extra line to stdout (usually a `require`-time warning) and
        // we've fallen out of sync. Fail loudly instead of silently
        // consuming a stale response — a future debug session will
        // thank us.
        if let Some(resp_id) = resp.id
            && resp_id != id
        {
            return Err(format!(
                "readPackage response id mismatch: sent {id}, got {resp_id} \
                 (did the pnpmfile print to stdout at require time?)"
            ));
        }
        // The hook's return value is surfaced untouched — the resolver
        // owns identity/platform restoration *after* its own warning
        // check, so pre-sanitizing here would silently swallow hook
        // edits to name/version.
        if let Some(err) = resp.error {
            return Err(err);
        }
        resp.pkg
            .ok_or_else(|| "readPackage response missing `pkg`".to_string())
    }
}

impl ReadPackageHook for ReadPackageHost {
    fn read_package<'a>(
        &'a mut self,
        pkg: VersionMetadata,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<VersionMetadata, String>> + Send + 'a>>
    {
        Box::pin(self.call(pkg))
    }
}

/// Quick scan of the pnpmfile source for a hook identifier. Avoids
/// the cost of spawning a node child when the hook doesn't exist —
/// the vast majority of pnpmfiles use only `afterAllResolved`. False
/// positives are fine: if a pnpmfile references the hook name in a
/// comment but doesn't export it, the child spawns, the hook is
/// absent, and the call short-circuits through the
/// `typeof ... === 'function'` check in the shim.
async fn has_hook(pnpmfile: &Path, name: &str) -> Result<bool> {
    let contents = tokio::fs::read_to_string(pnpmfile)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read pnpmfile at {}", pnpmfile.display()))?;
    Ok(contents.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_default_when_present_and_no_override() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        let found = detect(dir.path(), None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_CJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_returns_mjs_when_only_mjs_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_MJS_NAME), "").unwrap();
        let found = detect(dir.path(), None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_MJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_prefers_mjs_over_cjs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_MJS_NAME), "").unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        let found = detect(dir.path(), None);
        assert_eq!(
            found.as_deref(),
            Some(dir.path().join(PNPMFILE_MJS_NAME).as_path())
        );
    }

    #[test]
    fn detect_returns_none_when_default_missing_and_no_override() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect(dir.path(), None).is_none());
    }

    #[test]
    fn detect_honors_workspace_pnpmfile_path_override() {
        // pnpm v10 allows `pnpmfilePath: config/hooks.cjs` in
        // pnpm-workspace.yaml to keep hooks outside the project root.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        let custom = dir.path().join("config/hooks.cjs");
        std::fs::write(&custom, "").unwrap();
        // Even though .pnpmfile.cjs doesn't exist at the default
        // location, the workspace override points at the real file.
        let found = detect(dir.path(), Some("config/hooks.cjs"));
        assert_eq!(found.as_deref(), Some(custom.as_path()));
    }

    #[test]
    fn detect_workspace_override_returns_none_when_target_missing() {
        // A typo in `pnpmfilePath` must surface as "not loaded" rather
        // than silently falling back to `.pnpmfile.cjs` — otherwise the
        // user thinks their hooks are running when they aren't.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PNPMFILE_CJS_NAME), "").unwrap();
        assert!(detect(dir.path(), Some("typo/missing.cjs")).is_none());
    }
}
