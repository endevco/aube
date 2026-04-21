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

fn relative_path_or_original(path: &Path, base: &Path) -> String {
    pathdiff::diff_paths(path, base)
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .replace('\\', "/")
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
    #[serde(default)]
    pub layout: Option<InstallLayoutState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallLayoutState {
    pub linker: InstallLayoutMode,
    pub direct_entries: BTreeMap<String, Vec<String>>,
    pub packages: BTreeMap<String, InstalledPackageState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallLayoutMode {
    Isolated,
    Hoisted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPackageState {
    pub name: String,
    pub version: String,
    pub package_json_path: String,
    #[serde(default)]
    pub package_json_hash: String,
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

    // Check root + workspace package.json hashes.
    for (rel, stored_hash) in &state.package_json_hashes {
        let path = if rel == "." {
            project_dir.join("package.json")
        } else {
            project_dir.join(rel)
        };
        if !path.exists() {
            return Some(format!("{rel} is missing"));
        }
        if hash_file(&path) != *stored_hash {
            return Some(if rel == "." {
                "package.json has changed".into()
            } else {
                format!("{rel} has changed")
            });
        }
    }

    if state.section_filtered {
        return Some(
            "previous install omitted dependency sections; auto-installing full graph".into(),
        );
    }

    if let Some(reason) = verify_install_layout(project_dir, &state) {
        return Some(reason);
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
pub struct WriteStateLayout<'a> {
    pub graph: &'a aube_lockfile::LockfileGraph,
    pub node_linker: aube_linker::NodeLinker,
    pub modules_dir_name: &'a str,
    pub aube_dir: &'a Path,
    pub virtual_store_dir_max_length: usize,
    pub placements: Option<&'a aube_linker::HoistedPlacements>,
}

pub fn write_state(
    project_dir: &Path,
    section_filtered: bool,
    cli_flags: &[(String, String)],
    package_content_hashes: BTreeMap<String, String>,
    graph_lthash: String,
    package_subtree_hashes: BTreeMap<String, String>,
    layout: WriteStateLayout<'_>,
) -> Result<(), std::io::Error> {
    let lockfile_hash = match active_lockfile(project_dir).1 {
        Some(path) => hash_file(&path),
        None => String::new(),
    };

    let state = InstallState {
        lockfile_hash,
        package_json_hashes: collect_package_json_hashes(project_dir),
        aube_version: env!("CARGO_PKG_VERSION").to_string(),
        section_filtered,
        settings_hash: hash_settings(project_dir, cli_flags),
        package_content_hashes,
        graph_lthash,
        package_subtree_hashes,
        layout: Some(InstallLayoutState::from_graph(
            project_dir,
            layout.graph,
            layout.node_linker,
            layout.modules_dir_name,
            layout.aube_dir,
            layout.virtual_store_dir_max_length,
            layout.placements,
        )),
    };

    let state_path = state_file(project_dir);
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&state)?;
    // Atomic write via tempfile + rename. Old `fs::write` truncated
    // the file in place, so Ctrl+C, AV quarantine, or a crash mid
    // `write_all` left the state file zero bytes or partial JSON.
    // Next install saw corrupt state, fell back to "install state
    // not found" and ran a full cold install. User wondered why
    // frozen fast path never kicked in. Rename on POSIX is atomic,
    // on Windows ReplaceFileW behaves similarly post Win10.
    let parent = state_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".aube-state-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    use std::io::Write as _;
    {
        let mut f = tmp.as_file();
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    // persist() does the atomic rename. On Windows it still succeeds
    // when the target exists via MoveFileEx semantics.
    tmp.persist(&state_path)
        .map_err(|e| std::io::Error::other(format!("persist state: {e}")))?;

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

impl InstallLayoutState {
    fn from_graph(
        project_dir: &Path,
        graph: &aube_lockfile::LockfileGraph,
        node_linker: aube_linker::NodeLinker,
        modules_dir_name: &str,
        aube_dir: &Path,
        virtual_store_dir_max_length: usize,
        placements: Option<&aube_linker::HoistedPlacements>,
    ) -> Self {
        let linker = match node_linker {
            aube_linker::NodeLinker::Isolated => InstallLayoutMode::Isolated,
            aube_linker::NodeLinker::Hoisted => InstallLayoutMode::Hoisted,
        };
        let mut direct_entries = BTreeMap::new();
        if let Some(deps) = graph.importers.get(".") {
            let mut entries = Vec::with_capacity(deps.len());
            for dep in deps {
                entries.push(project_dir.join(modules_dir_name).join(&dep.name));
            }
            direct_entries.insert(
                ".".to_string(),
                entries
                    .into_iter()
                    .map(|p| relative_path_or_original(&p, project_dir))
                    .collect(),
            );
        }

        let mut packages = BTreeMap::new();
        let direct_dep_paths: std::collections::BTreeSet<String> = graph
            .importers
            .get(".")
            .into_iter()
            .flat_map(|deps| deps.iter().map(|dep| dep.dep_path.clone()))
            .collect();
        for dep_path in direct_dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path) else {
                continue;
            };
            let package_json_path = match pkg.local_source.as_ref() {
                Some(aube_lockfile::LocalSource::Link(path)) => {
                    project_dir.join(path).join("package.json")
                }
                _ => crate::commands::install::materialized_pkg_dir(
                    aube_dir,
                    &dep_path,
                    &pkg.name,
                    virtual_store_dir_max_length,
                    placements,
                )
                .join("package.json"),
            };
            packages.insert(
                dep_path,
                InstalledPackageState {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    package_json_path: relative_path_or_original(&package_json_path, project_dir),
                    package_json_hash: hash_file_if_exists(&package_json_path).unwrap_or_default(),
                },
            );
        }

        Self {
            linker,
            direct_entries,
            packages,
        }
    }
}

fn verify_install_layout(project_dir: &Path, state: &InstallState) -> Option<String> {
    let layout = state.layout.as_ref()?;

    for entries in layout.direct_entries.values() {
        for rel in entries {
            let path = project_dir.join(rel);
            if !path.exists() {
                return Some(format!("installed entry missing: {rel}"));
            }
        }
    }

    for pkg in layout.packages.values() {
        let pkg_json_path = project_dir.join(&pkg.package_json_path);
        let current_hash = hash_file_if_exists(&pkg_json_path);
        if let Some(current_hash) = current_hash
            && !pkg.package_json_hash.is_empty()
            && pkg.package_json_hash != empty_blake3_hash()
            && current_hash == pkg.package_json_hash
        {
            continue;
        }
        let manifest = match read_installed_package_manifest(&pkg_json_path) {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                return Some(format!(
                    "installed package metadata missing: {}",
                    pkg.package_json_path
                ));
            }
            Err(_) => {
                return Some(format!(
                    "installed package metadata unreadable: {}",
                    pkg.package_json_path
                ));
            }
        };
        if manifest.name != pkg.name || manifest.version != pkg.version {
            return Some(format!(
                "installed package metadata changed: {}",
                pkg.package_json_path
            ));
        }
    }

    None
}

#[derive(Deserialize)]
struct InstalledManifest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

fn read_installed_package_manifest(
    path: &Path,
) -> Result<Option<InstalledManifest>, std::io::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let parsed = serde_json::from_str(&content)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(parsed))
}

