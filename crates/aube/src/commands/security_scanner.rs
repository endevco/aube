//! Bun-compatible pluggable security scanner.
//!
//! Loads and runs a [Bun Security Scanner](https://bun.sh/docs/pm/security-scanner-api)
//! module. Drop-in compatible with the existing Bun ecosystem of
//! scanner packages — the user configures the same npm package
//! name in `aube-workspace.yaml#securityScanner` that they would
//! in Bun's `bunfig.toml#install.security.scanner`, and aube loads
//! the module through a `node` bridge that adapts Bun's in-process
//! plugin API to a subprocess + JSON-over-stdio shape.
//!
//! **Fired from**:
//! - `aube add` — the packages typed on the command line, via
//!   [`run_scanner`] from `commands::add`.
//! - `aube install` — direct deps from the root manifest (see
//!   [`direct_deps_for_scanner`]), via the same [`run_scanner`]
//!   entry point.
//!
//! `add` is the moment-of-human-intent gate; `install` is the
//! "your project's deps are about to materialize" gate. Matches
//! Bun, which runs its scanner on both `bun add` and `bun install`.
//!
//! ## Contract differences vs. Bun
//!
//! Bun runs the scanner *after* the resolver picks concrete
//! versions, so `package.version` is the resolved version string
//! (e.g. `"4.17.21"`). aube runs it *before* the resolver, so
//! `package.version` is the requested range verbatim from the
//! manifest (e.g. `"^4.17.21"`, `"latest"`). Scanners that match
//! on exact version strings will see misses they wouldn't under
//! Bun; scanners that match on package *name* (the
//! typosquat/malware case, which is the vast majority of public
//! scanners) work identically.
//!
//! Bun loads the scanner module in-process via its own JS runtime;
//! aube spawns `node` because aube is Rust and can't host JS
//! itself. Functionally equivalent for any scanner that doesn't
//! depend on Bun-specific runtime APIs (`Bun.spawn`, `Bun.file`,
//! etc.). TypeScript scanners must be compiled to JS before use —
//! `node` doesn't strip type annotations natively. The official
//! [security-scanner-template](https://github.com/oven-sh/security-scanner-template)
//! already ships compiled JS in its npm tarball, as do all the
//! commercial scanners.

use aube_codes::errors::ERR_AUBE_SECURITY_SCANNER_FATAL;
use aube_codes::warnings::{WARN_AUBE_SECURITY_SCANNER_FAILED, WARN_AUBE_SECURITY_SCANNER_FINDING};
use miette::miette;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Hard upper bound on how long the scanner may run. A scanner
/// that hangs forever shouldn't be able to wedge `aube install`.
/// 30s mirrors what npm and bun use for similar install-time hooks.
const SCANNER_TIMEOUT: Duration = Duration::from_secs(30);

/// Inline ESM runner that aube hands to `node` via `-e`. Resolves
/// the user's scanner module (npm package name → node_modules
/// lookup; relative path → file URL), reads the JSON payload from
/// stdin, calls `scanner.scan(payload)`, and writes the resulting
/// advisory array to stdout. Mirrors Bun's in-process plugin
/// contract so the user's scanner module sees the exact same call
/// shape it'd see under `bun install`.
///
/// Failures (resolution error, missing `scan()`, thrown exception)
/// exit non-zero with a stderr line — `invoke()` then surfaces
/// `WARN_AUBE_SECURITY_SCANNER_FAILED` and the install fails open.
///
/// Bun's docs specify the return value is `Advisory[]`. We also
/// accept `{ advisories: [...] }` as a friendly fallback for
/// scanners that wrap their result.
const NODE_BRIDGE_RUNNER: &str = r#"
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { resolve as pathResolve } from 'node:path';

const spec = process.env.AUBE_SCANNER_SPEC;
if (!spec) {
  console.error('AUBE_SCANNER_SPEC env not set');
  process.exit(2);
}
const cwd = process.cwd();

async function loadScanner(spec) {
  // Path-like specs (`./foo`, `/foo`, `C:\\foo`) → resolve to a
  // file URL so dynamic import sees an unambiguous target.
  if (spec.startsWith('.') || spec.startsWith('/') || /^[a-zA-Z]:[/\\]/.test(spec)) {
    const abs = pathResolve(cwd, spec);
    return import(pathToFileURL(abs).href);
  }
  // Bare npm package name → node resolves from cwd's
  // `node_modules`. Try ESM first; fall back to CJS via
  // `createRequire` for older scanner packages that haven't
  // shipped an ESM export.
  try {
    return await import(spec);
  } catch (e) {
    try {
      const require = createRequire(pathResolve(cwd, 'package.json'));
      return require(spec);
    } catch {
      throw e;
    }
  }
}

