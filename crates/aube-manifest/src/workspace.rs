use crate::UpdateConfig;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const WORKSPACE_YAML_NAMES: &[&str] = &["aube-workspace.yaml", "pnpm-workspace.yaml"];

fn find_and_read(project_dir: &Path) -> Result<Option<(PathBuf, String)>, crate::Error> {
    for name in WORKSPACE_YAML_NAMES {
        let path = project_dir.join(name);
        if path.exists() {
            let content =
                std::fs::read_to_string(&path).map_err(|e| crate::Error::Io(path.clone(), e))?;
            return Ok(Some((path, content)));
        }
    }
    Ok(None)
}

/// Extra privileges granted to one package pattern under `jailBuilds`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JailBuildPermission {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub network: bool,
}

/// Configuration from `pnpm-workspace.yaml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    /// Workspace package globs (e.g., `["packages/*"]`).
    #[serde(default)]
    pub packages: Vec<String>,

    /// Default catalog for dependency version pinning.
    #[serde(default)]
    pub catalog: BTreeMap<String, String>,

    /// Named catalogs for dependency version pinning.
    #[serde(default)]
    pub catalogs: BTreeMap<String, BTreeMap<String, String>>,

    // -- Node-Modules Settings --
    /// Linking strategy: "isolated" (default), "hoisted", or "pnp".
    #[serde(default)]
    pub node_linker: Option<String>,

    /// Whether to use the global virtual store (default: false in pnpm, true in aube).
    #[serde(default)]
    pub enable_global_virtual_store: Option<bool>,

    /// Package names whose presence in any importer forces
    /// per-project materialization (disabling the global virtual
    /// store for that install). Defaults to the common bundler /
    /// framework direct-devDeps whose module resolvers follow
    /// symlinks then walk up (Next.js's Turbopack, Vite, Rollup,
    /// Webpack, Parcel, Nuxt, VitePress) — the global virtual store
    /// makes `.aube/<pkg>` an absolute symlink that escapes the
    /// project's filesystem root, which those resolvers can't walk
    /// back from. Add more names as you discover tools with the same
    /// restriction; set to `[]` to disable the heuristic. Declared
    /// here so `settings.toml`'s workspaceYaml source stays in sync
    /// with the actual deserialize surface.
    #[serde(default)]
    pub disable_global_virtual_store_for_packages: Option<Vec<String>>,

    /// Package import method: "auto", "hardlink", "copy", "clone", "clone-or-copy".
    #[serde(default)]
    pub package_import_method: Option<String>,

    /// Path to the virtual store directory (default: "node_modules/.aube").
    #[serde(default)]
    pub virtual_store_dir: Option<String>,

    /// Top-level modules directory name (default: "node_modules").
    /// aube accepts the setting for parity but only honors the
    /// default value — see `settings.toml`'s `modulesDir` entry.
    #[serde(default)]
    pub modules_dir: Option<String>,

    /// Whether to shamefully hoist all packages to root node_modules.
    #[serde(default)]
    pub shamefully_hoist: Option<bool>,

    /// Master switch for the hidden modules directory at
    /// `node_modules/.aube/node_modules/`. Default true.
    #[serde(default)]
    pub hoist: Option<bool>,

    /// Patterns of packages to hoist.
    #[serde(default)]
    pub hoist_pattern: Option<Vec<String>>,

    /// Whether workspace packages get symlinked into each importer's
    /// `node_modules/`. Default true.
    #[serde(default)]
    pub hoist_workspace_packages: Option<bool>,

    /// When true, the linker skips a workspace package's per-importer
    /// `node_modules/<name>` symlink if the workspace root already
    /// links the same package at the same resolved version. Default
    /// false (pnpm parity).
    #[serde(default)]
    pub dedupe_direct_deps: Option<bool>,

    /// When true, `aube deploy` copies every file in the source
    /// workspace package into the target directory instead of
    /// applying pack's `files` / `.npmignore` filter. Default false
    /// (pnpm parity). Declared as a typed field so the settings-meta
    /// parity test can see the workspace-yaml key.
    #[serde(default)]
    pub deploy_all_files: Option<bool>,

    /// Patterns of packages to hoist to the root node_modules.
    #[serde(default)]
    pub public_hoist_pattern: Option<Vec<String>>,

    // -- Store Settings --
    /// Path to the content-addressable store.
    #[serde(default)]
    pub store_dir: Option<String>,

    // -- Lockfile Settings --
    /// Whether to use a lockfile (default: true).
    #[serde(default)]
    pub lockfile: Option<bool>,

    /// Whether to prefer frozen lockfile (default: true).
    #[serde(default)]
    pub prefer_frozen_lockfile: Option<bool>,

    /// Write a per-branch lockfile (`pnpm-lock.<branch>.yaml`) instead of
    /// the default `pnpm-lock.yaml`. Reduces merge conflicts on long-lived
    /// branches. Forward slashes in branch names are encoded as `!`.
    #[serde(default)]
    pub git_branch_lockfile: Option<bool>,

    /// Branch-name glob list that triggers an automatic branch-lockfile
    /// merge on matching branches. Companion to `gitBranchLockfile`.
    /// See `settings.toml` for the pattern syntax (including `!`-prefix
    /// negations). Declared as a typed field so the settings-meta parity
    /// test can see the workspace-yaml key.
    #[serde(default)]
    pub merge_git_branch_lockfiles_branch_pattern: Option<Vec<String>>,

    /// Cap on lockfile peer-ID suffix byte length before the resolver
    /// replaces the suffix with `_<sha256-hex>`. Default 1000 (pnpm
    /// parity). Same typed/raw duality as `child_concurrency` — see
    /// that field's comment.
    #[serde(default)]
    pub peers_suffix_max_length: Option<u64>,

    // -- Dependency Resolution --
    /// Override any dependency in the dependency graph.
    #[serde(default)]
    pub overrides: BTreeMap<String, String>,

    /// `name@version` → patch-file-path map. pnpm v10 moved this out
    /// of `package.json`'s `pnpm.patchedDependencies` so users can
    /// document *why* a patch exists with YAML comments; aube merges
    /// both locations, with workspace-yaml entries winning on key
    /// conflict (same precedence as `overrides`).
    #[serde(default, rename = "patchedDependencies")]
    pub patched_dependencies: BTreeMap<String, String>,

    /// os/cpu/libc widening set. pnpm v10 moved this alongside
    /// `overrides` — users generating a cross-platform lockfile on
    /// Linux CI want to widen in the workspace yaml (where the rest
    /// of their shared config lives) rather than `package.json`.
    /// Merged with `package.json`'s `pnpm.supportedArchitectures` /
    /// `aube.supportedArchitectures` at install time.
    #[serde(default, rename = "supportedArchitectures")]
    pub supported_architectures: Option<SupportedArchitectures>,

    /// Optional-dep names that should always be skipped, even when
    /// their platform matches. Merged with `package.json`'s
    /// `pnpm.ignoredOptionalDependencies` / `aube.*` at install time.
    /// Distinct from `--no-optional`, which drops *all* optional deps.
    #[serde(default, rename = "ignoredOptionalDependencies")]
    pub ignored_optional_dependencies: Vec<String>,

    /// Override for the pnpmfile path. pnpm lets users point at a
    /// non-default location; aube's default is `cwd/.pnpmfile.mjs`
    /// when present, otherwise `cwd/.pnpmfile.cjs`.
    /// Relative paths resolve against the workspace root.
    #[serde(default, rename = "pnpmfilePath")]
    pub pnpmfile_path: Option<String>,

    /// Extend package metadata during resolution.
    #[serde(default)]
    pub package_extensions: BTreeMap<String, yaml_serde::Value>,

    /// Package deprecation ranges whose warnings should be muted.
    #[serde(default)]
    pub allowed_deprecated_versions: BTreeMap<String, String>,

    /// Scope of install-time deprecation warnings: `none`, `direct`,
    /// `all`, or `summary`. Declared as a typed field so the
    /// settings-meta parity test sees the workspaceYaml key.
    #[serde(default)]
    pub deprecation_warnings: Option<String>,

    /// Update-time policy knobs.
    #[serde(default)]
    pub update_config: Option<UpdateConfig>,

    /// Trust-policy mode. Parsed for pnpm parity; resolver support is
    /// limited to accepting the configured policy surface until registry
    /// trust metadata is available.
    #[serde(default)]
    pub trust_policy: Option<String>,

    /// Packages exempt from trust-policy checks.
    #[serde(default)]
    pub trust_policy_exclude: Vec<String>,

    /// Ignore trust-policy checks for package versions older than this
    /// many minutes.
    #[serde(default)]
    pub trust_policy_ignore_after: Option<u64>,

    /// Reject transitive git/file/tarball dependency specs by default.
    #[serde(default)]
    pub block_exotic_subdeps: Option<bool>,

    // -- Build Settings --
    /// Whether to ignore all lifecycle scripts (default: false).
    #[serde(default)]
    pub ignore_scripts: Option<bool>,

    // -- aube-specific knobs --
    /// Skip the `aube run` / `aube exec` auto-install staleness check
    /// at the workspace level. Same semantics as the `aubeNoAutoInstall`
    /// setting resolved via `aube_settings::resolved`; both surfaces
    /// round-trip through `WorkspaceConfig` to keep the
    /// `workspace_yaml_keys_deserialize_onto_workspace_config` parity
    /// test happy. See `AUBE_NO_AUTO_INSTALL` env-var alias.
    #[serde(default)]
    pub aube_no_auto_install: Option<bool>,

    /// Bypass the project-level advisory lock on `node_modules/` for
    /// every mutating aube command in this workspace. Same semantics as
    /// the `aubeNoLock` setting resolved via `aube_settings::resolved`;
    /// useful for CI matrices or deliberately-parallel test rigs
    /// running from one shared workspace. See `AUBE_NO_LOCK` env-var
    /// alias.
    #[serde(default)]
    pub aube_no_lock: Option<bool>,

    /// Per-package allowlist for dependency lifecycle scripts. Keys are
    /// pnpm-style patterns (`name`, `name@version`, `name@v1 || v2`);
    /// values are `true` to allow or `false` to deny. Merged with
    /// `package.json`'s `pnpm.allowBuilds` — workspace-level entries
    /// take precedence for the same key.
    #[serde(default)]
    pub allow_builds: BTreeMap<String, yaml_serde::Value>,

    /// pnpm's canonical allowlist format: a flat list of package names
    /// whose lifecycle scripts are allowed to run. Merged with
    /// `allow_builds` into the same `BuildPolicy`. Workspace-level
    /// entries apply to every importer in the workspace.
    #[serde(default, rename = "onlyBuiltDependencies")]
    pub only_built_dependencies: Vec<String>,

    /// pnpm's canonical denylist: lifecycle scripts from these packages
    /// never run even if the allowlist includes them (explicit denies
    /// always win in `BuildPolicy::decide`).
    #[serde(default, rename = "neverBuiltDependencies")]
    pub never_built_dependencies: Vec<String>,

    /// Maximum number of dep lifecycle scripts running in parallel
    /// during the post-link `allowBuilds` phase. Defaults to 5 when
    /// unset. Mirrors pnpm's `childConcurrency` setting.
    ///
    /// The typed field isn't read directly by `install::run` —
    /// every int/string setting in aube has two faces: this struct
    /// field (for strict deserialization) and a
    /// `aube_settings::resolved::<name>` accessor that reads from the
    /// parallel raw YAML map in `ResolveCtx`. The duplication exists
    /// so the `meta::workspace_yaml_keys_deserialize_onto_workspace_config`
    /// test can catch settings.toml typos that would otherwise let a
    /// YAML key fall through to `extra` and be silently ignored. Same
    /// pattern as `minimum_release_age` / `auto_install_peers`.
    #[serde(default, rename = "childConcurrency")]
    pub child_concurrency: Option<u64>,

    /// Cap concurrent tarball downloads. When unset, aube uses its
    /// built-in defaults (128 for the lockfile path, 64 for the
    /// streaming path). Same typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "networkConcurrency")]
    pub network_concurrency: Option<u64>,

    /// Cap package materialization/linking worker count. When unset,
    /// aube uses platform-aware defaults in `aube-linker`. Same
    /// typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "linkConcurrency")]
    pub link_concurrency: Option<u64>,

    /// Whether to verify each tarball's SHA-512 against the lockfile
    /// integrity before importing into the store. Defaults to `true`
    /// (pnpm parity); `false` skips the check.
    #[serde(default, rename = "verifyStoreIntegrity")]
    pub verify_store_integrity: Option<bool>,

    /// Companion to `verifyStoreIntegrity`. When true, a missing
    /// `dist.integrity` on an imported packument is a hard error
    /// instead of a warning. Defaults to `false` for pnpm parity.
    #[serde(default, rename = "strictStoreIntegrity")]
    pub strict_store_integrity: Option<bool>,

    /// Cache post-build side effects for dependency packages.
    /// Accepted for pnpm-config parity but currently a no-op —
    /// aube skips dep lifecycle scripts by default.
    #[serde(default, rename = "sideEffectsCache")]
    pub side_effects_cache: Option<bool>,

    /// Run approved dependency lifecycle scripts in a restricted build
    /// jail. Same typed/raw duality as `child_concurrency`.
    #[serde(default, rename = "jailBuilds")]
    pub jail_builds: Option<bool>,

    /// Master switch that forces both `jailBuilds=true` and
    /// `trustPolicy=no-downgrade`. Same typed/raw duality as
    /// `child_concurrency`.
    #[serde(default)]
    pub paranoid: Option<bool>,

    /// Dependency package patterns that should run outside the jail even
    /// when `jailBuilds` is enabled. Same typed/raw duality as
    /// `child_concurrency`.
    #[serde(default, rename = "jailBuildExclusions")]
    pub jail_build_exclusions: Vec<String>,

    /// Extra env/path/network grants for packages that still run in the
    /// jail. Keys use the same package-pattern syntax as `allowBuilds`.
    #[serde(default, rename = "jailBuildPermissions")]
    pub jail_build_permissions: BTreeMap<String, JailBuildPermission>,

    // -- Catalog Settings --
    /// Drop catalog entries that no importer references after resolve.
    /// Wired through `aube_settings::resolved::cleanup_unused_catalogs`;
    /// the typed field exists only so `meta::workspace_yaml_keys_...`
    /// sees the key as a real field and doesn't fall through to `extra`.
    #[serde(default)]
    pub cleanup_unused_catalogs: Option<bool>,

    // -- Peer Dependency Settings --
    /// Whether to auto-install peer dependencies (default: true).
    #[serde(default)]
    pub auto_install_peers: Option<bool>,

    /// Fail the install if any required peer dependency is missing or
    /// resolves outside its declared range. Default: false (warn only).
    #[serde(default)]
    pub strict_peer_dependencies: Option<bool>,

    /// Omit `link:` dependencies from the lockfile's importer
    /// dependency maps on write. Default: false.
    #[serde(default)]
    pub exclude_links_from_lockfile: Option<bool>,

    /// Collapse peer-equivalent subtree variants into a single
    /// canonical dep_path (cross-subtree intersection). Default: true.
    #[serde(default)]
    pub dedupe_peer_dependents: Option<bool>,

    /// Emit peer suffixes as `(version)` instead of `(name@version)`
    /// in the lockfile. Default: false.
    #[serde(default)]
    pub dedupe_peers: Option<bool>,

    /// Consult the root workspace importer's direct deps for peer
    /// resolution before falling back to a graph-wide scan.
    /// Default: true.
    #[serde(default)]
    pub resolve_peers_from_workspace_root: Option<bool>,

    /// Record the full registry tarball URL on every registry-sourced
    /// package in the lockfile's `resolution.tarball:` field. Default:
    /// false. Round-trips through the lockfile `settings:` header so
    /// the value is preserved once set.
    #[serde(default)]
    pub lockfile_include_tarball_url: Option<bool>,

    // -- Supply Chain Settings --
    /// Minimum age in minutes that a package version must have before
    /// it's eligible for resolution. pnpm v11 default is 1440 (1 day).
    #[serde(default)]
    pub minimum_release_age: Option<u64>,

    /// Package names exempt from `minimum_release_age`.
    #[serde(default)]
    pub minimum_release_age_exclude: Option<Vec<String>>,

    /// When true, fail the install if no version satisfies a range
    /// without violating `minimum_release_age`. Default: false (fall back
    /// to the lowest satisfying version that ignores the cutoff).
    #[serde(default)]
    pub minimum_release_age_strict: Option<bool>,

    /// pnpm-style peer dependency escape hatches. Read by
    /// `PeerDependencyRules::resolve` during install; the actual matching
    /// logic lives in `aube`. We only need the container here so the
    /// settings-meta parity test can see the top-level key as a real
    /// field (not falling into `extra`). Leaves of the map are
    /// deserialized lazily on demand.
    #[serde(default)]
    pub peer_dependency_rules: Option<yaml_serde::Value>,

    /// Capture unknown fields for forward compatibility.
    #[serde(flatten)]
    pub extra: BTreeMap<String, yaml_serde::Value>,
}

