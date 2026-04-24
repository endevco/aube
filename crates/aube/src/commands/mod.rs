pub mod add;
pub mod approve_builds;
pub mod audit;
pub mod bin;
pub mod cache;
pub mod cat_file;
pub mod cat_index;
pub mod catalogs;
pub mod check;
pub mod ci;
pub mod clean;
pub mod completion;
pub mod config;
pub mod create;
pub mod dedupe;
pub mod deploy;
pub mod deprecate;
pub mod deprecations;
pub mod dist_tag;
pub mod dlx;
pub mod doctor;
pub mod exec;
pub mod fetch;
pub mod find_hash;
pub mod global;
pub mod ignored_builds;
pub mod import;
pub mod init;
pub mod inject;
pub mod install;
pub mod install_test;
pub mod licenses;
pub mod link;
pub mod list;
pub mod login;
pub mod logout;
pub mod npm_fallback;
pub mod npmrc;
pub mod outdated;
pub mod pack;
pub mod patch;
pub mod patch_commit;
pub mod patch_remove;
pub mod peers;
pub mod prune;
pub mod publish;
pub mod publish_provenance;
pub mod rebuild;
pub mod recursive;
pub mod remove;
pub mod restart;
pub mod root;
pub mod run;
pub mod sbom;
pub mod store;
pub mod undeprecate;
pub mod unlink;
pub mod unpublish;
pub mod update;
pub mod version;
pub mod view;
pub mod why;

use aube_registry::client::RegistryClient;
use aube_registry::config::NpmConfig;
use miette::{Context, IntoDiagnostic, miette};
use std::any::Any;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

/// Process-wide snapshot of the top-level `--frozen-lockfile` /
/// `--no-frozen-lockfile` / `--prefer-frozen-lockfile` flags. Set once
/// by `async_main` before any command runs so downstream helpers
/// (`ensure_installed`, chained `install::run` calls from
/// `add`/`remove`/`update`/…) can pick them up without plumbing a
/// context struct through every command signature.
static GLOBAL_FROZEN: OnceLock<Option<install::FrozenOverride>> = OnceLock::new();
static GLOBAL_VIRTUAL_STORE: OnceLock<install::GlobalVirtualStoreFlags> = OnceLock::new();
static SKIP_AUTO_INSTALL_ON_PM_MISMATCH: AtomicBool = AtomicBool::new(false);

/// Process-wide registry override from the top-level `--registry=<url>`
/// flag. Applied in `make_client` (and any direct `NpmConfig::load`
/// caller that funnels through `load_npm_config`) so a single flag
/// covers every registry touch point in one invocation.
static REGISTRY_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct GlobalOutputFlags {
    pub silent: bool,
}

static GLOBAL_OUTPUT: OnceLock<GlobalOutputFlags> = OnceLock::new();

pub(crate) fn set_registry_override(url: Option<String>) {
    *REGISTRY_OVERRIDE.write().expect("registry lock poisoned") =
        url.map(|u| aube_registry::config::normalize_registry_url_pub(&u));
}

pub(crate) fn set_skip_auto_install_on_package_manager_mismatch(skip: bool) {
    SKIP_AUTO_INSTALL_ON_PM_MISMATCH.store(skip, Ordering::Relaxed);
}

pub(crate) fn skip_auto_install_on_package_manager_mismatch() -> bool {
    SKIP_AUTO_INSTALL_ON_PM_MISMATCH.load(Ordering::Relaxed)
}

pub(crate) struct RegistryOverrideGuard {
    previous: Option<String>,
    changed: bool,
}

impl Drop for RegistryOverrideGuard {
    fn drop(&mut self) {
        if self.changed {
            *REGISTRY_OVERRIDE.write().expect("registry lock poisoned") = self.previous.take();
        }
    }
}

pub(crate) fn scoped_registry_override(url: Option<String>) -> RegistryOverrideGuard {
    let mut guard = REGISTRY_OVERRIDE.write().expect("registry lock poisoned");
    let previous = guard.clone();
    let changed = url.is_some();
    if let Some(u) = url {
        *guard = Some(aube_registry::config::normalize_registry_url_pub(&u));
    }
    RegistryOverrideGuard { previous, changed }
}

pub(crate) fn registry_override() -> Option<String> {
    REGISTRY_OVERRIDE
        .read()
        .expect("registry lock poisoned")
        .clone()
}

/// Load an `NpmConfig` for `dir` and then apply the process-wide
/// `--registry` override, if any. Use this from any command that
/// needs config but wants the CLI flag to win.
pub(crate) fn load_npm_config(dir: &std::path::Path) -> NpmConfig {
    let mut config = NpmConfig::load(dir);
    if let Some(url) = registry_override() {
        config.registry = url;
    }
    config
}

/// Record the global frozen-lockfile override snapshot. Called once per
/// process from `async_main`.
pub(crate) fn set_global_frozen_override(flags: Option<install::FrozenOverride>) {
    let _ = GLOBAL_FROZEN.set(flags);
}

pub(crate) fn set_global_virtual_store_flags(flags: install::GlobalVirtualStoreFlags) {
    let _ = GLOBAL_VIRTUAL_STORE.set(flags);
}

pub(crate) fn set_global_output_flags(flags: GlobalOutputFlags) {
    let _ = GLOBAL_OUTPUT.set(flags);
}

/// Read the recorded global frozen-lockfile override snapshot, or
/// `None` if none was set — e.g. in unit tests that bypass `async_main`.
pub(crate) fn global_frozen_override() -> Option<install::FrozenOverride> {
    GLOBAL_FROZEN.get().copied().unwrap_or_default()
}

pub(crate) fn global_virtual_store_flags() -> install::GlobalVirtualStoreFlags {
    GLOBAL_VIRTUAL_STORE.get().copied().unwrap_or_default()
}

pub(crate) fn global_output_flags() -> GlobalOutputFlags {
    GLOBAL_OUTPUT.get().copied().unwrap_or_default()
}