let mod;
try {
  mod = await loadScanner(spec);
} catch (e) {
  console.error(`failed to load scanner ${spec}: ${e?.message ?? e}`);
  process.exit(3);
}

// Accept the canonical Bun shape (`export const scanner = {...}`)
// plus a couple of common variants — `export default scanner`,
// default-export-is-the-scanner, or default-export-has-a-scanner-
// property. Keeps the bridge from breaking when scanner authors
// rearrange their entry points.
const scanner = mod?.scanner ?? mod?.default?.scanner ?? mod?.default ?? mod;
if (!scanner || typeof scanner.scan !== 'function') {
  console.error(`scanner ${spec} does not export a 'scan' function`);
  process.exit(4);
}

let buf = '';
for await (const chunk of process.stdin) buf += chunk;
const payload = JSON.parse(buf);

let result;
try {
  result = await scanner.scan(payload);
} catch (e) {
  console.error(`scanner.scan() threw: ${e?.message ?? e}`);
  process.exit(5);
}

const advisories = Array.isArray(result) ? result : (result?.advisories ?? []);
process.stdout.write(JSON.stringify(advisories));
"#;

/// One package the scanner will see. Field names match
/// `Bun.Security.Package`: `name` is the registry name, `version`
/// is what Bun calls the version specifier (resolved version under
/// Bun, requested range under aube — see the module-level note).
#[derive(Debug, Clone, Serialize)]
pub struct ScannerPackage {
    pub name: String,
    pub version: String,
}

/// Collect direct deps from a parsed root manifest into the
/// scanner's input format. Skips workspace / catalog / git / file /
/// link / jsr / npm-alias specs — the scanner runs against
/// public-registry names, and none of those route through public
/// registry names where an external advisory would apply.
///
/// Used by `aube install` to feed every direct dep through the
/// scanner on a manifest-driven install. `aube add` has its own
/// per-spec parsing path that produces the same `ScannerPackage`
/// shape but starts from the user-typed argument list.
pub fn direct_deps_for_scanner(manifest: &aube_manifest::PackageJson) -> Vec<ScannerPackage> {
    let mut out = Vec::new();
    let chains = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter());
    for (name, spec) in chains {
        if !is_registry_scannable(spec) {
            continue;
        }
        out.push(ScannerPackage {
            name: name.clone(),
            version: spec.clone(),
        });
    }
    // BTreeMap iteration is already sorted by key, but
    // `dependencies` and `devDependencies` will produce duplicates
    // for packages declared in both. Keep the first occurrence —
    // `dependencies` outranks dev in the chain order above, so the
    // production spec wins.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

/// Return `true` when `spec` is a public-registry version range
/// (e.g. `^1.2.3`, `~4`, `latest`, `*`) and `false` for every
/// non-registry routing form aube understands.
fn is_registry_scannable(spec: &str) -> bool {
    !(aube_util::pkg::is_workspace_spec(spec)
        || aube_util::pkg::is_catalog_spec(spec)
        || aube_util::pkg::is_jsr_spec(spec)
        || aube_util::pkg::is_npm_spec(spec)
        || aube_util::pkg::is_file_spec(spec)
        || aube_util::pkg::is_link_spec(spec)
        || aube_lockfile::parse_git_spec(spec).is_some())
}

#[derive(Debug, Serialize)]
struct ScannerRequest<'a> {
    packages: &'a [ScannerPackage],
}

#[derive(Debug, Deserialize)]
struct Advisory {
    package: String,
    level: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: Option<String>,
}

/// Outcome categories used when classifying scanner advisories.
/// `Fatal` blocks the install; `Warn` emits a warning and
/// continues; `Other` is logged at debug level and otherwise
/// ignored — future-proof for additional levels without changing
/// the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Fatal,
    Warn,
    Other,
}

fn classify(level: &str) -> Severity {
    match level.to_ascii_lowercase().as_str() {
        "fatal" => Severity::Fatal,
        "warn" | "warning" => Severity::Warn,
        _ => Severity::Other,
    }
}