fn collect_package_json_hashes(project_dir: &Path) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    let pkg_path = project_dir.join("package.json");
    if pkg_path.exists() {
        hashes.insert(".".to_string(), hash_file(&pkg_path));
    }
    if let Ok(workspaces) = aube_workspace::find_workspace_packages(project_dir) {
        for pkg_dir in workspaces {
            let pkg_json = pkg_dir.join("package.json");
            if !pkg_json.is_file() {
                continue;
            }
            hashes.insert(
                relative_path_or_original(&pkg_json, project_dir),
                hash_file(&pkg_json),
            );
        }
    }
    hashes
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
    // Raw `.npmrc` bytes. Resolved settings above only cover the
    // install-shape keys we read. A user swapping `registry=` or
    // `//host/:_authToken=` changes what tarballs we would fetch
    // but the resolved-values hash never noticed, so fast path
    // stayed green while the actual source of truth for deps
    // changed. Hashing raw bytes is coarse (comment edits
    // invalidate too) but correct.
    hasher.update(b"npmrc=");
    {
        let path = project_dir.join(".npmrc");
        hasher.update(b".npmrc\x1f");
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update(b"\x1e");
    }
    hasher.update(b"\0");
    // OS + arch + libc. Optional deps filter by these. Swap host
    // between runs (committed node_modules across machines, shared
    // CI cache volume, Rosetta switch) and the correct prebuilts
    // change. Old fast path did not notice and skipped the install,
    // node_modules had the wrong variant for the active host.
    hasher.update(b"host=");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"\x1f");
    // Piggyback on resolver's runtime libc probe. OS != linux
    // returns empty string, harmless but stable.
    hasher.update(aube_resolver::platform::host_triple().2.as_bytes());
    hasher.update(b"\0");
    // Patches dir. patch-commit and patch-remove touch patches in
    // `<project>/patches/` and `.aube-patches.json`. Old fast path
    // did not hash either. User edits a patch file, next install
    // says up-to-date, node_modules still has old patched content.
    hasher.update(b"patches=");
    let patches_sidecar = project_dir.join(".aube-patches.json");
    if let Ok(bytes) = std::fs::read(&patches_sidecar) {
        hasher.update(b".aube-patches.json\x1f");
        hasher.update(&bytes);
        hasher.update(b"\x1e");
    }
    let patches_dir = project_dir.join("patches");
    if let Ok(entries) = std::fs::read_dir(&patches_dir) {
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        // Sort so hash is deterministic across filesystems that
        // return dir entries in different order (ext4 vs tmpfs vs
        // NTFS).
        paths.sort();
        for p in paths {
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            hasher.update(name.as_bytes());
            hasher.update(b"\x1f");
            if let Ok(bytes) = std::fs::read(&p) {
                hasher.update(&bytes);
            }
            hasher.update(b"\x1e");
        }
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

fn hash_file_if_exists(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|content| {
        let hash = blake3::hash(&content);
        format!("blake3:{}", hash.to_hex())
    })
}