pub(crate) fn configure_script_settings(ctx: &aube_settings::ResolveCtx<'_>) {
    let node_options = aube_settings::resolved::node_options(ctx).and_then(non_empty_string);
    let script_shell = aube_settings::resolved::script_shell(ctx)
        .and_then(|s| non_empty_string(s).map(Into::into));
    let unsafe_perm = aube_settings::resolved::unsafe_perm(ctx);
    let shell_emulator = aube_settings::resolved::shell_emulator(ctx);
    aube_scripts::set_script_settings(aube_scripts::ScriptSettings {
        node_options,
        script_shell,
        unsafe_perm,
        shell_emulator,
    });
}

/// Load `.npmrc` + workspace settings for `cwd` and push them into the
/// process-wide script settings snapshot. Used by commands that run
/// lifecycle hooks (pack/publish/version) outside the install path,
/// which already does this via `configure_script_settings` directly.
pub(crate) fn configure_script_settings_for_cwd(cwd: &Path) -> miette::Result<()> {
    let npmrc_entries = aube_registry::config::load_npmrc_entries(cwd);
    let (_, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env: &env_snapshot,
        cli: &[],
    };
    configure_script_settings(&ctx);
    Ok(())
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub(crate) fn retarget_cwd(path: &Path) -> miette::Result<()> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().into_diagnostic()?.join(path)
    };
    std::env::set_current_dir(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chdir into {}", path.display()))?;
    crate::dirs::set_cwd(&path)?;
    Ok(())
}

/// Compute the `FrozenMode` a chained install (`add`, `remove`,
/// `update`, `ensure_installed`, …) should use, taking into account
/// the process-wide global `--frozen-lockfile` flags and falling back
/// to the given default when none was set on the command line.
pub(crate) fn chained_frozen_mode(default: install::FrozenMode) -> install::FrozenMode {
    match global_frozen_override() {
        Some(ovr) => install::FrozenMode::from_override(Some(ovr), None),
        None => default,
    }
}

pub(crate) fn ensure_registry_auth(
    client: &RegistryClient,
    registry_url: &str,
) -> miette::Result<()> {
    if client.has_resolved_auth_for(registry_url) {
        Ok(())
    } else {
        Err(miette!(
            "no auth token for {registry_url}. Run `aube login --registry {registry_url}` first."
        ))
    }
}

/// Process-wide guard: `true` while a project lock is held by this process.
/// Nested commands (e.g. `add` calling `install`) observe this and skip
/// re-acquiring so they don't deadlock against themselves.
static LOCK_HELD: AtomicBool = AtomicBool::new(false);

/// Whether the project-level advisory lock is disabled. Resolves the
/// `aubeNoLock` setting through the full cli > env > npmrc >
/// workspace.yaml chain so `.npmrc` and `aube-workspace.yaml` entries
/// participate alongside the canonical `AUBE_NO_LOCK` env var.
fn aube_no_lock_enabled(cwd: &std::path::Path) -> bool {
    with_settings_ctx(cwd, aube_settings::resolved::aube_no_lock)
}

/// Opaque guard holding a project-level advisory lock. Dropping it releases
/// the lock and clears the process-wide `LOCK_HELD` flag. Commands bind
/// this to a `_lock` variable at the top of `run` so the lock is held for
/// the duration of the command.
///
/// The `_inner` field holds an erased `fslock::LockFile` (via `dyn Any`)
/// so callers don't have to take a direct dep on `fslock` to name the
/// type — the lock is released on drop regardless.
pub(crate) struct ProjectLock {
    _inner: Option<Box<dyn Any + Send>>,
    owns_flag: bool,
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        if self.owns_flag {
            LOCK_HELD.store(false, Ordering::Release);
        }
    }
}

/// Take an advisory lock on the current project's `node_modules/`.
///
/// The lock is keyed off the canonical path of `node_modules` (hashed into
/// `$TMPDIR/fslock/`), so multiple `aube` invocations against the same
/// project — even via different relative paths or symlinks — serialize
/// correctly.
///
/// Returns a no-op guard when `AUBE_NO_LOCK` is active or when this
/// process already holds the project lock (re-entrant case for
/// `add` → `install`), so callers don't need to special-case.
pub(crate) fn take_project_lock(cwd: &std::path::Path) -> miette::Result<ProjectLock> {
    if aube_no_lock_enabled(cwd) {
        return Ok(ProjectLock {
            _inner: None,
            owns_flag: false,
        });
    }

    // Re-entrant: if this process already holds the lock (outer command
    // chained into an inner one like add → install), skip re-acquisition.
    if LOCK_HELD.load(Ordering::Acquire) {
        return Ok(ProjectLock {
            _inner: None,
            owns_flag: false,
        });
    }

    let nm_path = project_modules_dir(cwd);
    let lock = xx::fslock::FSLock::new(&nm_path)
        .with_callback(|_| {
            eprintln!("Waiting for another aube process to finish in this project...");
        })
        .lock()
        .map_err(|e| miette!("failed to acquire project lock: {e}"))?;

    // Only mark the flag as held AFTER the OS lock is in hand, so a nested
    // call can't observe `LOCK_HELD = true` and get a no-op guard before
    // this process actually owns the underlying advisory lock.
    LOCK_HELD.store(true, Ordering::Release);

    Ok(ProjectLock {
        _inner: Some(Box::new(lock)),
        owns_flag: true,
    })
}

