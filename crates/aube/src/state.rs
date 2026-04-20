use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_STATE_DIR: &str = "node_modules";
const STATE_FILE_NAME: &str = ".aube-state";

/// Resolve the modules dir and state file path for `project_dir` in a
/// single settings-context load. `check_needs_install` and `write_state`
/// both need both values, and this is on the hot path for every
/// `aube run` / `exec` / `test` / `start` / `restart`.
///
/// The default `stateDir` falls back to the resolved `modulesDir` so the
/// state file lives alongside the install tree — otherwise a
/// `modulesDir` override would create a phantom `node_modules/`
/// directory just to hold the state file.
fn resolve_paths(project_dir: &Path) -> (PathBuf, PathBuf) {
    crate::commands::with_settings_ctx(project_dir, |ctx| {
        let modules_dir = project_dir.join(aube_settings::resolved::modules_dir(ctx));
        let raw_state = aube_settings::resolved::state_dir(ctx);
        let state_dir = if raw_state == DEFAULT_STATE_DIR {
            modules_dir.clone()
        } else {
            crate::commands::expand_setting_path(&raw_state, project_dir)
                .unwrap_or_else(|| modules_dir.clone())
        };
        let state_file = state_dir.join(STATE_FILE_NAME);
        (modules_dir, state_file)
    })
}