/// `supportedArchitectures.{os,cpu,libc}` arrays from
/// pnpm-workspace.yaml. Same three-axis shape pnpm uses; each entry
/// can be a concrete token (`"linux"`) or the literal `"current"`,
/// which the resolver expands to the host triple.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SupportedArchitectures {
    #[serde(default)]
    pub os: Vec<String>,
    #[serde(default)]
    pub cpu: Vec<String>,
    #[serde(default)]
    pub libc: Vec<String>,
}

impl SupportedArchitectures {
    pub fn is_empty(&self) -> bool {
        self.os.is_empty() && self.cpu.is_empty() && self.libc.is_empty()
    }
}

impl WorkspaceConfig {
    /// Convert the raw `allow_builds` map to the same shape used for
    /// `package.json`'s `pnpm.allowBuilds`, so callers can merge both
    /// sources uniformly.
    pub fn allow_builds_raw(&self) -> BTreeMap<String, crate::AllowBuildRaw> {
        self.allow_builds
            .iter()
            .map(|(k, v)| {
                let raw = match v {
                    yaml_serde::Value::Bool(b) => crate::AllowBuildRaw::Bool(*b),
                    other => {
                        // Render via YAML serialization so the user sees
                        // the same text they wrote (`maybe`, `[a, b]`)
                        // rather than yaml_serde's Debug form
                        // (`String("maybe")`). Matches the JSON side in
                        // `AllowBuildRaw::from_json`.
                        let rendered = yaml_serde::to_string(other)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        crate::AllowBuildRaw::Other(rendered)
                    }
                };
                (k.clone(), raw)
            })
            .collect()
    }