/// Open the global content-addressable store, honoring a `storeDir`
/// override from `.npmrc` or `pnpm-workspace.yaml` in `cwd`. Falls
/// back to the aube-owned default under `$XDG_DATA_HOME/aube/store/`
/// (see [`aube_store::dirs::store_dir`] for exact resolution).
///
/// Path interpretation matches pnpm: a leading `~` expands to the
/// user's home directory; relative paths are resolved against `cwd`
/// (so each project sees a consistent store regardless of where the
/// command was invoked from). The CAS schema suffix `v1/files` is
/// appended to the user-supplied path so the on-disk layout is stable
/// across versions of aube and never collides with a pnpm store rooted
/// at the same path.
pub(crate) fn open_store(cwd: &std::path::Path) -> miette::Result<aube_store::Store> {
    if let Some(custom) = resolved_store_dir(cwd) {
        aube_store::Store::with_root(custom.join("v1").join("files"))
            .into_diagnostic()
            .wrap_err("failed to open store")
    } else {
        aube_store::Store::default_location()
            .into_diagnostic()
            .wrap_err("failed to open store")
    }
}

/// Resolve the configured `storeDir` for `cwd`, returning `None` if
/// no override is set or the value can't be parsed. Walks `.npmrc`
/// and `pnpm-workspace.yaml` via `aube_settings::resolved::store_dir`,
/// then expands `~` and makes relative paths absolute against `cwd`.
/// The returned path is the user-facing store root *without* the
/// `v3/files` schema suffix — callers append it where needed (see
/// [`open_store`]).
fn resolved_store_dir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::store_dir(ctx)?;
        expand_setting_path(&raw, cwd)
    })
}

/// Expand a path-typed setting value. `~` -> home dir, relative ->
/// absolute against `cwd`. Returns None if the value begins with `~`
/// but no home env var is set, caller then falls back to a platform
/// default. On Unix reads HOME. On Windows reads HOME first (for
/// POSIX-compat toolchains that set it) then USERPROFILE (native
/// Windows default). Old code only checked HOME, Windows users got
/// silent None back for any `~/...` settings like `storeDir: ~/store`,
/// and the caller fell through to the platform default, so custom
/// store paths never took effect on Windows.
pub(crate) fn expand_setting_path(raw: &str, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        std::path::PathBuf::from(home_dir_os()?).join(rest)
    } else if raw == "~" {
        std::path::PathBuf::from(home_dir_os()?)
    } else {
        std::path::PathBuf::from(raw)
    };
    Some(if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    })
}

fn home_dir_os() -> Option<std::ffi::OsString> {
    aube_util::env::home_dir().map(|p| p.into_os_string())
}

/// Build a file-only `ResolveCtx` for `cwd` and call `f` with it.
/// Handles the temporary ownership of npmrc/workspace/env data so
/// callers don't need to import `serde_yaml`.
pub(crate) fn with_settings_ctx<T>(
    cwd: &std::path::Path,
    f: impl FnOnce(&aube_settings::ResolveCtx<'_>) -> T,
) -> T {
    let npmrc = aube_registry::config::load_npmrc_entries(cwd);
    let raw_workspace = aube_manifest::workspace::load_raw(cwd).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &raw_workspace,
        env: &env,
        cli: &[],
    };
    f(&ctx)
}

/// Build a registry client configured from .npmrc files in the project directory.
///
/// Also resolves the `fetch*` settings (timeout + retries + backoff)
/// from the full cli > env > npmrc > workspace precedence chain and
/// threads the resulting [`aube_registry::config::FetchPolicy`] into
/// the client. `.npmrc` is the canonical source for these today, but
/// going through the settings resolver means env-var overrides like
/// `NPM_CONFIG_FETCH_TIMEOUT` and future CLI flags Just Work without
/// touching this function again.
pub(crate) fn make_client(cwd: &std::path::Path) -> aube_registry::client::RegistryClient {
    let config = load_npm_config(cwd);
    tracing::debug!("registry: {}", config.registry);
    for (scope, url) in &config.scoped_registries {
        tracing::debug!("scoped registry: {scope} -> {url}");
    }
    let policy = resolve_fetch_policy(cwd);
    aube_registry::client::RegistryClient::from_config_with_policy(config, policy)
}

/// Build the standard resolver used by add/remove/update/dedupe: a
/// shared `RegistryClient` wrapped in `Arc`, the shared packument
/// cache directory, the given catalog map, and the dependency policy
/// resolved from `.npmrc` / workspace yaml. Without applying the
/// policy here `blockExoticSubdeps=false` (and other policy bits)
/// gets honored by `aube install` but ignored by `aube add` — the
/// install pipeline runs `configure_resolver` while these commands
/// re-resolve manifests through this stripped-down builder.
pub(crate) fn build_resolver(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    catalogs: CatalogMap,
) -> aube_resolver::Resolver {
    let policy = with_settings_ctx(cwd, |ctx| install::resolve_dependency_policy(manifest, ctx));
    aube_resolver::Resolver::new(std::sync::Arc::new(make_client(cwd)))
        .with_packument_cache(packument_cache_dir())
        .with_catalogs(catalogs)
        .with_dependency_policy(policy)
}

/// Resolve [`aube_registry::config::FetchPolicy`] from the same
/// sources the rest of the CLI consumes settings from. Kept separate
/// from [`make_client`] so tests and ad-hoc callers (publish,
/// deprecate, etc) can opt in without duplicating the ctx-building
/// boilerplate.
pub(crate) fn resolve_fetch_policy(cwd: &std::path::Path) -> aube_registry::config::FetchPolicy {
    let npmrc = aube_registry::config::load_npmrc_entries(cwd);
    let workspace_yaml = aube_manifest::workspace::load_both(cwd)
        .map(|(_, raw)| raw)
        .unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &workspace_yaml,
        env: &env,
        cli: &[],
    };
    aube_registry::config::FetchPolicy::from_ctx(&ctx)
}