fn empty_blake3_hash() -> &'static str {
    "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
}

#[cfg(test)]
mod tests {
    use super::{
        InstallLayoutMode, InstallLayoutState, InstallState, InstalledPackageState,
        empty_blake3_hash, relative_path_or_original, verify_install_layout,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[test]
    fn relative_path_helper_keeps_original_path_when_diff_fails() {
        let original = Path::new("/tmp/aube-test/package.json");
        let base = Path::new("project/../project");

        assert_eq!(
            relative_path_or_original(original, base),
            original.to_string_lossy()
        );
    }

    #[test]
    fn verify_install_layout_treats_legacy_empty_hash_as_cache_miss() {
        let project_dir = temp_project_dir("legacy-empty-hash");
        let state = InstallState {
            lockfile_hash: String::new(),
            package_json_hashes: BTreeMap::new(),
            aube_version: String::new(),
            section_filtered: false,
            settings_hash: String::new(),
            package_content_hashes: BTreeMap::new(),
            graph_lthash: String::new(),
            package_subtree_hashes: BTreeMap::new(),
            layout: Some(InstallLayoutState {
                linker: InstallLayoutMode::Isolated,
                direct_entries: BTreeMap::new(),
                packages: BTreeMap::from([(
                    "is-odd@3.0.1".to_string(),
                    InstalledPackageState {
                        name: "is-odd".to_string(),
                        version: "3.0.1".to_string(),
                        package_json_path:
                            "node_modules/.aube/missing/node_modules/is-odd/package.json"
                                .to_string(),
                        package_json_hash: empty_blake3_hash().to_string(),
                    },
                )]),
            }),
        };

        assert_eq!(
            verify_install_layout(&project_dir, &state),
            Some(
                "installed package metadata missing: node_modules/.aube/missing/node_modules/is-odd/package.json"
                    .to_string()
            )
        );
    }

    fn temp_project_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aube-state-tests-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir should be creatable");
        dir
    }
}