    /// Load workspace config from `aube-workspace.yaml` (preferred) or
    /// `pnpm-workspace.yaml` (pnpm compatibility) in the given directory.
    /// Returns `Default` if neither file exists. If both exist, the aube
    /// file wins and the pnpm file is ignored.
    pub fn load(project_dir: &Path) -> Result<Self, crate::Error> {
        let Some((path, content)) = find_and_read(project_dir)? else {
            return Ok(Self::default());
        };
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        crate::parse_yaml(&path, content)
    }
}

/// Load the workspace yaml as a raw top-level key/value map, without
/// coercing into `WorkspaceConfig`'s typed fields. Intended for
/// metadata-driven setting resolution (see `aube-settings`), where
/// the caller walks a list of aliases from
/// `SettingMeta::workspace_yaml_keys` and pulls out whichever key is
/// present.
///
/// Returns an empty map if no file exists — same semantics as `load`.
/// File-precedence rules match `load`: `aube-workspace.yaml` wins
/// over `pnpm-workspace.yaml`.
// Process-wide memoization for the raw workspace-yaml map. Hot-path
// callers (`with_settings_ctx`, `aube_lock_filename`, `take_project_lock`,
// and the install-path `load_both` caller) all hit this with the same
// cwd. Same pattern as `aube_lockfile::aube_lock_filename`. Both
// `load_raw` and `load_both` populate + read this cache so a later
// `load_raw` after `load_both` doesn't re-read the file.
type RawCacheMap =
    std::collections::HashMap<std::path::PathBuf, BTreeMap<String, yaml_serde::Value>>;