/// Resolve the `cacheDir` setting for `cwd`. If an explicit override
/// is set in `.npmrc`, expands it and returns that path. Otherwise
/// falls back to the XDG-aware platform default (`~/.cache/aube`).
///
/// Note: `XDG_CACHE_HOME` is intentionally *not* a source for this
/// setting — it's a base directory, and `aube_store::dirs::cache_dir()`
/// already appends `/aube`. Routing it through the settings accessor
/// would lose the subdirectory.
pub(crate) fn resolved_cache_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    let platform_default =
        || aube_store::dirs::cache_dir().unwrap_or_else(|| std::env::temp_dir().join("aube"));
    // Check whether .npmrc explicitly sets cacheDir, rather than comparing
    // the resolved value against the default string — a user who writes
    // `cacheDir=~/.cache/aube` explicitly should get that literal path,
    // not the XDG_CACHE_HOME-aware platform default.
    let npmrc = aube_registry::config::load_npmrc_entries(cwd);
    let has_explicit = npmrc
        .iter()
        .any(|(k, _)| k == "cacheDir" || k == "cache-dir");
    if !has_explicit {
        return platform_default();
    }
    with_settings_ctx(cwd, |ctx| {
        let raw = aube_settings::resolved::cache_dir(ctx);
        expand_setting_path(&raw, cwd).unwrap_or_else(platform_default)
    })
}

/// Resolve the `virtualStoreDirMaxLength` setting, falling back to the
/// platform default (`DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH`, which is
/// 120 on Linux/macOS and will become 60 on Windows once Windows
/// support lands). Every call site that encodes `dep_path`s into
/// `.aube/<name>` filenames — install, list, why, patch, rebuild,
/// engines check — must resolve the same cap, otherwise the long-path
/// truncate-and-hash branch of `dep_path_to_filename` produces
/// different filenames for read-side and write-side callers and
/// silently misses packages.
pub(crate) fn resolve_virtual_store_dir_max_length(ctx: &aube_settings::ResolveCtx<'_>) -> usize {
    aube_settings::resolved::virtual_store_dir_max_length(ctx)
        .map(|v| v as usize)
        .unwrap_or(aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH)
}

/// Load `.npmrc` + `pnpm-workspace.yaml` for `cwd` and resolve the
/// effective `virtualStoreDirMaxLength` in one call. Convenience for
/// post-install commands (list, why, patch) that don't build a
/// `ResolveCtx` for any other reason.
pub(crate) fn resolve_virtual_store_dir_max_length_for_cwd(cwd: &std::path::Path) -> usize {
    with_settings_ctx(cwd, resolve_virtual_store_dir_max_length)
}

/// Project-level `node_modules` directory name (pnpm's `modulesDir`
/// setting). Defaults to `"node_modules"` — users who change it are
/// responsible for setting `NODE_PATH` themselves since Node's own
/// resolver still looks for a literal `node_modules/`.
///
/// Every command that touches the top-level project directory (bin,
/// root, prune, clean, link, unlink, run, exec, etc.) reads this so
/// it lands on the same path the install wrote to. Commands that
/// already build a `ResolveCtx` for other settings should call
/// `aube_settings::resolved::modules_dir(&ctx)` directly instead of
/// this shortcut.
pub(crate) fn resolve_modules_dir_name_for_cwd(cwd: &std::path::Path) -> String {
    with_settings_ctx(cwd, aube_settings::resolved::modules_dir)
}

/// Convenience: `<cwd>/<modulesDir>` as a `PathBuf`. Matches the
/// `project_dir.join("node_modules")` pattern that every command used
/// before `modulesDir` was wired; prefer this over the raw literal
/// so a workspace-level override flows through automatically.
pub(crate) fn project_modules_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(resolve_modules_dir_name_for_cwd(cwd))
}

/// Resolve the absolute path of the per-project virtual store
/// (pnpm's `virtualStoreDir`). When the user explicitly sets the value
/// in `.npmrc`, `pnpm-workspace.yaml`, or the environment, expand it
/// (relative paths resolve against `project_dir`, `~` expands to
/// `$HOME`) and return it. Otherwise derive from `modulesDir`:
/// `<project_dir>/<modulesDir>/.aube`. This matches pnpm, where the
/// documented default is `<modulesDir>/.pnpm` — a user who overrides
/// `modulesDir` alone keeps a coherent layout without having to set
/// both.
///
/// Every site that touches `.aube/<dep_path>/` — linker, install state
/// sidecar, `patch`, `rebuild`, `list --long`, `why`, `prune`, `clean`,
/// etc. — must resolve through this helper so a workspace-level
/// override lands at the same path the install wrote to.
pub(crate) fn resolve_virtual_store_dir(
    ctx: &aube_settings::ResolveCtx<'_>,
    project_dir: &std::path::Path,
) -> std::path::PathBuf {
    let default_from_modules_dir = || {
        let modules_dir = aube_settings::resolved::modules_dir(ctx);
        project_dir.join(modules_dir).join(".aube")
    };
    let has_explicit_npmrc = ctx
        .npmrc
        .iter()
        .any(|(k, _)| k == "virtualStoreDir" || k == "virtual-store-dir");
    let has_explicit_yaml = ctx.workspace_yaml.contains_key("virtualStoreDir");
    let has_explicit_env = ctx
        .env
        .iter()
        .any(|(k, _)| k == "npm_config_virtual_store_dir" || k == "NPM_CONFIG_VIRTUAL_STORE_DIR");
    if !(has_explicit_npmrc || has_explicit_yaml || has_explicit_env) {
        return default_from_modules_dir();
    }
    let raw = aube_settings::resolved::virtual_store_dir(ctx);
    expand_setting_path(&raw, project_dir).unwrap_or_else(default_from_modules_dir)
}

/// Load `.npmrc` + `pnpm-workspace.yaml` for `cwd` and resolve the
/// effective virtual-store path in one call. Convenience for
/// post-install commands (`patch`, `list --long`, `why`, `clean`,
/// `unlink`) that don't build a `ResolveCtx` for any other reason.
pub(crate) fn resolve_virtual_store_dir_for_cwd(cwd: &std::path::Path) -> std::path::PathBuf {
    with_settings_ctx(cwd, |ctx| resolve_virtual_store_dir(ctx, cwd))
}