/// Run `scanner_spec` against the candidate `packages`. Empty
/// `scanner_spec` or empty `packages` short-circuits to `Ok(())`
/// without spawning `node` — the caller already filtered to
/// registry-bound names, and there's nothing useful to scan
/// beyond that.
pub async fn run_scanner(
    scanner_spec: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> miette::Result<()> {
    if scanner_spec.is_empty() || packages.is_empty() {
        return Ok(());
    }
    let advisories = match invoke(scanner_spec, cwd, packages).await {
        Ok(a) => a,
        Err(e) => {
            // Fail open: a misconfigured scanner (node not
            // installed, module not in `node_modules` yet,
            // scanner threw) shouldn't break every install in
            // the project. The operator sees the warning and
            // can fix their setup; the install continues using
            // whatever other gates are configured.
            tracing::warn!(
                code = WARN_AUBE_SECURITY_SCANNER_FAILED,
                "securityScanner `{scanner_spec}` failed: {e}"
            );
            return Ok(());
        }
    };
    apply_advisories(scanner_spec, &advisories)
}

async fn invoke(
    scanner_spec: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> Result<Vec<Advisory>, String> {
    let request = ScannerRequest { packages };
    let body = serde_json::to_vec(&request)
        .map_err(|e| format!("failed to encode scanner request: {e}"))?;

    let mut cmd = tokio::process::Command::new("node");
    cmd.current_dir(cwd)
        .arg("--input-type=module")
        .arg("-e")
        .arg(NODE_BRIDGE_RUNNER)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Pass the scanner spec via env (not argv) so we don't
        // have to fight node's `-e <script>` argv handling, which
        // varies across versions and `--input-type=module`.
        .env("AUBE_SCANNER_SPEC", scanner_spec)
        // The scanner runs against unresolved package specs — it
        // has no business with npm auth tokens or registry
        // credentials. Scrubbing them keeps a hostile or buggy
        // scanner from exfiltrating them as a side effect.
        .env_remove("NPM_TOKEN")
        .env_remove("NODE_AUTH_TOKEN")
        .env_remove("GITHUB_TOKEN");

    let mut child = cmd.spawn().map_err(|e| {
        // Most common cause: `node` isn't on PATH. The error
        // string from std::io::Error already includes
        // "No such file or directory" or the platform
        // equivalent, which is enough signal for the operator.
        format!("failed to spawn `node` for scanner bridge: {e}")
    })?;

    // Take stdin out of the child so we can write to it. The
    // bridge runner reads stdin to EOF before invoking the
    // scanner; closing it here is what triggers that.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "internal pipe error: stdin not available".to_string())?;
    use tokio::io::AsyncWriteExt;
    stdin
        .write_all(&body)
        .await
        .map_err(|e| format!("failed to write request to scanner stdin: {e}"))?;
    drop(stdin);

    let wait = child.wait_with_output();
    let output = tokio::time::timeout(SCANNER_TIMEOUT, wait)
        .await
        .map_err(|_| {
            format!(
                "scanner exceeded {} second timeout",
                SCANNER_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("failed to wait for scanner subprocess: {e}"))?;

    if !output.status.success() {
        // Surface stderr (truncated) so the operator can diagnose
        // what's wrong with their scanner. The bridge runner
        // writes a single-line diagnostic before exiting; node
        // appends a stack trace if the scanner threw.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let snippet = if trimmed.len() > 500 {
            format!("{}…", &trimmed[..500])
        } else {
            trimmed.to_string()
        };
        return Err(format!(
            "scanner exited with status {:?}; stderr: {snippet}",
            output.status.code()
        ));
    }

    serde_json::from_slice::<Vec<Advisory>>(&output.stdout)
        .map_err(|e| format!("scanner stdout was not a JSON advisory array: {e}"))
}

fn apply_advisories(scanner_spec: &str, advisories: &[Advisory]) -> miette::Result<()> {
    let mut fatal: Vec<&Advisory> = Vec::new();
    for adv in advisories {
        match classify(&adv.level) {
            Severity::Fatal => fatal.push(adv),
            Severity::Warn => {
                let url_suffix = adv
                    .url
                    .as_deref()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                tracing::warn!(
                    code = WARN_AUBE_SECURITY_SCANNER_FINDING,
                    "{}: {}{}",
                    adv.package,
                    if adv.description.is_empty() {
                        "flagged by securityScanner"
                    } else {
                        adv.description.as_str()
                    },
                    url_suffix
                );
            }
            Severity::Other => {
                tracing::debug!(
                    "securityScanner reported level={} for {}: {}",
                    adv.level,
                    adv.package,
                    adv.description
                );
            }
        }
    }
    if fatal.is_empty() {
        return Ok(());
    }
    let mut lines = vec![format!(
        "refusing to install package(s) flagged by `securityScanner = {scanner_spec}`:"
    )];
    for adv in &fatal {
        let url_suffix = adv
            .url
            .as_deref()
            .map(|u| format!(" — {u}"))
            .unwrap_or_default();
        let body = if adv.description.is_empty() {
            "(no description)".to_string()
        } else {
            adv.description.clone()
        };
        lines.push(format!("  - {}: {}{url_suffix}", adv.package, body));
    }
    Err(miette!(
        code = ERR_AUBE_SECURITY_SCANNER_FATAL,
        "{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(package: &str, level: &str) -> Advisory {
        Advisory {
            package: package.to_string(),
            level: level.to_string(),
            description: String::new(),
            url: None,
        }
    }

    #[test]
    fn classify_is_case_insensitive() {
        // Bun's docs are case-sensitive (`fatal`, `warn`), but
        // scanner authors are inconsistent in practice. Accept
        // `FATAL` / `Warning` / `warn` interchangeably so a
        // scanner that loosely matches the spec still works.
        assert_eq!(classify("FATAL"), Severity::Fatal);
        assert_eq!(classify("fatal"), Severity::Fatal);
        assert_eq!(classify("Warning"), Severity::Warn);
        assert_eq!(classify("warn"), Severity::Warn);
        assert_eq!(classify("info"), Severity::Other);
        assert_eq!(classify(""), Severity::Other);
    }

    #[test]
    fn apply_advisories_empty_is_ok() {
        // No advisories ⇒ no block, no warning emitted.
        assert!(apply_advisories("/some/scanner", &[]).is_ok());
    }

    #[test]
    fn apply_advisories_warn_only_does_not_block() {
        // A scanner that reports only `warn`-level findings lets
        // the install through. Warnings surface via tracing; the
        // miette error path is reserved for `fatal`.
        let advs = vec![adv("pkg-a", "warn"), adv("pkg-b", "warning")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    #[test]
    fn apply_advisories_fatal_blocks() {
        // One fatal advisory is enough — install refused.
        let advs = vec![adv("pkg-a", "warn"), adv("evil", "fatal")];
        let err = apply_advisories("scanner", &advs).unwrap_err();
        let msg = format!("{err:?}");
        // Both package and scanner identity should be in the
        // error message so the user knows what blocked and why.
        assert!(msg.contains("evil"), "missing package name: {msg}");
        assert!(msg.contains("scanner"), "missing scanner ref: {msg}");
    }

    #[test]
    fn unknown_severity_falls_through() {
        // A future-dated scanner emitting an unrecognized level
        // (e.g. `info`) should not block or warn — we don't know
        // if it's a structural issue or just chatter, and the
        // contract is explicit that only `fatal` blocks and `warn`
        // surfaces.
        let advs = vec![adv("pkg-a", "info"), adv("pkg-b", "trace")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    #[test]
    fn registry_scannable_only_keeps_semver_specs() {
        // Specs that should reach the scanner.
        assert!(is_registry_scannable("^1.0.0"));
        assert!(is_registry_scannable("~4.17"));
        assert!(is_registry_scannable("latest"));
        assert!(is_registry_scannable("*"));
        assert!(is_registry_scannable("1.2.3 || 1.3.0"));

        // Specs that route through non-public-registry paths.
        assert!(!is_registry_scannable("workspace:*"));
        assert!(!is_registry_scannable("workspace:^"));
        assert!(!is_registry_scannable("catalog:"));
        assert!(!is_registry_scannable("catalog:default"));
        assert!(!is_registry_scannable("file:./packages/foo"));
        assert!(!is_registry_scannable("link:../sibling"));
        assert!(!is_registry_scannable("jsr:@std/collections@^1"));
        assert!(!is_registry_scannable("npm:other-pkg@^4"));
        assert!(!is_registry_scannable("github:kevva/is-negative"));
        assert!(!is_registry_scannable("git+https://example.com/r.git"));
    }

    #[test]
    fn direct_deps_collects_across_dep_kinds_and_dedupes() {
        // A package declared in both `dependencies` and
        // `devDependencies` should appear once, with the
        // `dependencies` spec winning (chain order).
        let mut manifest = aube_manifest::PackageJson::default();
        manifest
            .dependencies
            .insert("lodash".to_string(), "^4.17.21".to_string());
        manifest
            .dev_dependencies
            .insert("lodash".to_string(), "^4.17.0".to_string());
        manifest
            .dev_dependencies
            .insert("vitest".to_string(), "^2".to_string());
        manifest
            .optional_dependencies
            .insert("fsevents".to_string(), "^2.3".to_string());
        // Non-scannable specs that must be filtered out.
        manifest
            .dependencies
            .insert("@my/pkg".to_string(), "workspace:^".to_string());
        manifest
            .dependencies
            .insert("local-thing".to_string(), "file:./local".to_string());

        let packages = direct_deps_for_scanner(&manifest);
        let names: Vec<&str> = packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["fsevents", "lodash", "vitest"]);
        let lodash = packages.iter().find(|p| p.name == "lodash").unwrap();
        assert_eq!(
            lodash.version, "^4.17.21",
            "production spec should win over dev"
        );
    }

    /// Returns true iff `node` is on PATH and responsive. e2e
    /// tests that invoke the real bridge gate themselves on this
    /// — the dev box / CI runner might not have node installed,
    /// and the scanner is opt-in anyway.
    fn node_available() -> bool {
        std::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Write a minimal Bun-shape scanner module to `path`. The
    /// scanner reads `payload.packages`, scans for `target_name`
    /// (case-sensitive), and emits one advisory of the given
    /// level when it matches. Keeps the fixture small enough to
    /// inline so the test reads top-to-bottom.
    fn write_bun_scanner(path: &Path, target_name: &str, level: &str) {
        let body = format!(
            r#"export const scanner = {{
  version: '1',
  async scan({{ packages }}) {{
    const hits = [];
    for (const p of packages) {{
      if (p.name === {target:?}) {{
        hits.push({{
          level: {level:?},
          package: p.name,
          description: 'mock',
          url: 'https://example.org/mock',
        }});
      }}
    }}
    return hits;
  }},
}};
"#,
            target = target_name,
            level = level,
        );
        std::fs::write(path, body).unwrap();
    }

    /// End-to-end: drop a real Bun-shape `.mjs` module on disk,
    /// run the bridge against it, and verify the fatal path
    /// surfaces `ERR_AUBE_SECURITY_SCANNER_FATAL` with the
    /// expected package + description content. Exercises the
    /// inline node runner script, stdin piping, JSON parsing,
    /// and policy layer end-to-end.
    #[tokio::test]
    async fn end_to_end_blocks_on_fatal_advisory() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        write_bun_scanner(&scanner_path, "evil", "fatal");

        let pkgs = vec![ScannerPackage {
            name: "evil".to_string(),
            version: "latest".to_string(),
        }];
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("evil"), "missing pkg in error: {msg}");
        assert!(msg.contains("mock"), "missing description in error: {msg}");
    }

    /// Companion: a scanner emitting only `warn` lets the install
    /// through. Same fixture, different level — catches a
    /// regression where the fatal/warn branch wired up wrong.
    #[tokio::test]
    async fn end_to_end_passes_on_warn_only() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        write_bun_scanner(&scanner_path, "meh", "warn");

        let pkgs = vec![ScannerPackage {
            name: "meh".to_string(),
            version: "1.0.0".to_string(),
        }];
        assert!(
            run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
                .await
                .is_ok()
        );
    }

    /// A scanner module that doesn't resolve must surface as
    /// `WARN_AUBE_SECURITY_SCANNER_FAILED` and let the install
    /// proceed — a broken scanner shouldn't gate every install.
    /// `run_scanner` therefore returns `Ok(())` on resolve
    /// failure; the operator sees the warning via tracing.
    #[tokio::test]
    async fn missing_scanner_fails_open() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let pkgs = vec![ScannerPackage {
            name: "lodash".to_string(),
            version: "^4".to_string(),
        }];
        let result = run_scanner(
            "/definitely/not/a/real/path/to/a/scanner.mjs",
            std::path::Path::new("."),
            &pkgs,
        )
        .await;
        assert!(result.is_ok(), "fail-open contract broken: {result:?}");
    }

    /// Bun's docs specify the scanner returns an `Advisory[]`,
    /// but we also accept `{ advisories: [...] }` as a friendly
    /// fallback for scanners that wrap their result. This test
    /// uses a scanner that returns the wrapped shape to make
    /// sure the bridge's array-or-object handling stays wired.
    #[tokio::test]
    async fn accepts_wrapped_advisories_response() {
        if !node_available() {
            eprintln!("skipping: `node` not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let scanner_path = tmp.path().join("scanner.mjs");
        std::fs::write(
            &scanner_path,
            r#"export const scanner = {
  version: '1',
  async scan({ packages }) {
    return { advisories: packages.map(p => ({
      level: 'fatal',
      package: p.name,
      description: 'wrapped',
    })) };
  },
};
"#,
        )
        .unwrap();

        let pkgs = vec![ScannerPackage {
            name: "any".to_string(),
            version: "1".to_string(),
        }];
        let err = run_scanner(scanner_path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("wrapped"));
    }
}