static RAW_CACHE: std::sync::OnceLock<std::sync::Mutex<RawCacheMap>> = std::sync::OnceLock::new();

fn raw_cache_lookup(project_dir: &Path) -> Option<BTreeMap<String, yaml_serde::Value>> {
    let cache = RAW_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    cache.lock().ok()?.get(project_dir).cloned()
}

fn raw_cache_insert(project_dir: &Path, value: BTreeMap<String, yaml_serde::Value>) {
    let cache = RAW_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(mut map) = cache.lock() {
        map.insert(project_dir.to_path_buf(), value);
    }
}

pub fn load_raw(project_dir: &Path) -> Result<BTreeMap<String, yaml_serde::Value>, crate::Error> {
    if let Some(hit) = raw_cache_lookup(project_dir) {
        return Ok(hit);
    }
    let Some((path, content)) = find_and_read(project_dir)? else {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok(BTreeMap::new());
    };
    if content.trim().is_empty() {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok(BTreeMap::new());
    }
    let parsed: BTreeMap<String, yaml_serde::Value> = crate::parse_yaml(&path, content)?;
    raw_cache_insert(project_dir, parsed.clone());
    Ok(parsed)
}

/// Load the workspace yaml once and return both the typed
/// `WorkspaceConfig` view and the raw `BTreeMap` view, parsed from
/// the same file contents. Callers that need both (e.g. `install::run`,
/// which wants typed `allow_builds_raw()` *and* the raw map for
/// metadata-driven setting resolution) avoid the two-read hit this
/// way. Errors propagate instead of being silently swallowed.
#[allow(clippy::type_complexity)]
pub fn load_both(
    project_dir: &Path,
) -> Result<(WorkspaceConfig, BTreeMap<String, yaml_serde::Value>), crate::Error> {
    let Some((path, content)) = find_and_read(project_dir)? else {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok((WorkspaceConfig::default(), BTreeMap::new()));
    };
    if content.trim().is_empty() {
        raw_cache_insert(project_dir, BTreeMap::new());
        return Ok((WorkspaceConfig::default(), BTreeMap::new()));
    }
    let value: yaml_serde::Value = crate::parse_yaml(&path, content.clone())?;
    let typed: WorkspaceConfig = yaml_serde::from_value(value.clone())
        .map_err(|e| crate::Error::parse_yaml_err(&path, content.clone(), &e))?;
    let raw: BTreeMap<String, yaml_serde::Value> = yaml_serde::from_value(value)
        .map_err(|e| crate::Error::parse_yaml_err(&path, content, &e))?;
    raw_cache_insert(project_dir, raw.clone());
    Ok((typed, raw))
}