/// Format the resolved `virtualStoreDir` as a display-ready prefix for
/// `aube list --long` and `aube why --long`, ending with a path
/// separator so callers can concatenate an encoded `dep_path`
/// filename. When `aube_dir` is a subdirectory of `ref_dir` the result
/// is relative (`./node_modules/.aube/`), matching the historical
/// output. For overrides that sit above or outside `ref_dir` (custom
/// `virtualStoreDir` like `~/.my-store/project` or `.vstore-out`) the
/// absolute path is returned so users can still find where packages
/// actually live — `../../../...` would be technically correct but
/// hard to paste into a shell.
pub(crate) fn format_virtual_store_display_prefix(
    aube_dir: &std::path::Path,
    ref_dir: &std::path::Path,
) -> String {
    if let Some(rel) = pathdiff::diff_paths(aube_dir, ref_dir)
        && !rel.as_os_str().is_empty()
        && !rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return format!("./{}/", rel.display());
    }
    format!("{}/", aube_dir.display())
}

/// Disk cache directory for packument metadata. Falls back to a tmp dir if
/// the user cache dir can't be resolved (rare).
pub(crate) fn packument_cache_dir() -> std::path::PathBuf {
    let cwd = crate::dirs::cwd().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    resolved_cache_dir(&cwd).join("packuments-v1")
}

/// Disk cache directory for *full* (non-corgi) packument JSON used by
/// human-facing commands like `aube view`. Separate from the corgi cache
/// because the shapes differ.
pub(crate) fn packument_full_cache_dir() -> std::path::PathBuf {
    let cwd = crate::dirs::cwd().unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    resolved_cache_dir(&cwd).join("packuments-full-v1")
}

/// Type alias for the catalog map the resolver consumes — outer key is
/// the catalog name (`default` for the unnamed catalog), inner map goes
/// from package name to version range.
pub(crate) type CatalogMap =
    std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

/// Merge `default_cat` / `named_cats` into `out`. Later calls overwrite
/// earlier entries — callers invoke this in ascending precedence order
/// so the highest-priority source lands last.
fn merge_catalog_source(
    out: &mut CatalogMap,
    default_cat: &std::collections::BTreeMap<String, String>,
    named_cats: &CatalogMap,
) {
    if !default_cat.is_empty() {
        let entry = out.entry("default".to_string()).or_default();
        for (k, v) in default_cat {
            entry.insert(k.clone(), v.clone());
        }
    }
    for (name, entries) in named_cats {
        let bucket = out.entry(name.clone()).or_default();
        for (k, v) in entries {
            bucket.insert(k.clone(), v.clone());
        }
    }
}

/// Pull the bun-style `workspaces.catalog` / `workspaces.catalogs` and
/// pnpm-style `pnpm.catalog` / `pnpm.catalogs` out of a single
/// package.json and merge them into `out`. Precedence within one
/// manifest: `pnpm.*` wins over `workspaces.*`.
fn merge_manifest_catalogs(out: &mut CatalogMap, manifest: &aube_manifest::PackageJson) {
    if let Some(ws) = &manifest.workspaces {
        merge_catalog_source(out, ws.catalog(), ws.catalogs());
    }
    merge_catalog_source(out, &manifest.pnpm_catalog(), &manifest.pnpm_catalogs());
}

/// Discover catalog entries from every supported source and merge them
/// into a single map for the resolver.
///
/// Sources, in ascending precedence (later overrides earlier on a per-
/// entry basis):
/// 1. `workspaces.catalog` / `workspaces.catalogs` in the project-root
///    `package.json` (bun style).
/// 2. `pnpm.catalog` / `pnpm.catalogs` in the project-root `package.json`.
/// 3. Same two fields from the workspace-root `package.json` when it's
///    a different file (monorepo subpackage installs). The workspace
///    root is the nearest ancestor with either a `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` or a `package.json` carrying a `workspaces`
///    field — bun / npm / yarn projects use the latter and have no yaml.
/// 4. `catalog:` / `catalogs:` in the nearest `pnpm-workspace.yaml` /
///    `aube-workspace.yaml` walking up from `project_root`.
///
/// Walking up matters for monorepos where `aube install` runs from a
/// subpackage — without it, the loader only looks at `project_root`
/// and misses the root workspace's catalogs entirely.
///
/// Every command that builds a `Resolver` threads this map through
/// `Resolver::with_catalogs`; otherwise the resolver hard-fails any
/// `catalog:` dep with `UnknownCatalog(Entry)`.
pub(crate) fn discover_catalogs(project_root: &std::path::Path) -> miette::Result<CatalogMap> {
    use miette::{Context, IntoDiagnostic};

    let mut out = CatalogMap::new();

    // (1)+(2): project-root package.json catalogs.
    let project_manifest_path = project_root.join("package.json");
    let project_manifest = aube_manifest::PackageJson::from_path(&project_manifest_path).ok();
    if let Some(m) = &project_manifest {
        merge_manifest_catalogs(&mut out, m);
    }

    // (3): workspace-root package.json catalogs, if the workspace root
    // sits above the project root. We resolve the workspace root from
    // either marker — yaml first (pnpm convention), then `workspaces`
    // field (bun / npm / yarn convention) — so a subpackage install in
    // a non-pnpm monorepo still picks up the root catalog.
    let workspace_yaml_dir = crate::dirs::find_workspace_yaml_root(project_root);
    let workspace_root_dir = crate::dirs::find_workspace_root(project_root);
    if let Some(dir) = &workspace_root_dir
        && dir != project_root
        && let Ok(m) = aube_manifest::PackageJson::from_path(&dir.join("package.json"))
    {
        merge_manifest_catalogs(&mut out, &m);
    }

    // (4): workspace yaml catalogs, highest precedence. Loaded from the
    // walk-up directory when present, else from `project_root`.
    let yaml_dir = workspace_yaml_dir.as_deref().unwrap_or(project_root);
    let (ws_config, _raw) = aube_manifest::workspace::load_both(yaml_dir)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    merge_catalog_source(&mut out, &ws_config.catalog, &ws_config.catalogs);

    out.retain(|_, v| !v.is_empty());
    Ok(out)
}