fn state_file(project_dir: &Path) -> PathBuf {
    resolve_paths(project_dir).1
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallState {
    pub lockfile_hash: String,
    pub package_json_hashes: BTreeMap<String, String>,
    pub aube_version: String,
    #[serde(default, rename = "prod")]
    pub section_filtered: bool,
    #[serde(default)]
    pub settings_hash: String,
    /// Per-package content fingerprints from the last install,
    /// keyed by dep_path. Drives delta installs. Next install diffs
    /// these against the new lockfile's hashes and only re-fetches
    /// and re-links the entries that moved. Missing or stale values
    /// cascade to a full install. Purely additive, never
    /// load-bearing. Empty on fresh state or pre-delta aube.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_content_hashes: BTreeMap<String, String>,
    /// LtHash accumulator digest (hex) over every package in the
    /// installed graph. Wide-add multiset hash from
    /// `commands::install::delta::LtHash`. Match on this digest
    /// proves graph equivalence in a 32-byte compare and skips the
    /// O(N) map walk. Missing field cascades to the full diff.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub graph_lthash: String,
    /// Per-package Merkle subtree fingerprints, keyed by dep_path.
    /// Lets the delta path skip packages whose subtree matches the
    /// stored value even when their leaf changed. Peer-dep rewrites
    /// shuffle metadata without moving installed content, that is
    /// the case this catches. Missing field cascades to the
    /// leaf-only diff.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub package_subtree_hashes: BTreeMap<String, String>,
}

/// Check if install is needed. Returns None if up-to-date, or Some(reason) if stale.
pub fn check_needs_install(project_dir: &Path) -> Option<String> {
    let (modules_dir, state_path) = resolve_paths(project_dir);

    // No state file = never installed (or `rm -rf <modulesDir>` wiped it).
    let state = match read_state(&state_path) {
        Some(s) => s,
        None => return Some("install state not found".into()),
    };

    // In the default config the state file lives inside `modulesDir` so
    // `rm -rf <modules>` wipes it. But `stateDir` can point elsewhere,
    // in which case the state survives a manual modules-dir nuke and
    // the hashes below would falsely report "up to date". Guard against
    // that explicitly — zero-dep projects still get a modules directory
    // (with `.bin/`) from install, so the directory check covers them.
    if !modules_dir.exists() {
        let name = modules_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("node_modules");
        return Some(format!("{name} is missing"));
    }

    // Check lockfile hash. Honor `gitBranchLockfile` so a branch-specific
    // lockfile is the freshness anchor when present, but fall back to the
    // base lockfile names so a freshly-enabled branch doesn't loop on
    // "no lockfile found" — see `active_lockfile` for the full resolution
    // order.
    let (lockfile_name, lockfile_path) = active_lockfile(project_dir);
    if let Some(path) = lockfile_path {
        let current_hash = hash_file(&path);
        if current_hash != state.lockfile_hash {
            return Some(format!("{lockfile_name} has changed"));
        }
    } else {
        return Some("no lockfile found".into());
    }

    // Check root package.json hash
    let pkg_path = project_dir.join("package.json");
    if pkg_path.exists() {
        let current_hash = hash_file(&pkg_path);
        let stored_hash = state.package_json_hashes.get(".");
        if stored_hash != Some(&current_hash) {
            return Some("package.json has changed".into());
        }
    }

    if state.section_filtered {
        return Some(
            "previous install omitted dependency sections; auto-installing full graph".into(),
        );
    }

    // no settings_hash check here. this path feeds ensure_installed
    // (aube run / exec / test). those commands do not care about
    // install-shape settings changing because the tree is still the tree
    // built by the last install. also skipping this check avoids the
    // asymmetry bug where `aube install --node-linker=hoisted` writes
    // hash with cli_flag set, then bare `aube run` reads without the
    // flag, mismatches, triggers spurious auto-install.
    None
}

/// Variant of [`check_needs_install`] that also checks `settings_hash`
/// with the caller's `cli_flags` bag. Use from `install::run`'s warm
/// path short circuit so `--node-linker=hoisted` and friends also feed
/// the hash. `ensure_installed` (from `aube run`) uses the plain
/// [`check_needs_install`] on purpose, see the note there.
pub fn check_needs_install_with_flags(
    project_dir: &Path,
    cli_flags: &[(String, String)],
) -> Option<String> {
    if let Some(reason) = check_needs_install(project_dir) {
        return Some(reason);
    }
    let state_path = resolve_paths(project_dir).1;
    let Some(state) = read_state(&state_path) else {
        return Some("install state not found".into());
    };
    let current_settings_hash = hash_settings(project_dir, cli_flags);
    if current_settings_hash != state.settings_hash {
        return Some(".npmrc or workspace config has changed".into());
    }
    None
}

/// Write state file after a successful install. `section_filtered` should be
/// `true` when the install omitted dependency sections, so that
/// `check_needs_install` knows to trigger a full re-install before commands
/// that expect the whole graph. `cli_flags` is the install's `opts.cli_flags`
/// bag — threaded through so the stored `settings_hash` reflects CLI overrides
/// (e.g. `--node-linker=hoisted`) that shaped the tree on disk.
pub fn write_state(
    project_dir: &Path,
    section_filtered: bool,
    cli_flags: &[(String, String)],
    package_content_hashes: BTreeMap<String, String>,
    graph_lthash: String,
    package_subtree_hashes: BTreeMap<String, String>,
) -> Result<(), std::io::Error> {
    let mut package_json_hashes = BTreeMap::new();

    let pkg_path = project_dir.join("package.json");
    if pkg_path.exists() {
        package_json_hashes.insert(".".to_string(), hash_file(&pkg_path));
    }

    // TODO: hash workspace package.json files

    let lockfile_hash = match active_lockfile(project_dir).1 {
        Some(path) => hash_file(&path),
        None => String::new(),
    };

    let state = InstallState {
        lockfile_hash,
        package_json_hashes,
        aube_version: env!("CARGO_PKG_VERSION").to_string(),
        section_filtered,
        settings_hash: hash_settings(project_dir, cli_flags),
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
    };

    let state_path = state_file(project_dir);
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&state)?;
    std::fs::write(state_path, json)?;

    Ok(())
}

/// Read per-package fingerprints from a project's state file.
/// Returns `None` on any failure path (file missing, malformed
/// JSON, pre-delta aube). Caller treats that as "no prior
/// fingerprints, full install". Never surfaces an error because
/// delta is additive. A miss just lands on the full-install path.
pub fn read_state_package_content_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_file(project_dir))?;
    if state.package_content_hashes.is_empty() {
        return None;
    }
    Some(state.package_content_hashes)
}

/// Read the LtHash accumulator digest the last install wrote, if
/// any. Empty string on fresh state or pre-lthash aube versions.
pub fn read_state_graph_lthash(project_dir: &Path) -> Option<String> {
    let state = read_state(&state_file(project_dir))?;
    if state.graph_lthash.is_empty() {
        return None;
    }
    Some(state.graph_lthash)
}