/// Resolve which workspace-yaml path `add_to_allow_builds` should
/// mutate in `project_dir`. Existing `aube-workspace.yaml` wins
/// over `pnpm-workspace.yaml`; when neither exists, create
/// `aube-workspace.yaml` — aube's own filename, parallel to the
/// `aube-lock.yaml` shape we use for the lockfile.
///
/// Background: aube reads both `aube-workspace.yaml` (preferred)
/// and `pnpm-workspace.yaml` (fallback) for backward compatibility
/// with pnpm-style repos that already ship the latter. The
/// generated default flips to the aube-prefixed name so a fresh
/// project's filesystem layout matches aube's overall naming
/// (`aube-lock.yaml`, `aube-workspace.yaml`) rather than mixing
/// vendor namespaces.
pub fn workspace_yaml_target(project_dir: &Path) -> std::path::PathBuf {
    for name in WORKSPACE_YAML_NAMES {
        let path = project_dir.join(name);
        if path.exists() {
            return path;
        }
    }
    project_dir.join("aube-workspace.yaml")
}

/// Merge `names` into the workspace's `allowBuilds` map.
///
/// Write-target precedence:
/// 1. `aube-workspace.yaml` if it exists — top-level `allowBuilds`.
/// 2. `pnpm-workspace.yaml` if it exists — top-level `allowBuilds`.
/// 3. Otherwise — create `aube-workspace.yaml` with top-level
///    `allowBuilds` (matches aube's `aube-lock.yaml` naming).
///
/// `allowed=true` is used by `aube approve-builds`; `allowed=false` is
/// used by install to seed unreviewed packages for later review. Returns
/// the file that was written.
pub fn add_to_allow_builds(
    project_dir: &Path,
    names: &[String],
    allowed: bool,
) -> Result<std::path::PathBuf, crate::Error> {
    let path = workspace_yaml_target(project_dir);
    write_to_yaml(&path, names, allowed)
}