/// Convenience alias preserved for existing call sites; forwards to
/// [`discover_catalogs`] so every command sees the same merged view.
pub(crate) fn load_workspace_catalogs(cwd: &std::path::Path) -> miette::Result<CatalogMap> {
    discover_catalogs(cwd)
}

/// Read and parse `package.json` at `manifest_path` with the standard
/// miette-wrapped error message used across commands.
pub(crate) fn load_manifest(manifest_path: &Path) -> miette::Result<aube_manifest::PackageJson> {
    aube_manifest::PackageJson::from_path(manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

/// Serialize `value` as pretty JSON with a trailing newline and
/// atomically write it to `path`. Wraps the serialize + atomic-write
/// pair used by add/remove/update/audit when mutating `package.json`.
pub(crate) fn write_manifest_json<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> miette::Result<()> {
    let json = serde_json::to_string_pretty(value)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(path, format!("{json}\n").as_bytes())
        .wrap_err("failed to write package.json")
}

/// Atomic write for `package.json` (and any sibling JSON we care
/// about): write to a tempfile in the same directory then rename.
/// The old `fs::write` truncates in place and a crash mid-write left
/// users with an empty manifest — the worst aube failure mode.
pub(crate) fn write_manifest_atomic(path: &Path, body: &[u8]) -> miette::Result<()> {
    aube_util::fs_atomic::atomic_write(path, body)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", path.display()))
}

/// Parse the project lockfile, mapping `NotFound` to a user-facing hint
/// that includes `context` (e.g. `"aube audit"`).
pub(crate) fn load_graph(
    project_dir: &Path,
    manifest: &aube_manifest::PackageJson,
    missing_hint: &str,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    match aube_lockfile::parse_lockfile(project_dir, manifest) {
        Ok(g) => Ok(g),
        Err(aube_lockfile::Error::NotFound(_)) => Err(miette!("{missing_hint}")),
        Err(e) => Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    }
}

/// Collect the transitive dep-path closure reachable from the filtered
/// root deps, keyed by dep_path for stable iteration. Used by audit,
/// sbom, and anything else that needs "which packages would apply if
/// the user ran install in this mode".
pub(crate) fn collect_dep_closure(
    graph: &aube_lockfile::LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
) -> std::collections::BTreeMap<String, &aube_lockfile::LockedPackage> {
    let mut out: std::collections::BTreeMap<String, &aube_lockfile::LockedPackage> =
        std::collections::BTreeMap::new();
    let mut stack: Vec<String> = graph
        .root_deps()
        .iter()
        .filter(|d| filter.keeps(d.dep_type))
        .filter(|d| !(no_optional && matches!(d.dep_type, aube_lockfile::DepType::Optional)))
        .map(|d| d.dep_path.clone())
        .collect();
    while let Some(dep_path) = stack.pop() {
        if out.contains_key(&dep_path) {
            continue;
        }
        let Some(pkg) = graph.get_package(&dep_path) else {
            continue;
        };
        out.insert(dep_path.clone(), pkg);
        for (name, version) in &pkg.dependencies {
            stack.push(format!("{name}@{version}"));
        }
    }
    out
}

/// Restore `cwd` after a filtered-workspace loop and fold any restore
/// error into the original `result`. Filter loops mutate the process
/// cwd so they can run per-package commands as if the user were in
/// that directory; this puts things back exactly once, even when the
/// loop itself failed.
pub(crate) fn finish_filtered_workspace(
    cwd: &Path,
    result: miette::Result<()>,
) -> miette::Result<()> {
    let restore =
        retarget_cwd(cwd).wrap_err_with(|| format!("failed to restore cwd to {}", cwd.display()));
    match result {
        Ok(()) => restore,
        Err(err) => {
            let _ = restore;
            Err(err)
        }
    }
}

/// Write lockfile preserving existing format and log the file name.
pub(crate) fn write_and_log_lockfile(
    cwd: &Path,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<std::path::PathBuf> {
    let written_path = aube_lockfile::write_lockfile_preserving_existing(cwd, graph, manifest)
        .into_diagnostic()
        .wrap_err("failed to write lockfile")?;
    eprintln!(
        "Wrote {}",
        written_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| written_path.display().to_string())
    );
    Ok(written_path)
}

/// Walk up from `start` looking for a directory that marks a workspace
/// root — either an `aube-workspace.yaml` / `pnpm-workspace.yaml` file
/// or a `package.json` with a `workspaces` field.
pub(crate) fn find_workspace_root(start: &std::path::Path) -> miette::Result<std::path::PathBuf> {
    crate::dirs::find_workspace_root(start).ok_or_else(|| {
        miette!(
            "no workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) found above {}",
            start.display()
        )
    })
}

/// Resolve `--filter` to the matching workspace packages, returning the
/// workspace root alongside the matches. Callers need the root to
/// compute importer paths, resolve the lockfile, etc., and `cwd`
/// alone isn't it in yarn / npm / bun subpackage installs where only
/// the monorepo root carries `package.json#workspaces`.
pub(crate) fn select_workspace_packages(
    cwd: &std::path::Path,
    filter: &aube_workspace::selector::EffectiveFilter,
    command: &str,
) -> miette::Result<(
    std::path::PathBuf,
    Vec<aube_workspace::selector::SelectedPackage>,
)> {
    let root = crate::dirs::find_workspace_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let workspace_pkgs = aube_workspace::find_workspace_packages(&root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "aube {command}: --filter requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at or above {}",
            cwd.display()
        ));
    }
    let matched =
        aube_workspace::selector::select_workspace_packages(&root, &workspace_pkgs, filter)
            .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if matched.is_empty() {
        return Err(miette!(
            "aube {command}: filter {filter:?} did not match any workspace package"
        ));
    }
    Ok((root, matched))
}