/// Read stored subtree hashes for delta installs that want to
/// prune at the subtree granularity rather than the leaf
/// granularity. Absent field cascades to the leaf diff path.
pub fn read_state_subtree_hashes(project_dir: &Path) -> Option<BTreeMap<String, String>> {
    let state = read_state(&state_file(project_dir))?;
    if state.package_subtree_hashes.is_empty() {
        return None;
    }
    Some(state.package_subtree_hashes)
}

/// Remove the install state file. Missing state is not an error.
pub fn remove_state(project_dir: &Path) -> Result<(), std::io::Error> {
    match std::fs::remove_file(state_file(project_dir)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// Pick the lockfile path that an install in `project_dir` will actually
/// read or write through, mirroring `aube_lockfile::lockfile_candidates`.
///
/// Order:
///   1. `aube-lock.<branch>.yaml` (only if `gitBranchLockfile` is on
///      and we resolve a branch — the preferred value).
///   2. `aube-lock.yaml` — the default base file. Critical for the
///      freshly-enabled-branch case: the branch file hasn't been
///      written yet, but the base file exists, and without this step
///      `check_needs_install` would fall through to pnpm lockfiles
///      (or to `None` on aube-lock projects) and loop on
///      every `aube run` / `aube exec`.
///   3. `pnpm-lock.<branch>.yaml` / `pnpm-lock.yaml`.
///
/// Returns the display name (for messages) plus the resolved path, if
/// any exists.
fn active_lockfile(project_dir: &Path) -> (String, Option<PathBuf>) {
    let preferred = aube_lockfile::aube_lock_filename(project_dir);
    let preferred_path = project_dir.join(&preferred);
    if preferred_path.exists() {
        return (preferred, Some(preferred_path));
    }
    // Freshly-enabled `gitBranchLockfile`: base file exists, branch
    // file does not. Pick up the base so we don't loop on every run.
    if preferred != "aube-lock.yaml" {
        let base = project_dir.join("aube-lock.yaml");
        if base.exists() {
            return ("aube-lock.yaml".to_string(), Some(base));
        }
    }
    // Preserve pnpm-lock.yaml (and its branch variant) as an active
    // lockfile when the project already uses it.
    let pnpm_preferred = preferred.replacen("aube-lock.", "pnpm-lock.", 1);
    if pnpm_preferred != preferred {
        let pnpm_branch = project_dir.join(&pnpm_preferred);
        if pnpm_branch.exists() {
            return (pnpm_preferred, Some(pnpm_branch));
        }
    }
    let pnpm_base = project_dir.join("pnpm-lock.yaml");
    if pnpm_base.exists() {
        return ("pnpm-lock.yaml".to_string(), Some(pnpm_base));
    }
    // Also track npm/yarn/bun lockfiles written by the format-preserving
    // install path, so `check_needs_install` doesn't loop on "no lockfile
    // found" for projects that use these formats.
    for name in [
        "bun.lock",
        "yarn.lock",
        "npm-shrinkwrap.json",
        "package-lock.json",
    ] {
        let path = project_dir.join(name);
        if path.exists() {
            return (name.to_string(), Some(path));
        }
    }
    (preferred, None)
}

fn read_state(path: &PathBuf) -> Option<InstallState> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn hash_settings(project_dir: &Path, cli_flags: &[(String, String)]) -> String {
    // hash resolved settings not raw file bytes. old byte hash tripped on
    // noop edits like `optimisticRepeatInstall=true` (same as default).
    // resolved values collapse defaults to identical hash. cli flags feed
    // through ctx so `--node-linker=hoisted` also shows up here.
    // workspace yaml bytes still hashed on top, covers map shaped settings
    // like catalog, overrides, packageExtensions, onlyBuiltDependencies
    // where any change means a real re-resolve.
    let npmrc = aube_registry::config::load_npmrc_entries(project_dir);
    let raw_workspace = aube_manifest::workspace::load_raw(project_dir).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &raw_workspace,
        env: &env,
        cli: cli_flags,
    };
    let mut hasher = blake3::Hasher::new();
    // node_linker, hoist family, modules_dir, import method. these shape
    // the tree on disk. flip any of them, linker needs to rebuild.
    let node_linker = aube_settings::resolved::node_linker(&ctx);
    hasher.update(b"node_linker=");
    hasher.update(format!("{node_linker:?}").as_bytes());
    hasher.update(b"\0");
    let hoist = aube_settings::resolved::hoist(&ctx);
    hasher.update(format!("hoist={hoist}\0").as_bytes());
    let shamefully_hoist = aube_settings::resolved::shamefully_hoist(&ctx);
    hasher.update(format!("shamefully_hoist={shamefully_hoist}\0").as_bytes());
    let hoist_pattern = aube_settings::resolved::hoist_pattern(&ctx);
    hasher.update(b"hoist_pattern=");
    for p in &hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let public_hoist_pattern = aube_settings::resolved::public_hoist_pattern(&ctx);
    hasher.update(b"public_hoist_pattern=");
    for p in &public_hoist_pattern {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    let modules_dir = aube_settings::resolved::modules_dir(&ctx);
    hasher.update(format!("modules_dir={modules_dir}\0").as_bytes());
    let package_import_method = aube_settings::resolved::package_import_method(&ctx);
    hasher.update(b"package_import_method=");
    hasher.update(format!("{package_import_method:?}").as_bytes());
    hasher.update(b"\0");
    // enable_global_virtual_store is Option<bool>. Debug format keeps
    // None/Some(true)/Some(false) distinct which matters because Some(false)
    // is user opt out while None is "follow default".
    let enable_gvs = aube_settings::resolved::enable_global_virtual_store(&ctx);
    hasher.update(b"enable_gvs=");
    hasher.update(format!("{enable_gvs:?}").as_bytes());
    hasher.update(b"\0");
    let lockfile_enabled = aube_settings::resolved::lockfile(&ctx);
    hasher.update(format!("lockfile={lockfile_enabled}\0").as_bytes());
    // additional tree shape settings. cover enable_modules_dir flip
    // (pnpm equivalent of --lockfile-only persistent), virtual_store_only,
    // hoist_workspace_packages, dedupe_direct_deps, symlink,
    // disable_global_virtual_store_for_packages. any of these flipping
    // means the tree shape needs rebuild.
    let enable_modules_dir = aube_settings::resolved::enable_modules_dir(&ctx);
    hasher.update(format!("enable_modules_dir={enable_modules_dir}\0").as_bytes());
    let virtual_store_only = aube_settings::resolved::virtual_store_only(&ctx);
    hasher.update(format!("virtual_store_only={virtual_store_only}\0").as_bytes());
    let hoist_workspace_packages = aube_settings::resolved::hoist_workspace_packages(&ctx);
    hasher.update(format!("hoist_workspace_packages={hoist_workspace_packages}\0").as_bytes());
    let dedupe_direct_deps = aube_settings::resolved::dedupe_direct_deps(&ctx);
    hasher.update(format!("dedupe_direct_deps={dedupe_direct_deps}\0").as_bytes());
    let symlink = aube_settings::resolved::symlink(&ctx);
    hasher.update(format!("symlink={symlink}\0").as_bytes());
    let disable_gvs_for_packages =
        aube_settings::resolved::disable_global_virtual_store_for_packages(&ctx);
    hasher.update(b"disable_gvs_for_packages=");
    for p in &disable_gvs_for_packages {
        hasher.update(p.as_bytes());
        hasher.update(b"\x1f");
    }
    hasher.update(b"\0");
    // map shaped workspace settings live in yaml. raw byte hash catches
    // catalog edits, overrides bumps, packageExtensions, allowBuilds list.
    // any of those mean re-resolve is needed, yaml bytes are the source.
    hasher.update(b"workspace_yaml=");
    for name in ["pnpm-workspace.yaml", "aube-workspace.yaml"] {
        let path = project_dir.join(name);
        hasher.update(name.as_bytes());
        hasher.update(b"\x1f");
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update(b"\x1e");
    }
    hasher.update(b"\0");
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn hash_file(path: &Path) -> String {
    // BLAKE3 is 3–5× faster than SHA-256 on the state-check hot path.
    // The `"blake3:"` prefix makes old `"sha256:"` state mismatch on
    // first run after upgrade, which correctly triggers a rebuild.
    let content = std::fs::read(path).unwrap_or_default();
    let hash = blake3::hash(&content);
    format!("blake3:{}", hash.to_hex())
}