/// Insert or replace a single `patchedDependencies` entry in the
/// workspace yaml at `path`. Creates the file (and the
/// `patchedDependencies` mapping) if needed. Returns the file that
/// was written.
pub fn upsert_workspace_patched_dependency(
    path: &Path,
    key: &str,
    rel_patch_path: &str,
) -> Result<std::path::PathBuf, crate::Error> {
    edit_patched_dependencies_yaml(path, |map| {
        map.insert(
            yaml_serde::Value::String(key.to_string()),
            yaml_serde::Value::String(rel_patch_path.to_string()),
        );
    })
}

/// Drop a `patchedDependencies` entry from the workspace yaml at
/// `path`. Returns `Ok(true)` when the entry existed (and the file was
/// rewritten), `Ok(false)` when the file was missing or had no such
/// entry. When the removal empties `patchedDependencies`, the key is
/// dropped from the document so we don't leave a `patchedDependencies: {}`
/// stub behind.
pub fn remove_workspace_patched_dependency(path: &Path, key: &str) -> Result<bool, crate::Error> {
    if !path.exists() {
        return Ok(false);
    }
    let mut existed = false;
    edit_patched_dependencies_yaml(path, |map| {
        existed = map.shift_remove(key).is_some();
    })?;
    Ok(existed)
}

fn edit_patched_dependencies_yaml<F>(path: &Path, f: F) -> Result<std::path::PathBuf, crate::Error>
where
    F: FnOnce(&mut yaml_serde::Mapping),
{
    use yaml_serde::{Mapping, Value};

    let mut doc: Value = if path.exists() {
        let content =
            std::fs::read_to_string(path).map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
        if content.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            crate::parse_yaml(path, content)?
        }
    } else {
        Value::Mapping(Mapping::new())
    };

    let map = doc.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "top-level yaml must be a mapping".to_string(),
        )
    })?;

    let pd_key = Value::String("patchedDependencies".to_string());
    let entry = map
        .entry(pd_key.clone())
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let pd_map = entry.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "`patchedDependencies` must be a mapping".to_string(),
        )
    })?;

    f(pd_map);

    if pd_map.is_empty() {
        map.shift_remove("patchedDependencies");
    }

    let raw = yaml_serde::to_string(&doc)
        .map_err(|e| crate::Error::YamlParse(path.to_path_buf(), e.to_string()))?;
    let indented = indent_block_sequences(&raw);
    aube_util::fs_atomic::atomic_write(path, indented.as_bytes())
        .map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
    Ok(path.to_path_buf())
}

fn write_to_yaml(
    path: &Path,
    names: &[String],
    allowed: bool,
) -> Result<std::path::PathBuf, crate::Error> {
    use yaml_serde::{Mapping, Value};

    let mut doc: Value = if path.exists() {
        let content =
            std::fs::read_to_string(path).map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
        if content.trim().is_empty() {
            Value::Mapping(Mapping::new())
        } else {
            crate::parse_yaml(path, content)?
        }
    } else {
        Value::Mapping(Mapping::new())
    };

    let map = doc.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "top-level yaml must be a mapping".to_string(),
        )
    })?;

    let key = Value::String("allowBuilds".to_string());
    let existing = map
        .entry(key.clone())
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let allow_builds = existing.as_mapping_mut().ok_or_else(|| {
        crate::Error::YamlParse(
            path.to_path_buf(),
            "`allowBuilds` must be a mapping".to_string(),
        )
    })?;

    for name in names {
        let key = Value::String(name.clone());
        if allowed {
            allow_builds.insert(key, Value::Bool(true));
        } else {
            allow_builds.entry(key).or_insert(Value::Bool(false));
        }
    }

    let raw = yaml_serde::to_string(&doc)
        .map_err(|e| crate::Error::YamlParse(path.to_path_buf(), e.to_string()))?;
    // yaml_serde emits block sequences flush-left (`- foo`) while pnpm's
    // canonical workspace yaml indents them by two (`  - foo`). Reindent
    // so the output matches what a human or pnpm would write. Safe because
    // yaml_serde's block style always starts sequence items at the parent's
    // column; bumping every sequence line by two is a consistent transform.
    let indented = indent_block_sequences(&raw);
    aube_util::fs_atomic::atomic_write(path, indented.as_bytes())
        .map_err(|e| crate::Error::Io(path.to_path_buf(), e))?;
    Ok(path.to_path_buf())
}