/// Resolve a version spec against a full packument. Returns the concrete
/// version string to look up in the `versions` object.
///
/// Resolution order, matching npm/pnpm:
/// 1. No spec → `dist-tags.latest`
/// 2. Spec is a dist-tag → `dist-tags[spec]`
/// 3. Spec is an exact version in `versions` → that version
/// 4. Spec is a semver range → highest matching version in `versions`
///
/// Shared by `aube view` and `aube store add` so fixes land in one place.
pub(crate) fn resolve_version(packument: &serde_json::Value, spec: Option<&str>) -> Option<String> {
    let dist_tags = packument.get("dist-tags").and_then(|v| v.as_object());
    let versions = packument.get("versions").and_then(|v| v.as_object())?;

    let spec = match spec {
        None | Some("") => {
            return dist_tags?
                .get("latest")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        Some(s) => s,
    };

    if let Some(tag) = dist_tags.and_then(|t| t.get(spec)).and_then(|v| v.as_str()) {
        return Some(tag.to_string());
    }

    if versions.contains_key(spec) {
        return Some(spec.to_string());
    }

    let range: node_semver::Range = spec.parse().ok()?;
    versions
        .keys()
        .filter_map(|v| {
            v.parse::<node_semver::Version>()
                .ok()
                .filter(|parsed| parsed.satisfies(&range))
                .map(|parsed| (v.clone(), parsed))
        })
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(raw, _)| raw)
}

/// Split `name[@version]` into the package name and optional version spec.
/// Handles scoped packages (`@scope/name[@version]`) correctly — the first
/// `@` in a scoped input is the scope sigil, not a version separator.
///
/// Returns borrowed slices of the input. Callers that need owned `String`s
/// or a default like `"latest"` can adapt the result at their call site.
pub(crate) fn split_name_spec(input: &str) -> (&str, Option<&str>) {
    aube_util::pkg::split_name_spec(input)
}

/// Percent-encode a package name for npm registry path segments.
/// `@scope/name` becomes `@scope%2Fname`; the leading `@` stays literal
/// and only the scope/name slash is encoded. Plain names pass through.
///
/// Shared between `publish` and `unpublish` (both target
/// `{registry}/{name}/...` endpoints) so the two write commands can't
/// drift on URL shape — the registry routes auth on these paths, so
/// even a subtle encoding change would break one command silently
/// while leaving the other working.
pub(crate) fn encode_package_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('@')
        && let Some((scope, pkg)) = rest.split_once('/')
    {
        return format!("@{scope}%2F{pkg}");
    }
    name.to_string()
}

#[cfg(test)]
mod encode_package_name_tests {
    use super::encode_package_name;

    #[test]
    fn scoped_name_encodes_slash() {
        assert_eq!(encode_package_name("@scope/pkg"), "@scope%2Fpkg");
    }

    #[test]
    fn plain_name_passthrough() {
        assert_eq!(encode_package_name("lodash"), "lodash");
    }

    #[test]
    fn malformed_scoped_name_passthrough() {
        // `@scope` with no slash isn't a valid package name, but we
        // shouldn't panic — return it verbatim so the registry can
        // surface the error.
        assert_eq!(encode_package_name("@scope"), "@scope");
    }
}

#[cfg(test)]
mod split_name_spec_tests {
    use super::split_name_spec;

    #[test]
    fn plain_name() {
        assert_eq!(split_name_spec("lodash"), ("lodash", None));
    }

    #[test]
    fn name_with_version() {
        assert_eq!(
            split_name_spec("lodash@4.17.21"),
            ("lodash", Some("4.17.21"))
        );
    }

    #[test]
    fn name_with_range() {
        assert_eq!(split_name_spec("lodash@^4"), ("lodash", Some("^4")));
    }

    #[test]
    fn name_with_tag() {
        assert_eq!(split_name_spec("react@next"), ("react", Some("next")));
    }

    #[test]
    fn scoped_no_version() {
        assert_eq!(split_name_spec("@babel/core"), ("@babel/core", None));
    }

    #[test]
    fn scoped_with_version() {
        assert_eq!(
            split_name_spec("@babel/core@7.0.0"),
            ("@babel/core", Some("7.0.0"))
        );
    }
}

