use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_STATE_DIR: &str = "node_modules";
const STATE_FILE_NAME: &str = ".aube-state";

/// Resolve the state directory for `project_dir`. Checks the `stateDir`
/// setting in `.npmrc` / env / workspace yaml; falls back to the
/// resolved `modulesDir` so the state file lives alongside the install
/// tree (and a `modulesDir` override doesn't accidentally create a
/// phantom `node_modules/` for the state file alone).
fn state_dir(project_dir: &Path) -> PathBuf {
    let default = || crate::commands::project_modules_dir(project_dir);
    crate::commands::with_settings_ctx(project_dir, |ctx| {
        let raw = aube_settings::resolved::state_dir(ctx);
        if raw == DEFAULT_STATE_DIR {
            default()
        } else {
            crate::commands::expand_setting_path(&raw, project_dir).unwrap_or_else(default)
        }
    })
}

fn state_file(project_dir: &Path) -> PathBuf {
    state_dir(project_dir).join(STATE_FILE_NAME)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstallState {
    pub lockfile_hash: String,
    pub package_json_hashes: BTreeMap<String, String>,
    pub aube_version: String,
    /// Whether this install omitted at least one dependency section (`--prod`
    /// or `--dev`). Used so `ensure_installed` can trigger a full re-install
    /// when a subsequent command needs deps that are not present.
    /// Pre-existing state files without this field deserialize as `false`.
    #[serde(default, rename = "prod")]
    pub section_filtered: bool,
}

/// Check if install is needed. Returns None if up-to-date, or Some(reason) if stale.
pub fn check_needs_install(project_dir: &Path) -> Option<String> {
    let state_path = state_file(project_dir);

    // No state file = never installed (or `rm -rf node_modules` wiped it).
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
    if !crate::commands::project_modules_dir(project_dir).exists() {
        return Some("node_modules is missing".into());
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

    // If the last install was section-filtered, part of the dependency graph
    // is missing even though the lockfile + manifest hashes match. Auto-install
    // the full graph to avoid silent "module not found" errors at runtime.
    if state.section_filtered {
        return Some(
            "previous install omitted dependency sections; auto-installing full graph".into(),
        );
    }

    // TODO: check workspace package.json hashes

    None
}

/// Write state file after a successful install. `section_filtered` should be
/// `true` when the install omitted dependency sections, so that
/// `check_needs_install` knows to trigger a full re-install before commands
/// that expect the whole graph.
pub fn write_state(project_dir: &Path, section_filtered: bool) -> Result<(), std::io::Error> {
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
    };

    let state_path = state_file(project_dir);
    if let Some(parent) = state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&state)?;
    std::fs::write(state_path, json)?;

    Ok(())
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

fn hash_file(path: &Path) -> String {
    let content = std::fs::read(path).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let hash = hasher.finalize();
    format!("sha256:{}", hex::encode(hash))
}