/// Bump every block-sequence item line (`- ...`) by two spaces. Leaves
/// already-indented lines and non-sequence lines alone. yaml_serde's
/// output uses a single indent step per nesting level, so this produces
/// the `parent:\n  - item` shape humans expect.
fn indent_block_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for line in input.split_inclusive('\n') {
        let stripped = line.trim_start_matches(' ');
        if stripped.starts_with("- ") || stripped == "-\n" || stripped == "-" {
            out.push_str("  ");
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_config() {
        let config: WorkspaceConfig = yaml_serde::from_str("{}").unwrap();
        assert!(config.packages.is_empty());
        assert!(config.enable_global_virtual_store.is_none());
    }

    #[test]
    fn test_packages_only() {
        let yaml = r#"
packages:
  - 'packages/*'
  - 'apps/*'
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.packages, vec!["packages/*", "apps/*"]);
    }

    #[test]
    fn test_settings() {
        let yaml = r#"
packages:
  - 'packages/*'
enableGlobalVirtualStore: true
shamefullyHoist: false
packageImportMethod: hardlink
storeDir: /tmp/my-store
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.packages, vec!["packages/*"]);
        assert_eq!(config.enable_global_virtual_store, Some(true));
        assert_eq!(config.shamefully_hoist, Some(false));
        assert_eq!(config.package_import_method, Some("hardlink".to_string()));
        assert_eq!(config.store_dir, Some("/tmp/my-store".to_string()));
    }

    #[test]
    fn test_catalog() {
        let yaml = r#"
catalog:
  chalk: ^4.1.2
  lodash: ^4.17.21
catalogs:
  react16:
    react: ^16.7.0
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.catalog.get("chalk").unwrap(), "^4.1.2");
        assert_eq!(
            config
                .catalogs
                .get("react16")
                .unwrap()
                .get("react")
                .unwrap(),
            "^16.7.0"
        );
    }

    #[test]
    fn test_overrides() {
        let yaml = r#"
overrides:
  foo: 1.0.0
  bar: npm:baz@^2
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.overrides.get("foo").unwrap(), "1.0.0");
        assert_eq!(config.overrides.get("bar").unwrap(), "npm:baz@^2");
    }

    #[test]
    fn test_supported_architectures() {
        let yaml = r#"
supportedArchitectures:
  os: ["current", "linux"]
  cpu: ["current", "x64"]
  libc: ["glibc"]
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        let sa = config.supported_architectures.as_ref().unwrap();
        assert_eq!(sa.os, vec!["current", "linux"]);
        assert_eq!(sa.cpu, vec!["current", "x64"]);
        assert_eq!(sa.libc, vec!["glibc"]);
        assert!(!sa.is_empty());
    }

    #[test]
    fn test_ignored_optional_dependencies() {
        let yaml = r#"
ignoredOptionalDependencies:
  - fsevents
  - dtrace-provider
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config.ignored_optional_dependencies,
            vec!["fsevents", "dtrace-provider"]
        );
    }

    #[test]
    fn test_pnpmfile_path() {
        let yaml = r#"
pnpmfilePath: config/pnpmfile.cjs
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(config.pnpmfile_path.as_deref(), Some("config/pnpmfile.cjs"));
    }

    #[test]
    fn test_patched_dependencies() {
        // pnpm v10 lets users declare patches in pnpm-workspace.yaml so
        // they can annotate each patch with YAML comments explaining
        // WHY the patch exists — something package.json's JSON syntax
        // can't host. Parse shape matches `pnpm.patchedDependencies`.
        let yaml = r#"
patchedDependencies:
  "is-positive@3.1.0": patches/is-positive@3.1.0.patch
  "@scope/pkg@1.0.0": patches/scope-pkg.patch
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config
                .patched_dependencies
                .get("is-positive@3.1.0")
                .unwrap(),
            "patches/is-positive@3.1.0.patch"
        );
        assert_eq!(
            config.patched_dependencies.get("@scope/pkg@1.0.0").unwrap(),
            "patches/scope-pkg.patch"
        );
    }

    #[test]
    fn test_load_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(config.packages.is_empty());
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'src/*'\nenableGlobalVirtualStore: false\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["src/*"]);
        assert_eq!(config.enable_global_virtual_store, Some(false));
    }

    #[test]
    fn aube_workspace_preferred_over_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aube-workspace.yaml"),
            "packages:\n  - 'aube/*'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["aube/*"]);
    }

    #[test]
    fn add_to_allow_builds_creates_aube_workspace_when_no_yaml() {
        // When no workspace yaml exists yet, aube creates
        // `aube-workspace.yaml` (parallels `aube-lock.yaml`).
        // Existing `pnpm-workspace.yaml` files are still read +
        // mutated in place — see
        // `add_to_allow_builds_writes_to_existing_pnpm_workspace`
        // for that branch.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"solo","version":"0.0.0"}"#,
        )
        .unwrap();
        let path = add_to_allow_builds(
            dir.path(),
            &["esbuild".to_string(), "sharp".to_string()],
            true,
        )
        .unwrap();
        assert_eq!(path, dir.path().join("aube-workspace.yaml"));
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(true))
        ));
    }

    #[test]
    fn add_to_allow_builds_writes_to_existing_pnpm_workspace() {
        // Pin the backward-compat behavior: a project that
        // already ships `pnpm-workspace.yaml` (e.g. migrated
        // from pnpm) keeps mutating the existing file in
        // place rather than spawning a parallel
        // `aube-workspace.yaml`. Without this, an `aube
        // approve-builds` run would silently fork the config
        // into two files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"solo","version":"0.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let path = add_to_allow_builds(dir.path(), &["esbuild".to_string()], true).unwrap();
        assert_eq!(path, dir.path().join("pnpm-workspace.yaml"));
        assert!(!dir.path().join("aube-workspace.yaml").exists());
    }

    #[test]
    fn add_to_allow_builds_flips_existing_workspace_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  esbuild: false\n",
        )
        .unwrap();
        add_to_allow_builds(
            dir.path(),
            &["sharp".to_string(), "esbuild".to_string()],
            true,
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(true))
        ));
    }

    #[test]
    fn add_to_allow_builds_writes_false_for_review() {
        let dir = tempfile::tempdir().unwrap();
        add_to_allow_builds(dir.path(), &["esbuild".to_string()], false).unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(false))
        ));
    }

    #[test]
    fn add_to_allow_builds_false_does_not_revoke_approved_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  esbuild: true\n",
        )
        .unwrap();
        add_to_allow_builds(
            dir.path(),
            &["sharp".to_string(), "esbuild".to_string()],
            false,
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(false))
        ));
    }

    #[test]
    fn add_to_allow_builds_appends_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\nallowBuilds:\n  esbuild: true\n",
        )
        .unwrap();
        add_to_allow_builds(
            dir.path(),
            &["sharp".to_string(), "esbuild".to_string()],
            true,
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["packages/*"]);
        assert!(matches!(
            config.allow_builds.get("esbuild"),
            Some(yaml_serde::Value::Bool(true))
        ));
        assert!(matches!(
            config.allow_builds.get("sharp"),
            Some(yaml_serde::Value::Bool(true))
        ));
        let on_disk = std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap();
        assert!(on_disk.contains("\n  esbuild: true"), "got:\n{on_disk}");
        assert!(on_disk.contains("\n  sharp: true"), "got:\n{on_disk}");
    }

    #[test]
    fn add_to_allow_builds_writes_to_aube_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("aube-workspace.yaml"),
            "packages:\n  - 'a/*'\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'p/*'\n",
        )
        .unwrap();
        let path = add_to_allow_builds(dir.path(), &["esbuild".to_string()], true).unwrap();
        assert_eq!(path, dir.path().join("aube-workspace.yaml"));
        let pnpm = std::fs::read_to_string(dir.path().join("pnpm-workspace.yaml")).unwrap();
        assert!(!pnpm.contains("allowBuilds"));
    }

    #[test]
    fn pnpm_workspace_used_when_aube_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pnpm/*'\n",
        )
        .unwrap();
        let config = WorkspaceConfig::load(dir.path()).unwrap();
        assert_eq!(config.packages, vec!["pnpm/*"]);
    }

    #[test]
    fn test_unknown_fields_captured() {
        let yaml = r#"
someNewField: true
anotherSetting: value
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert!(config.extra.contains_key("someNewField"));
    }

    #[test]
    fn update_config_deserializes_ignore_dependencies() {
        let yaml = r#"
updateConfig:
  ignoreDependencies:
    - is-odd
"#;
        let config: WorkspaceConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(
            config
                .update_config
                .as_ref()
                .map(|u| u.ignore_dependencies.as_slice()),
            Some(["is-odd".to_string()].as_slice())
        );
    }

    #[test]
    fn upsert_workspace_patched_dependency_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        upsert_workspace_patched_dependency(
            &path,
            "is-positive@3.1.0",
            "patches/is-positive@3.1.0.patch",
        )
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("patchedDependencies:"));
        assert!(written.contains("is-positive@3.1.0"));
        assert!(written.contains("patches/is-positive@3.1.0.patch"));
    }

    #[test]
    fn upsert_workspace_patched_dependency_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages:\n  - 'pkgs/*'\noverrides:\n  foo: 1.0.0\n").unwrap();
        upsert_workspace_patched_dependency(&path, "bar@2.0.0", "patches/bar@2.0.0.patch").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("packages:"));
        assert!(written.contains("- 'pkgs/*'") || written.contains("- pkgs/*"));
        assert!(written.contains("overrides:"));
        assert!(written.contains("foo:"));
        assert!(written.contains("patchedDependencies:"));
        assert!(written.contains("bar@2.0.0"));
    }

    #[test]
    fn remove_workspace_patched_dependency_drops_empty_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let removed = remove_workspace_patched_dependency(&path, "a@1.0.0").unwrap();
        assert!(removed);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("patchedDependencies"));
    }

    #[test]
    fn remove_workspace_patched_dependency_missing_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let removed = remove_workspace_patched_dependency(&path, "missing@9.9.9").unwrap();
        assert!(!removed);
    }
}