/// Auto-install if needed, unless disabled.
pub(crate) async fn ensure_installed(no_install: bool) -> miette::Result<()> {
    if no_install {
        return Ok(());
    }
    if skip_auto_install_on_package_manager_mismatch() {
        return Ok(());
    }

    let initial_cwd = crate::dirs::cwd()?;
    // Prefer the workspace root as the freshness anchor. A monorepo
    // install writes its state files at the workspace root —
    // subpackages get symlinked `node_modules/` with no state file of
    // their own. Walking up only to the nearest `package.json` (the
    // subpackage itself) would miss that state and report "install
    // state not found" on every `aube run`/`exec`/`start` from a
    // subpackage even when the root install is fresh. Fall back to the
    // nearest `package.json` for non-workspace projects, and finally
    // to the cwd itself so we never panic resolving it.
    let cwd = crate::dirs::find_workspace_root(&initial_cwd)
        .or_else(|| crate::dirs::find_project_root(&initial_cwd))
        .unwrap_or(initial_cwd);
    // Resolve both pieces of auto-install policy in a single
    // `with_settings_ctx` call so the `.npmrc` + workspace-yaml read
    // pays off once. `aubeNoAutoInstall` lets a project/workspace opt
    // out of the staleness check entirely (env alias:
    // `AUBE_NO_AUTO_INSTALL`). `optimisticRepeatInstall=false`
    // disables the cheap lockfile/manifest hash short-circuit so every
    // check becomes a full install — matches pnpm's semantics where
    // the fast path is opt-out, not a staleness contract.
    let (skip_auto_install, optimistic_repeat) = with_settings_ctx(&cwd, |ctx| {
        (
            aube_settings::resolved::aube_no_auto_install(ctx),
            aube_settings::resolved::optimistic_repeat_install(ctx),
        )
    });
    if skip_auto_install {
        return Ok(());
    }
    let g = global_frozen_override();
    let needs = if optimistic_repeat {
        crate::state::check_needs_install(&cwd)
    } else {
        Some("optimisticRepeatInstall=false".to_string())
    };
    let verify_mode = resolve_verify_deps_before_run(&cwd)?;
    // A global `--frozen-lockfile` / `--no-frozen-lockfile` /
    // `--prefer-frozen-lockfile` re-triggers the install path even
    // when the state file says the tree is fresh, so the flag is
    // honored on every command that auto-installs.
    let Some(reason) = needs.or_else(|| g.map(|o| format!("global {} flag", o.cli_flag()))) else {
        return Ok(());
    };
    match verify_mode {
        VerifyDepsBeforeRun::Skip => return Ok(()),
        VerifyDepsBeforeRun::Warn => {
            eprintln!("Dependencies need install before run: {reason}");
            return Ok(());
        }
        VerifyDepsBeforeRun::Error => {
            return Err(miette!(
                "dependencies need install before run: {reason}\nRun `aube install`, or set verifyDepsBeforeRun=install to let aube do it automatically."
            ));
        }
        VerifyDepsBeforeRun::Install => {}
    }
    eprintln!("Auto-installing: {reason}");
    let mode = chained_frozen_mode(install::FrozenMode::Prefer);
    let mut opts = install::InstallOptions::with_mode(mode);
    opts.strict_no_lockfile = matches!(g, Some(install::FrozenOverride::Frozen));
    install::run(opts).await?;

    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum VerifyDepsBeforeRun {
    Install,
    Warn,
    Error,
    Skip,
}

fn resolve_verify_deps_before_run(cwd: &std::path::Path) -> miette::Result<VerifyDepsBeforeRun> {
    let npmrc = aube_registry::config::load_npmrc_entries(cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &empty_ws,
        env: &env,
        cli: &[],
    };
    let raw = aube_settings::resolved::verify_deps_before_run(&ctx);
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "false" | "0" => VerifyDepsBeforeRun::Skip,
        "warn" => VerifyDepsBeforeRun::Warn,
        "error" => VerifyDepsBeforeRun::Error,
        "prompt" | "install" => VerifyDepsBeforeRun::Install,
        _ => VerifyDepsBeforeRun::Install,
    })
}

/// Remove an existing file/dir/symlink at the given path, if present.
pub(crate) fn remove_existing(path: &std::path::Path) -> miette::Result<()> {
    if path.symlink_metadata().is_err() {
        return Ok(());
    }
    if path.is_dir() && !path.is_symlink() {
        std::fs::remove_dir_all(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display()))?;
    } else {
        std::fs::remove_file(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

pub(crate) fn workspace_importer_path(
    workspace_root: &std::path::Path,
    dir: &std::path::Path,
) -> miette::Result<String> {
    let rel = dir.strip_prefix(workspace_root).map_err(|_| {
        miette!(
            "workspace package {} is outside {}",
            dir.display(),
            workspace_root.display()
        )
    })?;
    if rel.as_os_str().is_empty() {
        Ok(".".to_string())
    } else {
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }
}

/// Create a directory link (symlink on Unix, NTFS junction on
/// Windows). Thin re-export of [`aube_linker::create_dir_link`] —
/// the linker owns the platform-specific implementation so every
/// directory-link call site in the workspace behaves identically,
/// including Windows' "junctions not symlinks" choice that keeps
/// installs working without Developer Mode.
pub(crate) fn symlink_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    aube_linker::create_dir_link(src, dst)
}

/// Dep-type filter derived from `--prod` / `--dev` on list-style commands
/// (`list`, `why`). Both commands take the same two flags with the same
/// semantics — this enum is the shared derivation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DepFilter {
    /// Include every dep type.
    All,
    /// `--prod`: include `Production` and `Optional`, drop `Dev`.
    ProdOnly,
    /// `--dev`: include only `Dev`.
    DevOnly,
}

impl DepFilter {
    /// Collapse the two mutually-exclusive boolean flags into a filter.
    /// `(true, _)` wins because clap enforces `conflicts_with = "dev"`.
    pub(crate) fn from_flags(prod: bool, dev: bool) -> Self {
        match (prod, dev) {
            (true, _) => Self::ProdOnly,
            (_, true) => Self::DevOnly,
            _ => Self::All,
        }
    }

    /// Does this filter keep the given dep type?
    pub(crate) fn keeps(self, dep_type: aube_lockfile::DepType) -> bool {
        use aube_lockfile::DepType;
        matches!(
            (self, dep_type),
            (Self::All, _)
                | (Self::ProdOnly, DepType::Production | DepType::Optional)
                | (Self::DevOnly, DepType::Dev)
        )
    }
}

#[cfg(test)]
mod dep_filter_tests {
    use super::*;
    use aube_lockfile::DepType;

    #[test]
    fn all_keeps_everything() {
        let f = DepFilter::from_flags(false, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Dev));
        assert!(f.keeps(DepType::Optional));
    }

    #[test]
    fn prod_keeps_production_and_optional() {
        let f = DepFilter::from_flags(true, false);
        assert!(f.keeps(DepType::Production));
        assert!(f.keeps(DepType::Optional));
        assert!(!f.keeps(DepType::Dev));
    }

    #[test]
    fn dev_keeps_only_dev() {
        let f = DepFilter::from_flags(false, true);
        assert!(!f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Optional));
        assert!(f.keeps(DepType::Dev));
    }

    #[test]
    fn prod_wins_over_dev_when_both_set() {
        // clap should prevent this combination via conflicts_with, but we
        // still want deterministic behavior if it ever gets through.
        let f = DepFilter::from_flags(true, true);
        assert!(f.keeps(DepType::Production));
        assert!(!f.keeps(DepType::Dev));
    }

    #[test]
    fn package_manager_mismatch_skip_auto_install_defaults_off() {
        assert!(!skip_auto_install_on_package_manager_mismatch());
    }
}
