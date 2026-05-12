use super::make_client;
use crate::progress::InstallProgress;
use crate::state;
use aube_lockfile::DriftStatus;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::io::Write;

mod bin_linking;
mod delta;
mod dep_selection;
mod frozen;
mod git_prepare;
mod lifecycle;
pub(crate) mod node_gyp_bootstrap;
mod prefetch;
mod settings;
mod side_effects_cache;

pub(crate) use bin_linking::{PkgJsonCache, link_dep_bins, materialized_pkg_dir};
use bin_linking::{link_bin_entries, link_bins, link_bins_for_dep};
pub use dep_selection::DepSelection;
pub use frozen::{FrozenMode, FrozenOverride, GlobalVirtualStoreFlags};
use git_prepare::{prepare_scratch_copy, run_git_dep_prepare};
pub(crate) use lifecycle::{
    JailBuildPolicy, build_policy_from_manifest_sources, build_policy_from_sources,
    run_dep_lifecycle_scripts,
};
use lifecycle::{
    resolve_link_strategy, run_import_on_blocking, run_root_lifecycle, unreviewed_dep_builds,
    validate_required_scripts,
};
pub(crate) use settings::PeerDependencyRules;
pub(crate) use settings::{resolve_dependency_policy, resolve_force_metadata_primer};
pub(crate) use side_effects_cache::{SideEffectsCacheConfig, side_effects_cache_root};

use settings::{
    ResolverConfigInputs, check_unmet_peers, configure_resolver,
    default_lockfile_network_concurrency, default_streaming_network_concurrency,
    detect_aube_dir_gvs_mode, find_gvs_incompatible_trigger, maybe_cleanup_unused_catalogs,
    resolve_dedupe_peer_dependents, resolve_dedupe_peers, resolve_git_shallow_hosts,
    resolve_link_concurrency, resolve_network_concurrency, resolve_peers_from_workspace_root,
    resolve_peers_suffix_max_length, resolve_side_effects_cache,
    resolve_side_effects_cache_readonly, resolve_strict_peer_dependencies,
    resolve_strict_store_pkg_content_check, resolve_symlink, resolve_use_running_store_server,
    resolve_verify_store_integrity,
};

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// Install only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,
    /// Skip devDependencies; install only production deps
    #[arg(short = 'P', long, visible_alias = "production")]
    pub prod: bool,
    /// Allow every dependency's lifecycle scripts to run.
    ///
    /// Bypasses the `allowBuilds` allowlist. Do not use in CI.
    #[arg(long)]
    pub dangerously_allow_all_builds: bool,
    /// Re-resolve lockfile entries whose spec drifted from package.json.
    ///
    /// Leaves everything else pinned at its locked version. Unchanged
    /// specs keep their existing version and integrity hash; only
    /// drifted entries (and any new transitives they pull in) get
    /// re-resolved.
    #[arg(long, conflicts_with_all = ["frozen_lockfile", "no_frozen_lockfile", "prefer_frozen_lockfile"])]
    pub fix_lockfile: bool,
    /// Force reinstall, ignoring lockfile/state freshness.
    ///
    /// Bypasses the `node_modules/.aube-state` freshness check and
    /// re-resolves the lockfile even when nothing has drifted. Mirrors
    /// pnpm's `install --force`.
    #[arg(long)]
    pub force: bool,
    /// Add a global pnpmfile that runs before the local one.
    ///
    /// Mirrors pnpm's `--global-pnpmfile <path>`. Relative paths
    /// resolve against the project root. The global hook runs first
    /// and the local hook (if any) runs second, so local mutations
    /// win on conflicts — matching pnpm's composition order.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// Skip running `.pnpmfile.mjs` / `.pnpmfile.cjs` hooks for this install
    #[arg(long)]
    pub ignore_pnpmfile: bool,
    /// Skip lifecycle scripts (no-op; aube already skips by default)
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Read and write the lockfile in the given directory.
    ///
    /// Instead of placing the lockfile alongside `package.json`, the
    /// project becomes an importer keyed by its relative path from the
    /// lockfile directory. Mirrors pnpm's `--lockfile-dir`.
    #[arg(long, value_name = "PATH")]
    pub lockfile_dir: Option<String>,
    /// Resolve dependencies and write the lockfile, but don't link
    /// `node_modules`.
    ///
    /// Useful for CI workflows that only update the lockfile.
    #[arg(long, conflicts_with = "frozen_lockfile")]
    pub lockfile_only: bool,
    /// Merge per-branch lockfiles into the main `aube-lock.yaml`.
    ///
    /// Combines every `aube-lock.<branch>.yaml` file in the project
    /// into `aube-lock.yaml` and deletes the branch files. Companion
    /// to `gitBranchLockfile`. When
    /// `mergeGitBranchLockfilesBranchPattern` is set in
    /// `pnpm-workspace.yaml`, this happens automatically on matching
    /// branches; the flag forces it regardless.
    #[arg(long)]
    pub merge_git_branch_lockfiles: bool,
    /// Cap concurrent tarball downloads.
    ///
    /// Overrides `network-concurrency` from `.npmrc` /
    /// `aube-workspace.yaml` when set. Falls back to an auto-scaled
    /// default of worker count x3, clamped to 16-64.
    #[arg(long, value_name = "N")]
    pub network_concurrency: Option<u64>,
    /// Skip optionalDependencies; don't install optional native modules
    #[arg(long)]
    pub no_optional: bool,
    /// Inverse of `--side-effects-cache`.
    #[arg(long, overrides_with = "side_effects_cache")]
    pub no_side_effects_cache: bool,
    /// Inverse of `--verify-store-integrity`.
    ///
    /// Skips the SHA-512 verify step for every tarball aube pulls
    /// into the store during this install.
    #[arg(long, overrides_with = "verify_store_integrity")]
    pub no_verify_store_integrity: bool,
    /// Which layout to materialize `node_modules/` as.
    ///
    /// `isolated` (default) uses pnpm's `.aube/`-backed symlink tree;
    /// `hoisted` builds an npm-style flat tree with conflict nesting.
    /// Overrides `node-linker` / `nodeLinker` from `.npmrc` /
    /// `aube-workspace.yaml` when set. `pnp` is not supported.
    #[arg(long, value_name = "MODE")]
    pub node_linker: Option<String>,
    /// Fail if any metadata or tarball isn't already in the local cache.
    ///
    /// Never hits the network.
    #[arg(long, conflicts_with = "prefer_offline")]
    pub offline: bool,
    /// How to import package files from the global store into the
    /// virtual store.
    ///
    /// One of `auto` (default: detect the fastest strategy),
    /// `hardlink`, `copy`, `clone` (reflink; falls back to copy
    /// pending strict enforcement), or `clone-or-copy` (reflink with
    /// a copy fallback). Overrides `package-import-method` /
    /// `packageImportMethod` from `.npmrc` / `aube-workspace.yaml`
    /// when set.
    #[arg(long, value_name = "METHOD")]
    pub package_import_method: Option<String>,
    /// Override the local pnpmfile location.
    ///
    /// Mirrors pnpm's `--pnpmfile <path>`. Relative paths resolve
    /// against the project root; absolute paths are used as-is. Wins
    /// over `pnpmfilePath` from `pnpm-workspace.yaml`. A typo (target
    /// missing) is a hard miss with a warning rather than a silent
    /// fallback to the default.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub pnpmfile: Option<std::path::PathBuf>,
    /// Prefer cached metadata over revalidation; only hit the network on a miss.
    #[arg(long, conflicts_with = "offline")]
    pub prefer_offline: bool,
    /// Selectively hoist matching transitive deps to the root node_modules.
    ///
    /// Repeatable; comma-separated values are also accepted.
    #[arg(long, value_name = "GLOB", value_delimiter = ',')]
    pub public_hoist_pattern: Vec<String>,
    /// How to resolve version ranges.
    ///
    /// `highest` (pnpm's classic behavior) or `time-based` (pick the
    /// lowest satisfying direct dep and constrain transitives by a
    /// publish-date cutoff). Accepts pnpm's aliases `time` and
    /// `lowest-direct`. When omitted, falls back to the
    /// `resolution-mode` key in `.npmrc` / `aube-workspace.yaml`.
    #[arg(long, value_name = "MODE")]
    pub resolution_mode: Option<String>,
    /// Hoist every non-local transitive dep to the top-level
    /// `node_modules/`.
    ///
    /// Overrides `shamefully-hoist` / `shamefullyHoist` from
    /// `.npmrc` / `aube-workspace.yaml` when set.
    #[arg(long)]
    pub shamefully_hoist: bool,
    /// Cache post-build side effects for dependency packages.
    ///
    /// Defaults to on and only applies to packages allowed by
    /// `allowBuilds` / `onlyBuiltDependencies`. Pair with
    /// `--no-side-effects-cache` to opt out.
    #[arg(long, overrides_with = "no_side_effects_cache")]
    pub side_effects_cache: bool,
    /// Verify tarball SHA-512 before importing into the store.
    ///
    /// Checks each tarball against the lockfile integrity. Defaults to
    /// `true` (pnpm parity); pair with `--no-verify-store-integrity`
    /// to skip.
    #[arg(long, overrides_with = "no_verify_store_integrity")]
    pub verify_store_integrity: bool,
    /// Short alias for the global `--workspace-root` flag.
    ///
    /// Runs install from the workspace root regardless of cwd (`pnpm
    /// install -w`).
    #[arg(short = 'w', hide = true)]
    pub workspace_root_short: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

impl InstallArgs {
    /// Build the CLI flag bag that feeds
    /// [`aube_settings::ResolveCtx::cli`]. Each entry is a
    /// `(flag_name, value)` pair where `flag_name` matches a
    /// `sources.cli` alias declared in `settings.toml`. Values are
    /// already normalized to the raw form the
    /// `aube_settings::values::*_from_cli` helpers expect
    /// (`"true"`/`"false"` for bools, passthrough for strings). Only
    /// flags explicitly present on the command line are emitted —
    /// unset flags stay out of the bag so they don't override
    /// lower-precedence sources with their clap-derived default.
    pub fn to_cli_flag_bag(
        &self,
        global: Option<FrozenOverride>,
        global_gvs: GlobalVirtualStoreFlags,
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if let Some(mode) = self.resolution_mode.as_deref() {
            out.push(("resolution-mode".to_string(), mode.to_string()));
        }
        if let Some(linker) = self.node_linker.as_deref() {
            out.push(("node-linker".to_string(), linker.to_string()));
        }
        if let Some(d) = self.lockfile_dir.as_deref() {
            out.push(("lockfile-dir".to_string(), d.to_string()));
        }
        if let Some(method) = self.package_import_method.as_deref() {
            out.push(("package-import-method".to_string(), method.to_string()));
        }
        for pattern in &self.public_hoist_pattern {
            out.push(("public-hoist-pattern".to_string(), pattern.to_string()));
        }
        if self.shamefully_hoist {
            out.push(("shamefully-hoist".to_string(), "true".to_string()));
        }
        out.extend(global_gvs.to_cli_flag_bag());
        if let Some(ovr) = global {
            let (k, v) = ovr.cli_flag_bag_entry();
            out.push((k.to_string(), v.to_string()));
        }
        if let Some(n) = self.network_concurrency {
            out.push(("network-concurrency".to_string(), n.to_string()));
        }
        if self.verify_store_integrity {
            out.push(("verify-store-integrity".to_string(), "true".to_string()));
        }
        if self.no_verify_store_integrity {
            out.push(("verify-store-integrity".to_string(), "false".to_string()));
        }
        if self.side_effects_cache {
            out.push(("side-effects-cache".to_string(), "true".to_string()));
        }
        if self.no_side_effects_cache {
            out.push(("side-effects-cache".to_string(), "false".to_string()));
        }
        // `--fix-lockfile` is a distinct `FrozenMode::Fix` state, not a
        // `frozen-lockfile=false` shorthand — don't leak it into the
        // settings bag; `into_options` routes it directly.
        out
    }

    /// Resolve this CLI arg set into a full `InstallOptions`,
    /// consulting the workspace config for `preferFrozenLockfile`
    /// when no CLI flag forces it. Takes a pre-built `cli_flags` bag
    /// so the caller can reuse a single `to_cli_flag_bag` call for
    /// both the early `ResolveCtx` (used to read
    /// `preferFrozenLockfile`) and the `InstallOptions.cli_flags`
    /// field that threads the same values into `install::run`.
    pub fn into_options(
        self,
        global: Option<FrozenOverride>,
        yaml_prefer_frozen: Option<bool>,
        cli_flags: Vec<(String, String)>,
        env_snapshot: Vec<(String, String)>,
    ) -> InstallOptions {
        let force = self.force;
        let mode = if self.fix_lockfile {
            FrozenMode::Fix
        } else if force && global.is_none() {
            // `--force` without an explicit frozen mode re-resolves.
            FrozenMode::No
        } else {
            FrozenMode::from_override(global, yaml_prefer_frozen)
        };
        let network_mode = if self.offline {
            aube_registry::NetworkMode::Offline
        } else if self.prefer_offline {
            aube_registry::NetworkMode::PreferOffline
        } else {
            aube_registry::NetworkMode::Online
        };
        // pnpm parity: explicit `--frozen-lockfile` errors on a missing
        // lockfile (ERR_PNPM_NO_LOCKFILE), but the auto-CI default does
        // not — CI without a lockfile just does a regular resolve + write.
        let strict_no_lockfile = matches!(global, Some(FrozenOverride::Frozen));
        InstallOptions {
            project_dir: None,
            mode,
            dep_selection: DepSelection::from_flags(self.prod, self.dev, self.no_optional),
            ignore_pnpmfile: self.ignore_pnpmfile,
            pnpmfile: self.pnpmfile,
            global_pnpmfile: self.global_pnpmfile,
            ignore_scripts: self.ignore_scripts,
            lockfile_only: self.lockfile_only,
            merge_git_branch_lockfiles: self.merge_git_branch_lockfiles,
            dangerously_allow_all_builds: self.dangerously_allow_all_builds,
            network_mode,
            minimum_release_age_override: None,
            strict_no_lockfile,
            force,
            cli_flags,
            env_snapshot,
            git_prepare_depth: 0,
            inherited_build_policy: None,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
            // Argumentless `aube install` runs root lifecycle hooks; the
            // chained-call constructor (`with_mode`) is where commands
            // with package args opt into skipping them.
            skip_root_lifecycle: false,
        }
    }
}

/// Aggregated options for `install::run`. Grouped into a struct so we can add
/// more flags (`--no-optional`, `--offline`, etc.) without changing every caller.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Explicit project directory for in-process nested installs. When
    /// unset, install discovers the project from the logical command cwd.
    pub project_dir: Option<std::path::PathBuf>,
    pub mode: FrozenMode,
    /// Which dep sections to keep in the materialized graph
    /// (`--prod` / `--dev` / `--no-optional`, in any valid combo).
    pub dep_selection: DepSelection,
    /// `--ignore-pnpmfile`: don't load or execute `.pnpmfile.mjs` / `.pnpmfile.cjs`
    /// hooks for this install, even if one exists in the project root.
    pub ignore_pnpmfile: bool,
    /// `--pnpmfile <path>`: override the local pnpmfile location for
    /// this run. Wins over `pnpmfilePath` in `pnpm-workspace.yaml` and
    /// the `.pnpmfile.mjs` / `.pnpmfile.cjs` defaults. `None` falls
    /// back to the workspace yaml + default search.
    pub pnpmfile: Option<std::path::PathBuf>,
    /// `--global-pnpmfile <path>`: add a second pnpmfile that runs
    /// *before* the local one, so org-wide rules can be layered under
    /// per-project hooks.
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// `--ignore-scripts`: skip root lifecycle scripts (`preinstall`,
    /// `install`, `postinstall`, `prepare`) *and* every dependency's
    /// lifecycle scripts, regardless of `allowBuilds`.
    pub ignore_scripts: bool,
    /// `--lockfile-only`: resolve and write the lockfile, but skip
    /// linking `node_modules` and running lifecycle scripts. Useful
    /// for CI workflows that only need to refresh the lockfile.
    pub lockfile_only: bool,
    /// `--merge-git-branch-lockfiles`: force a one-shot branch
    /// lockfile merge before the main install runs. See
    /// [`aube_lockfile::merge_branch_lockfiles`]. Equivalent to the
    /// `mergeGitBranchLockfilesBranchPattern` setting matching the
    /// current branch.
    pub merge_git_branch_lockfiles: bool,
    /// `--dangerously-allow-all-builds`: run every dependency's
    /// lifecycle scripts, bypassing the `allowBuilds` allowlist.
    /// Equivalent to pnpm's `--dangerously-allow-all-builds`.
    pub dangerously_allow_all_builds: bool,
    /// `--offline` / `--prefer-offline`: controls whether the registry client
    /// is allowed to hit the network during resolve and fetch.
    pub network_mode: aube_registry::NetworkMode,
    /// CLI override for `minimumReleaseAge` in minutes. `None` means
    /// "consult .npmrc / workspace config" — the run path resolves it
    /// to a concrete value (defaulting to 1440) before creating the
    /// resolver. There is no CLI flag yet, so this is always `None`
    /// today; reserved so future flags don't change the call site.
    pub minimum_release_age_override: Option<u64>,
    /// Error out if no lockfile is present. Matches pnpm's
    /// `ERR_PNPM_NO_LOCKFILE`: set by an explicit `--frozen-lockfile`
    /// flag and by `aube ci` / `aube clean-install`. The auto-CI
    /// default (`CI=1`, no explicit flag) leaves this `false` so a
    /// fresh checkout still resolves and writes a lockfile.
    pub strict_no_lockfile: bool,
    /// `--force`: re-resolve and relink even when `node_modules/.aube-state` says the
    /// tree is up to date. Mirrors pnpm's `install --force`.
    pub force: bool,
    /// Parsed CLI flag bag forwarded into
    /// [`aube_settings::ResolveCtx::cli`] so the build-time-generated
    /// `aube_settings::resolved::*` accessors can see CLI values with
    /// the highest precedence. Entries are `(long_flag, value)` pairs
    /// where `value` is already normalized to the raw form the
    /// type-specific resolver expects (`"true"`/`"false"` for bools,
    /// passthrough for strings). Populated at the clap-aware entry
    /// point via [`InstallArgs::to_cli_flag_bag`] and then threaded
    /// through every downstream caller that builds a `ResolveCtx`.
    pub cli_flags: Vec<(String, String)>,
    /// Process environment snapshot forwarded into
    /// [`aube_settings::ResolveCtx::env`]. Captured once at the
    /// clap-aware entry point via
    /// [`aube_settings::values::capture_env`] and threaded through so
    /// every `ResolveCtx` within a single `aube install` invocation
    /// sees the same env, keeping `preferFrozenLockfile` and the
    /// settings resolved inside [`run`] consistent. Commands that
    /// construct `InstallOptions` directly (`ci`, `deploy`) populate
    /// this with [`capture_env`] at their own entry point.
    pub env_snapshot: Vec<(String, String)>,
    /// Current git dependency prepare nesting depth. Kept in options so
    /// in-process prepare installs do not need cascading environment vars.
    pub git_prepare_depth: u32,
    /// Dependency build policy inherited by an in-process nested install.
    /// Used for git dependency `prepare`: the nested install runs in a
    /// scratch clone, but dependency build approval belongs to the outer
    /// project that requested the git package.
    pub inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    /// Global `--filter` / `--filter-prod` selectors. Resolution and
    /// lockfile writing still happen at the workspace root; these
    /// selectors narrow only the graph passed to the linker. Prod-only
    /// selectors additionally skip `devDependencies` edges during
    /// graph traversal — see `aube_workspace::selector::EffectiveFilter`.
    pub workspace_filter: aube_workspace::selector::EffectiveFilter,
    /// Skip the root package's `preinstall` / `install` / `postinstall` /
    /// `prepare` lifecycle hooks. pnpm parity: those hooks fire only on
    /// argumentless `pnpm install`. Every other user-facing entry point —
    /// `add`, `remove`, `update`, `dedupe`, `dlx`, patch tooling, the
    /// `ensure_installed` auto-install before `run`/`test` — must skip
    /// them so a chained `aube add foo` doesn't re-run an expensive root
    /// postinstall on every invocation. Independent of `ignore_scripts`,
    /// which also skips dep scripts. `with_mode()` defaults to `true`
    /// (chained-call constructor). The exceptions are argumentless
    /// `aube install` (`InstallArgs::into_options`), `aube ci` /
    /// `aube deploy` (literal struct constructions), and the nested
    /// git-prepare install — that one's "root" IS the git dep itself and
    /// running its `prepare` is the whole point.
    pub skip_root_lifecycle: bool,
}

#[derive(Default)]
struct InstallPhaseTimings {
    path: Option<std::path::PathBuf>,
    phases_ms: BTreeMap<&'static str, u128>,
    /// Last kernel snapshot, captured immediately after the previous
    /// phase recorded. The next [`record`] call diffs against this and
    /// emits a `kernel.<phase>` event with the per-phase user/sys CPU,
    /// peak RSS, and page fault deltas.
    last_kernel_snap: Option<aube_util::diag_kernel::KernelSnapshot>,
}

impl InstallPhaseTimings {
    fn from_env() -> Self {
        Self {
            path: std::env::var_os("AUBE_BENCH_PHASES_FILE").map(std::path::PathBuf::from),
            phases_ms: BTreeMap::new(),
            last_kernel_snap: aube_util::diag_kernel::snapshot(),
        }
    }

    fn record(&mut self, phase: &'static str, elapsed: std::time::Duration) {
        if self.path.is_some() {
            self.phases_ms.insert(phase, elapsed.as_millis());
        }
        aube_util::diag::event(
            aube_util::diag::Category::InstallPhase,
            phase,
            elapsed,
            None,
        );
        // When kernel sampling is on, emit a per-phase kernel delta so
        // user/sys CPU split, page fault counts, and peak RSS land in
        // the trace alongside the wall-time phase event.
        if aube_util::diag_kernel::enabled()
            && let Some(after) = aube_util::diag_kernel::snapshot()
        {
            if let Some(before) = self.last_kernel_snap.take() {
                aube_util::diag_kernel::emit_phase_delta(phase, before, after);
            }
            self.last_kernel_snap = Some(after);
        }
    }

    fn write(
        &self,
        cwd: &std::path::Path,
        total: std::time::Duration,
        packages: usize,
        cached: usize,
        fetched: usize,
    ) {
        let Some(path) = &self.path else {
            return;
        };
        let payload = serde_json::json!({
            "cwd": cwd,
            "scenario": std::env::var("AUBE_BENCH_SCENARIO").ok(),
            "total_ms": total.as_millis(),
            "packages": packages,
            "cached": cached,
            "fetched": fetched,
            "phases_ms": self.phases_ms,
        });
        let Ok(line) = serde_json::to_string(&payload) else {
            return;
        };
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "{line}");
            }
            Err(e) => tracing::debug!("failed to write install phase timings: {e}"),
        }
    }
}

impl InstallOptions {
    /// Construct with the given frozen mode and all other flags at their
    /// defaults. Used by commands that chain into install (`add`, `remove`,
    /// `update`, `ensure_installed`) where none of the install-specific flags
    /// apply.
    pub fn with_mode(mode: FrozenMode) -> Self {
        Self {
            project_dir: None,
            mode,
            dep_selection: DepSelection::All,
            ignore_pnpmfile: false,
            pnpmfile: None,
            global_pnpmfile: None,
            ignore_scripts: false,
            lockfile_only: false,
            merge_git_branch_lockfiles: false,
            dangerously_allow_all_builds: false,
            network_mode: aube_registry::NetworkMode::Online,
            minimum_release_age_override: None,
            strict_no_lockfile: false,
            force: false,
            cli_flags: Vec::new(),
            env_snapshot: aube_settings::values::capture_env(),
            git_prepare_depth: 0,
            inherited_build_policy: None,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
            // pnpm parity: every chained-call site (add / remove / update
            // / dedupe / dlx / patch / ensure_installed / git prepare)
            // skips root lifecycle hooks. Argumentless `aube install` is
            // the only construction path that runs them and it goes
            // through `InstallArgs::into_options`, not here.
            skip_root_lifecycle: true,
        }
    }
}

impl From<FrozenMode> for InstallOptions {
    fn from(mode: FrozenMode) -> Self {
        Self::with_mode(mode)
    }
}

/// Materialize a `file:` / `link:` package into the store.
///
/// `Directory` walks the target and hash-imports every file; `Tarball`
/// opens the `.tgz` and reuses the normal tarball importer. `Link`
/// returns `None` because link deps never have a store-backed index —
/// the linker symlinks directly to the target in step 2.
#[allow(clippy::too_many_arguments)]
pub(super) async fn import_local_source(
    store: &std::sync::Arc<aube_store::Store>,
    project_root: &std::path::Path,
    local: &aube_lockfile::LocalSource,
    client: Option<&std::sync::Arc<aube_registry::client::RegistryClient>>,
    ignore_scripts: bool,
    git_prepare_depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    git_shallow_hosts: &[String],
    pkg_name: &str,
    pkg_version: &str,
) -> miette::Result<Option<aube_store::PackageIndex>> {
    // `chain` is appended to per-error messages below so users see
    // *why* a `file:` / `link:` / git / remote-tarball dep was pulled
    // in. Empty when the package isn't in the resolved chain index
    // (e.g. when the install pipeline hasn't seeded one yet for an
    // out-of-band caller).
    let chain = crate::dep_chain::format_chain_for(pkg_name, pkg_version);
    use aube_lockfile::LocalSource;
    match local {
        LocalSource::Link(_) => Ok(None),
        LocalSource::Directory(rel) => {
            let abs = project_root.join(rel);
            if !abs.is_dir() {
                return Err(miette!(
                    "local dependency {}: {} is not a directory{chain}",
                    local.specifier(),
                    abs.display()
                ));
            }
            let index = store
                .import_directory(&abs)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::Tarball(rel) => {
            let abs = project_root.join(rel);
            let bytes = std::fs::read(&abs)
                .into_diagnostic()
                .wrap_err_with(|| format!("read {}{chain}", abs.display()))?;
            let index = store
                .import_tarball(&bytes)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::Git(g) => {
            // Materialize the git dep into a commit-keyed cache
            // directory and hardlink-import into the store exactly
            // like a `file:` directory. The resolver already pinned
            // `g.resolved` to a full commit SHA, so we route through
            // the same hosted-tarball-then-clone fallback npm and
            // pnpm use:
            //
            //   1. github / gitlab / bitbucket public reads → a flat
            //      HTTPS tarball over codeload (no `git` binary, no
            //      SSH key required).
            //   2. Anything else, plus codeload errors → shallow
            //      `git clone` over HTTPS (rewritten from the stored
            //      lockfile URL when the host is hosted, so an
            //      `git+ssh://git@github.com/…` lockfile still works
            //      on a host with no SSH key).
            //   3. Non-hosted hosts → unchanged: clone whatever URL
            //      the lockfile recorded, preserving SSH-only setups.
            //
            // Both the codeload extract and the clone share the
            // `(url, commit)` cache so the resolver's earlier call
            // for the same dep doesn't pay the network round-trip
            // twice.
            let url = g.url.clone();
            let resolved = g.resolved.clone();
            let spec = local.specifier();
            let hosted = aube_lockfile::parse_hosted_git(&url);
            let runtime_url = hosted
                .as_ref()
                .map(|h| h.https_url())
                .unwrap_or_else(|| url.clone());
            let codeload_url = hosted.as_ref().and_then(|h| h.tarball_url(&resolved));

            // Cache hit fast path: skip the HTTPS round-trip when the
            // resolver already populated the codeload cache for this
            // (url, commit) pair earlier in the install. Mirrors
            // `git_shallow_clone`'s top-of-function reuse check.
            let mut clone_dir: Option<std::path::PathBuf> = if codeload_url.is_some() {
                aube_store::codeload_cache_lookup(&url, &resolved).map(|(dir, _)| dir)
            } else {
                None
            };
            if clone_dir.is_none()
                && let (Some(c), Some(url_to_fetch)) = (client, codeload_url.as_deref())
            {
                match c.fetch_tarball_bytes(url_to_fetch).await {
                    Ok(bytes) => {
                        let bytes_vec = bytes.to_vec();
                        let url_for_extract = url.clone();
                        let resolved_for_extract = resolved.clone();
                        match tokio::task::spawn_blocking(move || {
                            aube_store::extract_codeload_tarball(
                                &bytes_vec,
                                &url_for_extract,
                                &resolved_for_extract,
                            )
                        })
                        .await
                        .map_err(|e| miette!("codeload extract task panicked: {e}"))?
                        {
                            Ok((dir, _sha)) => clone_dir = Some(dir),
                            Err(e) => {
                                tracing::debug!(
                                    %spec,
                                    "codeload extract failed, falling back to git clone: {e}",
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            %spec,
                            url = %aube_util::url::redact_url(url_to_fetch),
                            "codeload fetch failed, falling back to git clone: {e}",
                        );
                    }
                }
            }

            let clone_dir = if let Some(dir) = clone_dir {
                dir
            } else {
                let shallow = aube_store::git_host_in_list(&runtime_url, git_shallow_hosts);
                let url_for_clone = runtime_url.clone();
                let resolved_for_clone = resolved.clone();
                let (dir, _head_sha) = tokio::task::spawn_blocking(move || {
                    aube_store::git_shallow_clone(&url_for_clone, &resolved_for_clone, shallow)
                })
                .await
                .map_err(|e| miette!("git clone task panicked: {e}{chain}"))?
                .map_err(|e| miette!("failed to clone {spec}: {e}{chain}"))?;
                dir
            };

            // `&path:/<sub>` narrows the package root to a
            // subdirectory of the cloned repo (pnpm-compatible).
            // Everything below this line — manifest read, prepare
            // scratch copy, archive build, plain directory import —
            // operates on the subdir rather than the whole clone.
            //
            // Defense in depth against a `..`-laden subpath: the
            // parser already rejects them, but we also canonicalize
            // and assert the result stays under `clone_dir` so a
            // future code path that fills `subpath` from a different
            // source can't bypass the check.
            let pkg_root = match &g.subpath {
                Some(sub) => clone_dir.join(sub),
                None => clone_dir.clone(),
            };
            if !pkg_root.is_dir() {
                return Err(miette!(
                    "git dep {spec}: subpath {} not found in clone{chain}",
                    pkg_root.display()
                ));
            }
            if g.subpath.is_some() {
                let canonical_clone = clone_dir
                    .canonicalize()
                    .into_diagnostic()
                    .wrap_err_with(|| format!("canonicalize clone dir for {spec}{chain}"))?;
                let canonical_pkg = pkg_root
                    .canonicalize()
                    .into_diagnostic()
                    .wrap_err_with(|| format!("canonicalize subpath for {spec}{chain}"))?;
                if !canonical_pkg.starts_with(&canonical_clone) {
                    return Err(miette!(
                        "git dep {spec}: subpath {} escapes clone root {}{chain}",
                        canonical_pkg.display(),
                        canonical_clone.display()
                    ));
                }
            }

            // If the cloned repo defines a `prepare` script, treat
            // it as a source checkout that needs to be built before
            // we snapshot it. Matches npm/pnpm: a TypeScript repo
            // installed from git has devDependencies + a `prepare`
            // that compiles `src/` into `dist/`, and consumers
            // expect the built output. We run a nested `aube
            // install` inside the clone, which installs its deps
            // and runs its own root lifecycle hooks (including
            // `prepare`), then `aube pack`'s file-selection logic
            // snapshots exactly what would be published (honors
            // `files`, `.npmignore`, and skips `node_modules`).
            //
            // `--ignore-scripts` short-circuits the whole branch:
            // the only reason we'd pay the cost of a nested install
            // is to run `prepare`, so with scripts disabled we fall
            // through to the plain directory import. Matches pnpm,
            // which skips `prepare` for git deps under
            // `--ignore-scripts` as well.
            let manifest_path = pkg_root.join("package.json");
            let needs_prepare = !ignore_scripts
                && aube_manifest::PackageJson::from_path(&manifest_path)
                    .ok()
                    .is_some_and(|pj| pj.scripts.contains_key("prepare"));

            if needs_prepare {
                // Run `prepare` on a private copy of the checkout,
                // not on the shared `git_shallow_clone` cache
                // directory. The cache is keyed by (url, commit)
                // and reused across installs; mutating it in place
                // would leave `node_modules/`, `aube-lock.yaml`,
                // and any generated `dist/` behind, so a later
                // `aube install --ignore-scripts` — which falls
                // through to the plain directory-import path —
                // would silently pull those build artifacts into
                // the store even though the user asked for a
                // scripts-free install. Copying also isolates
                // concurrent installs of the same git dep from
                // clobbering each other's in-progress prepare.
                //
                // `ScratchDir` removes the copy on drop, including
                // on the error path.
                let scratch = prepare_scratch_copy(&pkg_root, &spec)?;
                run_git_dep_prepare(
                    scratch.path(),
                    &spec,
                    ignore_scripts,
                    git_prepare_depth,
                    inherited_build_policy,
                )
                .await?;
                let archive = crate::commands::pack::build_archive(scratch.path())
                    .wrap_err_with(|| format!("failed to pack prepared git dep {spec}{chain}"))?;
                let index = store
                    .import_tarball(&archive.tarball)
                    .map_err(|e| miette!("failed to import prepared {spec}: {e}{chain}"))?;
                return Ok(Some(index));
            }

            let index = store
                .import_directory(&pkg_root)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
        LocalSource::RemoteTarball(t) => {
            // Remote tarball URL: download once, verify the
            // resolver-pinned integrity, and import like any other
            // .tgz. Reuses the normal tarball importer so the
            // linker sees a plain PackageIndex. No store-level
            // index cache lookup — the canonical key would need to
            // be `(url, integrity)` rather than `(name, version)`
            // and remote tarball deps are rare enough that the
            // redundant walk isn't worth a new cache namespace.
            let client = client.ok_or_else(|| {
                miette!(
                    "internal: import_local_source called without a registry client for {}{chain}",
                    local.specifier()
                )
            })?;
            let bytes = client
                .fetch_tarball_bytes(&t.url)
                .await
                .map_err(|e| miette!("failed to fetch {}: {e}{chain}", t.url))?;
            if t.integrity.is_empty() {
                tracing::warn!(
                    code = aube_codes::warnings::WARN_AUBE_MISSING_INTEGRITY,
                    url = %aube_util::url::redact_url(&t.url),
                    "remote tarball lockfile entry has no integrity field; importing fetched bytes without verification (run `aube install --no-frozen-lockfile` to refresh the lockfile)",
                );
            } else {
                aube_store::verify_integrity(&bytes, &t.integrity)
                    .map_err(|e| miette!("{}: {e}{chain}", aube_util::url::redact_url(&t.url)))?;
            }
            let index = store
                .import_tarball(&bytes)
                .map_err(|e| miette!("failed to import {}: {e}{chain}", local.specifier()))?;
            Ok(Some(index))
        }
    }
}

/// Fetch tarballs for resolved packages, checking the index cache first.
/// Used by the lockfile path where all packages are known upfront.
/// Exposed to sibling commands so `aube fetch` can reuse the same
/// parallel-download + integrity-check + index-cache pipeline.
pub(super) async fn fetch_packages(
    packages: &BTreeMap<String, aube_lockfile::LockedPackage>,
    store: &std::sync::Arc<aube_store::Store>,
    client: std::sync::Arc<aube_registry::client::RegistryClient>,
    progress: Option<&InstallProgress>,
    ignore_scripts: bool,
    git_prepare_depth: u32,
    git_shallow_hosts: Vec<String>,
) -> miette::Result<(BTreeMap<String, aube_store::PackageIndex>, usize, usize)> {
    // Eager-client caller (`aube fetch`): the command only exists to
    // download tarballs, so there's no point deferring construction.
    // `skip_already_linked_shortcut=true` because `aube fetch`'s entire
    // job is to verify/populate the global store — it must not be
    // short-circuited by a stale `node_modules/.aube/<dep>` from a
    // prior install, which could leave the store empty on a setup
    // that wipes the global aube store but not `node_modules/` (e.g.
    // Docker layer caching, where the store lives in one cached
    // layer and `node_modules` in another).
    let cwd = crate::dirs::project_root_or_cwd()?;
    // `aube fetch` is a thin wrapper whose only job is populating
    // the store, so resolve `networkConcurrency` and
    // `verifyStoreIntegrity` from the project context here and hand
    // them down. Doing the resolve in the wrapper (instead of in
    // `aube fetch`'s own entry point) keeps the two call paths
    // honest: the lockfile install path and the standalone fetch
    // path share the same hardcoded fallback behavior when no
    // setting is configured.
    let files = crate::commands::FileSources::load(&cwd);
    let raw_workspace = aube_manifest::workspace::load_both(&cwd)
        .map(|(_, raw)| raw)
        .unwrap_or_default();
    let env = aube_settings::values::process_env();
    let ctx = files.ctx(&raw_workspace, env, &[]);
    let network_concurrency = resolve_network_concurrency(&ctx);
    let verify_integrity = resolve_verify_store_integrity(&ctx);
    let strict_integrity = settings::resolve_strict_store_integrity(&ctx);
    let strict_pkg_content_check = resolve_strict_store_pkg_content_check(&ctx);
    let virtual_store_dir_max_length = super::resolve_virtual_store_dir_max_length(&ctx);
    let aube_dir = super::resolve_virtual_store_dir(&ctx, &cwd);
    fetch_packages_with_root(
        packages,
        store,
        || client,
        progress,
        &cwd,
        &aube_dir,
        /*materialize_tx=*/ None,
        /*skip_already_linked_shortcut=*/ true,
        virtual_store_dir_max_length,
        ignore_scripts,
        network_concurrency,
        verify_integrity,
        strict_integrity,
        strict_pkg_content_check,
        git_prepare_depth,
        None,
        git_shallow_hosts,
    )
    .await
}

// Inputs for the GVS-prewarm materializer task. Built once before
// fetch starts, moved into spawned task.
#[allow(clippy::too_many_arguments)]
pub(super) struct GvsPrewarmInputs {
    pub graph: std::sync::Arc<aube_lockfile::LockfileGraph>,
    pub store: std::sync::Arc<aube_store::Store>,
    pub cwd: std::path::PathBuf,
    pub virtual_store_dir_max_length: usize,
    pub link_strategy: aube_linker::LinkStrategy,
    pub link_concurrency: Option<usize>,
    pub patches: aube_linker::Patches,
    pub patch_hashes: std::collections::BTreeMap<String, String>,
    pub node_version: Option<String>,
    pub build_policy: std::sync::Arc<aube_scripts::BuildPolicy>,
    pub use_global_virtual_store_override: Option<bool>,
}

/// Initial capacity for the (canonical_key, PackageIndex) channel
/// that feeds the GVS-prewarm materializer. Bounded so RSS on a
/// huge graph stays sane while a slow filesystem (Defender,
/// network share) backs up the materializer; backpressure only
/// kicks in under real producer/consumer skew.
///
/// Tokio mpsc capacity is fixed at construction, so a bigger
/// learned-from-prior-run value couldn't be applied to the
/// current channel anyway. A static cap keeps the construction
/// path obvious without dragging cross-run telemetry through the
/// hot send/recv loops for marginal gain.
pub(super) const MATERIALIZE_CHANNEL_CAPACITY: usize = 2048;

pub(super) type MaterializeChannel = (
    tokio::sync::mpsc::Sender<(String, aube_store::PackageIndex)>,
    tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
);

pub(super) type MaterializeJoinHandle = tokio::task::JoinHandle<
    miette::Result<(
        aube_linker::LinkStats,
        Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
    )>,
>;

pub(super) fn materialize_channel() -> MaterializeChannel {
    let (tx, rx) = tokio::sync::mpsc::channel(MATERIALIZE_CHANNEL_CAPACITY);
    aube_util::diag::register_channel("materialize", &tx, MATERIALIZE_CHANNEL_CAPACITY);
    (tx, rx)
}

/// Spawn the GVS-prewarm consumer with the given inputs and rx.
/// Centralizes the JoinHandle type + tokio::spawn boilerplate.
pub(super) fn spawn_gvs_prewarm(
    inputs: GvsPrewarmInputs,
    rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> MaterializeJoinHandle {
    tokio::spawn(run_gvs_prewarm_materializer(inputs, rx))
}

/// When the fetch task fails the surfaced error is often just
/// "materializer task exited before fetch finished" — a symptom of the
/// materializer dying first (its `rx` was dropped, fetch's `tx.send`
/// returned Err). Await the materializer (don't abort) so its real
/// error joins the chain. The returned report shows both errors:
///
/// * top message = how the pipeline aborted (the fetch error)
/// * source chain = why the materializer task failed (root cause)
///
/// If the materializer succeeded the fetch error was the real one and
/// is returned unchanged.
async fn combine_install_pipeline_errors(
    materialize_handle: MaterializeJoinHandle,
    fetch_err: miette::Report,
) -> miette::Report {
    let mat_err = match materialize_handle.await {
        Ok(Ok(_)) => return fetch_err,
        Ok(Err(err)) => err,
        Err(join_err) => Err::<(), _>(join_err)
            .into_diagnostic()
            .unwrap_err()
            .wrap_err("materializer task panicked"),
    };
    mat_err.wrap_err(format!("install aborted: {fetch_err}"))
}

// Consumes (canonical_key, index) from rx as tarballs land in the CAS,
// reflinks each into the global virtual store. Returns aggregate
// LinkStats plus computed graph hashes so the caller's link phase
// reuses them. Hides 30-200ms of GVS reflinks behind the in-flight
// download tail. Non-GVS installs drain rx as a no-op consumer.
pub(super) async fn run_gvs_prewarm_materializer(
    inputs: GvsPrewarmInputs,
    materialize_rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> miette::Result<(
    aube_linker::LinkStats,
    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
)> {
    let GvsPrewarmInputs {
        graph,
        store,
        cwd,
        virtual_store_dir_max_length,
        link_strategy,
        link_concurrency,
        patches,
        patch_hashes,
        node_version,
        build_policy,
        use_global_virtual_store_override,
    } = inputs;

    let engine = node_version
        .as_deref()
        .map(aube_lockfile::graph_hash::engine_name_default);

    // Build a probe linker without graph_hashes to check GVS mode
    // first. compute_graph_hashes_with_patches walks every package
    // BLAKE3-style, expensive on huge graphs. Skip it when GVS is
    // off so per-project installs and cold CI (CI=true gates GVS)
    // don't pay for hashes nothing reads.
    let mut probe = aube_linker::Linker::new(store.as_ref(), link_strategy)
        .with_virtual_store_dir_max_length(virtual_store_dir_max_length);
    if let Some(enabled) = use_global_virtual_store_override {
        probe = probe.with_use_global_virtual_store(enabled);
    }
    if !probe.uses_global_virtual_store() {
        return run_aube_dir_materializer(probe, graph, cwd, link_concurrency, materialize_rx)
            .await;
    }

    // Hash compute walks every package BLAKE3-style. spawn_blocking
    // pushes it off the tokio worker so the canonical_to_contextualized
    // build below + nested_link_targets walk + first materialize_rx
    // recv keep making progress in parallel. compute_graph_hashes_with_patches
    // is CPU-bound and was previously blocking the executor.
    let graph_for_hash = graph.clone();
    let build_policy_for_hash = build_policy.clone();
    let engine_for_hash = engine.clone();
    let patch_hashes_for_hash = patch_hashes.clone();
    aube_util::diag::instant(
        aube_util::diag::Category::Materialize,
        "hash_compute_spawn",
        None,
    );
    let hash_handle = tokio::task::spawn_blocking(move || {
        let _diag = aube_util::diag::Span::new(
            aube_util::diag::Category::Materialize,
            "graph_hash_compute",
        );
        let allow = |name: &str, version: &str| {
            matches!(
                build_policy_for_hash.decide(name, version),
                aube_scripts::AllowDecision::Allow
            )
        };
        let patch_hash_fn = |name: &str, version: &str| -> Option<String> {
            let key = format!("{name}@{version}");
            patch_hashes_for_hash.get(&key).cloned()
        };
        aube_lockfile::graph_hash::compute_graph_hashes_with_patches(
            &graph_for_hash,
            &allow,
            engine_for_hash.as_ref(),
            &patch_hash_fn,
        )
    });

    let nested_link_targets =
        aube_linker::build_nested_link_targets(&cwd, &graph).map(std::sync::Arc::new);

    // Channel emits `pkg.dep_path` (canonical on resolver first-pass,
    // contextualized on post-pass). When the received key is canonical
    // and fans out to one-or-more peer-contextualized variants in the
    // graph, this map points canonical -> {contextualized}. Identity
    // entries (canonical == dep_path) skip the map and fall back to a
    // direct graph lookup at receive time.
    let mut canonical_to_contextualized: aube_util::collections::FxMap<
        String,
        aube_util::collections::FxSet<String>,
    > = aube_util::collections::FxMap::default();
    for (dep_path, pkg) in &graph.packages {
        if pkg.local_source.is_some() {
            continue;
        }
        let canonical = pkg.spec_key();
        if canonical != *dep_path {
            canonical_to_contextualized
                .entry(canonical)
                .or_default()
                .insert(dep_path.clone());
        }
    }

    let _diag_hash_wait =
        aube_util::diag::Span::new(aube_util::diag::Category::Materialize, "hash_await");
    let graph_hashes = hash_handle
        .await
        .into_diagnostic()
        .wrap_err("graph_hash compute task failed")?;
    drop(_diag_hash_wait);
    aube_util::diag::instant(
        aube_util::diag::Category::Materialize,
        "drain_rx_begin",
        None,
    );
    let graph_hashes_arc = std::sync::Arc::new(graph_hashes);
    let mut linker = probe.with_graph_hashes((*graph_hashes_arc).clone());
    if !patches.is_empty() {
        linker = linker.with_patches(patches);
    }
    let linker = std::sync::Arc::new(linker);

    /*
     * Adaptive linker parallelism. The signal is the same as the
     * network limiter for ceiling/throttle behavior, but with
     * CUSUM-driven shrink disabled. Per-package
     * `ensure_in_virtual_store` wall on storage-bound workloads
     * has high intrinsic variance (Defender scans, NTFS
     * cold-cache, COW reflink fall-through to copy) that has no
     * upstream queue to relieve — treating rising RTT as
     * backpressure was observed to collapse the limit from seed
     * 16 down to 12 on Windows, queueing 1195 packages behind a
     * 12-permit cap (mean 2456ms permit_wait).
     *
     * `record_throttle` shrink remains active for real IO errors.
     * `link_concurrency` setting is a *seed* (when set); default
     * seed clamps `default_linker_parallelism()` into [16, 48].
     * Floor 8 prevents pathological collapse under throttle
     * cascades; ceiling 64 caps concurrent open file descriptors.
     */
    let permit_seed = link_concurrency.unwrap_or_else(aube_linker::default_linker_parallelism);
    let linker_persistent = aube_util::adaptive::global_persistent_state();
    let sem = match linker_persistent.as_ref() {
        Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
            state,
            "linker_prewarm:default",
            permit_seed.clamp(16, 48),
            8,
            64,
        ),
        None => aube_util::adaptive::AdaptiveLimit::new(permit_seed.clamp(16, 48), 8, 64),
    };
    sem.disable_cusum_shrink();
    let linker_sem_for_persist = std::sync::Arc::clone(&sem);
    let linker_persistent_for_save = linker_persistent.clone();
    let mut in_flight: Vec<tokio::task::JoinHandle<miette::Result<aube_linker::LinkStats>>> =
        Vec::new();
    let mut rx = materialize_rx;
    while let Some((key, index)) = rx.recv().await {
        // canonical_to_contextualized only stores entries where
        // canonical != dep_path. Identity packages (the common case)
        // fall through to a direct graph lookup. Without this fallback
        // the lockfile path — which emits dep_path == canonical for
        // every non-peer-suffixed package — would silently skip
        // materialize for every identity entry.
        let dep_paths: Vec<String> = if let Some(set) = canonical_to_contextualized.get(&key) {
            set.iter().cloned().collect()
        } else if graph.packages.contains_key(&key) {
            vec![key.clone()]
        } else {
            continue;
        };
        let index = std::sync::Arc::new(index);
        for dep_path in dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path).cloned() else {
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            let linker = linker.clone();
            let sem = sem.clone();
            let index = index.clone();
            let nested_link_targets = nested_link_targets.clone();
            in_flight.push(tokio::spawn(async move {
                let _diag_pkg =
                    aube_util::diag::Span::new(aube_util::diag::Category::Materialize, "package")
                        .with_meta_fn(|| {
                            format!(r#"{{"dep_path":{}}}"#, aube_util::diag::jstr(&dep_path))
                        });
                let _diag_pkg_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Imp);
                let permit_wait = std::time::Instant::now();
                let permit = sem.acquire().await;
                let permit_wait_ms = permit_wait.elapsed();
                if permit_wait_ms.as_millis() > 1 {
                    aube_util::diag::event_lazy(
                        aube_util::diag::Category::Materialize,
                        "permit_wait",
                        permit_wait_ms,
                        || format!(r#"{{"dep_path":{}}}"#, aube_util::diag::jstr(&dep_path)),
                    );
                }
                let dep_path_for_err = dep_path.clone();
                let outcome = tokio::task::spawn_blocking(move || -> miette::Result<_> {
                    let _diag_blk = aube_util::diag::Span::new(
                        aube_util::diag::Category::Materialize,
                        "package_blocking",
                    );
                    let mut stats = aube_linker::LinkStats::default();
                    linker
                        .ensure_in_virtual_store(
                            &dep_path,
                            &pkg,
                            &index,
                            &mut stats,
                            nested_link_targets.as_deref(),
                        )
                        .map_err(|e| miette!("prewarm GVS for {dep_path_for_err}: {e}"))?;
                    Ok(stats)
                })
                .await
                .into_diagnostic()?;
                match &outcome {
                    Ok(_) => permit.record_success(),
                    Err(_) => permit.record_cancelled(),
                }
                outcome
            }));
        }
    }
    let mut total = aube_linker::LinkStats::default();
    for handle in in_flight {
        let s = handle.await.into_diagnostic()??;
        total.packages_linked += s.packages_linked;
        total.packages_cached += s.packages_cached;
        total.files_linked += s.files_linked;
    }
    if let Some(state) = linker_persistent_for_save.as_ref() {
        linker_sem_for_persist.persist(state, "linker_prewarm:default");
    }
    Ok((total, Some(graph_hashes_arc)))
}

/// Per-project materializer: pipelines the link work into the fetch
/// phase under non-GVS mode. Each (canonical_key, PackageIndex) the
/// fetch coordinator emits triggers a `materialize_into` against the
/// per-project `.aube/<dep_path>/`, so by the time fetch finishes
/// the dedicated link phase only has to create the top-level
/// `node_modules/<name>` symlinks.
async fn run_aube_dir_materializer(
    linker: aube_linker::Linker,
    graph: std::sync::Arc<aube_lockfile::LockfileGraph>,
    cwd: std::path::PathBuf,
    link_concurrency: Option<usize>,
    materialize_rx: tokio::sync::mpsc::Receiver<(String, aube_store::PackageIndex)>,
) -> miette::Result<(
    aube_linker::LinkStats,
    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
)> {
    let aube_dir = std::sync::Arc::new(linker.aube_dir_for(&cwd));
    aube_linker::mkdirp(&aube_dir).map_err(|e| miette!("create {}: {e}", aube_dir.display()))?;
    let nested_link_targets =
        aube_linker::build_nested_link_targets(&cwd, &graph).map(std::sync::Arc::new);

    // Channel emits `pkg.dep_path` (canonical on the resolver's
    // first-pass packages, contextualized on post-pass). When the
    // received key is a canonical that fans out to one-or-more
    // peer-contextualized variants in the graph, this map points
    // canonical -> {contextualized dep_paths}. Identity entries
    // (canonical == dep_path) are skipped because the receive loop
    // falls back to a direct graph lookup for those.
    let mut canonical_to_contextualized: aube_util::collections::FxMap<
        String,
        aube_util::collections::FxSet<String>,
    > = aube_util::collections::FxMap::default();
    for (dep_path, pkg) in &graph.packages {
        if pkg.local_source.is_some() {
            continue;
        }
        let canonical = pkg.spec_key();
        if canonical != *dep_path {
            canonical_to_contextualized
                .entry(canonical)
                .or_default()
                .insert(dep_path.clone());
        }
    }

    let linker = std::sync::Arc::new(linker);
    /*
     * Adaptive per-project materialize parallelism. Same gradient
     * controller as the prewarm path, with CUSUM-driven shrink
     * disabled for the same reason: per-package `ensure_in_aube_dir`
     * wall is filesystem-bound (Defender, NTFS cold-cache, COW
     * fall-through) and rising RTT here is intrinsic noise rather
     * than upstream backpressure. `record_throttle` shrink remains
     * active for real IO errors. Floor 8 prevents pathological
     * collapse under throttle cascades; persisted under
     * `linker_per_project:default` so the next process resumes
     * the converged operating point.
     */
    let permit_seed = link_concurrency.unwrap_or_else(aube_linker::default_linker_parallelism);
    let perproj_persistent = aube_util::adaptive::global_persistent_state();
    let sem = match perproj_persistent.as_ref() {
        Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
            state,
            "linker_per_project:default",
            permit_seed.clamp(16, 48),
            8,
            64,
        ),
        None => aube_util::adaptive::AdaptiveLimit::new(permit_seed.clamp(16, 48), 8, 64),
    };
    sem.disable_cusum_shrink();
    let perproj_sem_for_persist = std::sync::Arc::clone(&sem);
    let perproj_persistent_for_save = perproj_persistent.clone();
    // JoinSet aborts in-flight tasks if we early-return on error,
    // so a failed materialize doesn't leave orphan tasks racing
    // disk writes against the install driver's cleanup.
    let mut in_flight: tokio::task::JoinSet<miette::Result<aube_linker::LinkStats>> =
        tokio::task::JoinSet::new();
    let mut total = aube_linker::LinkStats::default();
    let mut rx = materialize_rx;
    while let Some((key, index)) = rx.recv().await {
        // Surface task failures while still receiving so a sustained
        // I/O error aborts before we queue hundreds more.
        while let Some(joined) = in_flight.try_join_next() {
            let s = joined.into_diagnostic()??;
            total.packages_linked += s.packages_linked;
            total.packages_cached += s.packages_cached;
            total.files_linked += s.files_linked;
        }

        let dep_paths: Vec<String> = if let Some(set) = canonical_to_contextualized.get(&key) {
            set.iter().cloned().collect()
        } else if graph.packages.contains_key(&key) {
            vec![key.clone()]
        } else {
            continue;
        };
        let index = std::sync::Arc::new(index);
        for dep_path in dep_paths {
            let Some(pkg) = graph.packages.get(&dep_path).cloned() else {
                continue;
            };
            if pkg.local_source.is_some() {
                continue;
            }
            let linker = linker.clone();
            let sem = sem.clone();
            let index = index.clone();
            let aube_dir = aube_dir.clone();
            let nested_link_targets = nested_link_targets.clone();
            in_flight.spawn(async move {
                let permit = sem.acquire().await;
                let dep_path_for_err = dep_path.clone();
                let outcome = tokio::task::spawn_blocking(move || -> miette::Result<_> {
                    let mut stats = aube_linker::LinkStats::default();
                    linker
                        .ensure_in_aube_dir(
                            &aube_dir,
                            &dep_path,
                            &pkg,
                            &index,
                            &mut stats,
                            nested_link_targets.as_deref(),
                        )
                        .map_err(|e| miette!("materialize {dep_path_for_err}: {e}"))?;
                    Ok(stats)
                })
                .await
                .into_diagnostic()?;
                match &outcome {
                    Ok(_) => permit.record_success(),
                    Err(_) => permit.record_cancelled(),
                }
                outcome
            });
        }
    }
    while let Some(joined) = in_flight.join_next().await {
        let s = joined.into_diagnostic()??;
        total.packages_linked += s.packages_linked;
        total.packages_cached += s.packages_cached;
        total.files_linked += s.files_linked;
    }
    if let Some(state) = perproj_persistent_for_save.as_ref() {
        perproj_sem_for_persist.persist(state, "linker_per_project:default");
    }
    Ok((total, None))
}

// `network_concurrency`: override for the tarball-fetch semaphore.
//   `None` uses the built-in default (128). Surfaced so the
//   `networkConcurrency` setting, resolved once at the install-run
//   entry point, can cap parallel downloads.
// `verify_integrity`: whether to verify each tarball's SHA-512 against
//   its lockfile integrity before importing into the store. `false`
//   skips the check entirely; corresponds to `verifyStoreIntegrity=false`.
// `strict_pkg_content_check`: whether to validate that the imported
//   tarball's `package.json` advertises the same (name, version) the
//   resolver requested. `true` (pnpm default) rejects mismatches before
//   linking; corresponds to `strictStorePkgContentCheck=true`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_packages_with_root<F>(
    packages: &BTreeMap<String, aube_lockfile::LockedPackage>,
    store: &std::sync::Arc<aube_store::Store>,
    client: F,
    progress: Option<&InstallProgress>,
    project_root: &std::path::Path,
    aube_dir: &std::path::Path,
    // Some streams every successful (dep_path, index) so a concurrent
    // GVS-prewarm materializer can start reflinks before the full
    // batch finishes. None keeps batch-then-return for `aube fetch`.
    // Sender drops on function exit so consumer sees channel close.
    materialize_tx: Option<tokio::sync::mpsc::Sender<(String, aube_store::PackageIndex)>>,
    // When true, every package classifies as `Cached` or `NeedsFetch`
    // based on `store.load_index`, regardless of whether
    // `.aube/<dep>` already exists on disk. Callers pass true when
    // either:
    //
    //   - the linker will wipe `node_modules/` before running
    //     (`link_workspace`), so the `AlreadyLinked` classification
    //     would be immediately invalidated; or
    //   - the caller needs `load_index` to actually run as its store
    //     verification step (`aube fetch`, which treats the act of
    //     walking the store-file existence check as the operation's
    //     primary side effect).
    //
    // Both cases share the same implementation: skip the `.aube/`
    // existence check entirely so every package goes through
    // `store.load_index` → either `Cached` (store has it) or
    // `NeedsFetch` (store is missing the file, download fresh).
    skip_already_linked_shortcut: bool,
    virtual_store_dir_max_length: usize,
    ignore_scripts: bool,
    network_concurrency: Option<usize>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
    git_prepare_depth: u32,
    inherited_build_policy: Option<std::sync::Arc<aube_scripts::BuildPolicy>>,
    git_shallow_hosts: Vec<String>,
) -> miette::Result<(BTreeMap<String, aube_store::PackageIndex>, usize, usize)>
where
    F: FnOnce() -> std::sync::Arc<aube_registry::client::RegistryClient>,
{
    // No-op fast path: for every package whose per-project
    // `node_modules/.aube/<dep_path>` entry already resolves to an
    // existing target, skip the package-index load entirely. The
    // linker's only consumer of a `PackageIndex` is
    // `materialize_into` — if the package is already materialized
    // (either as a real directory here in per-project mode, or as a
    // symlink into the global virtual store that itself exists),
    // there's nothing to materialize and the 13–15 KB JSON on disk at
    // `~/.cache/aube/index/<name>@<ver>.json` would be read for
    // nothing. A fresh no-op install against the 1.4k-package medium
    // fixture drops from ~38 ms of parallel index reads to a handful
    // of `stat(2)`s.
    //
    // Two call sites disable the fast path entirely via
    // `skip_already_linked_shortcut=true`:
    //
    //   - **Workspace installs.** `link_workspace` unconditionally
    //     wipes `node_modules/` (including `.aube/`) before
    //     rebuilding, so every `AlreadyLinked` classification would
    //     be invalidated by the time the linker runs. With the fast
    //     path enabled, the linker would then fall back to
    //     `self.store.load_index` *serially* inside `link_workspace`'s
    //     for-loop, which is strictly slower than loading them here
    //     in parallel via rayon.
    //
    //   - **`aube fetch`.** The command exists to populate the
    //     global store (typical use: Docker layer caching, warming
    //     a CI mirror, or recovering from a wiped aube store).
    //     If `node_modules/.aube/<dep>` happens to exist from a
    //     previous install, the `AlreadyLinked` shortcut would skip
    //     both `load_index` and the tarball fetch — which silently
    //     leaves the store empty even though the user explicitly
    //     asked for it to be repopulated. Disabling the shortcut
    //     makes every package flow through `store.load_index`,
    //     which does a first-file existence check on the CAS and
    //     correctly downgrades to `NeedsFetch` when the store entry
    //     has been wiped.
    //
    // `Path::exists` follows symlinks, so a per-project entry pointing
    // at a global virtual-store target that no longer exists correctly
    // falls through to the slow path. The linker re-derives the entry
    // name through `aube_dir_entry_name(dep_path)`, which is just
    // `dep_path_to_filename(dep_path, max_length)` — we take the max
    // length as a parameter (instead of reaching for
    // `DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH`) so the fast path checks
    // the exact same filename the linker will write. The install
    // driver (and the `aube fetch` wrapper) both resolve this through
    // `super::resolve_virtual_store_dir_max_length(&ctx)` so user
    // overrides of `virtualStoreDirMaxLength` flow to both sites and
    // we can't produce "the fast path saw `.aube/<X>` but the linker
    // expected `.aube/<Y>`" bugs on dep_paths long enough to trigger
    // the truncate-and-hash fallback inside `dep_path_to_filename`.
    // `aube_dir` is threaded in from
    // `commands::resolve_virtual_store_dir` so custom `virtualStoreDir`
    // values land on the same path the linker will write to.

    enum CheckResult {
        /// Already linked into `node_modules/.aube/<dep_path>`. The
        /// linker won't need the package index for this dep at all.
        AlreadyLinked,
        /// Store has the index; linker will reuse it to (re)create any
        /// missing symlinks.
        Cached(aube_store::PackageIndex),
        /// Missing from the store — falls through to the tarball fetch.
        NeedsFetch,
    }

    // Parallel index check (rayon)
    let check_results: Vec<_> = packages
        .par_iter()
        .filter(|(_, pkg)| pkg.local_source.is_none())
        .map(|(dep_path, pkg)| {
            if !skip_already_linked_shortcut {
                let entry_name = dep_path_to_filename(dep_path, virtual_store_dir_max_length);
                if aube_dir.join(&entry_name).exists() {
                    return (dep_path.clone(), pkg, CheckResult::AlreadyLinked);
                }
            }
            // Keyed by registry name so two npm-aliases of the same
            // real package share one store index entry instead of
            // wastefully double-fetching under the alias. Integrity
            // is part of the cache key so a different tarball served
            // under the same (name, version) — e.g. a github codeload
            // archive vs. the npm-published bytes — can't return the
            // wrong file list.
            match store.load_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref()) {
                Some(index) => (dep_path.clone(), pkg, CheckResult::Cached(index)),
                None => (dep_path.clone(), pkg, CheckResult::NeedsFetch),
            }
        })
        .collect();

    let mut indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();

    // Remote tarball deps need a registry client to download the
    // bits during `import_local_source`. Build it eagerly when any
    // package has a RemoteTarball source so the local-import loop
    // can share a single reqwest client with the fetch branch
    // below. Projects without URL tarballs still get the lazy
    // construction path in the `to_fetch` branch.
    let has_remote_tarball = packages.values().any(|p| {
        matches!(
            p.local_source,
            Some(aube_lockfile::LocalSource::RemoteTarball(_))
        )
    });
    let mut client_slot: Option<std::sync::Arc<aube_registry::client::RegistryClient>> = None;
    let mut client_builder = Some(client);
    if has_remote_tarball {
        client_slot = Some((client_builder.take().unwrap())());
    }

    // Local (`file:` / `link:`) packages: import directories or
    // tarballs straight into the store so the linker has a
    // PackageIndex to walk. Link-only deps don't get an index.
    for (dep_path, pkg) in packages {
        let Some(ref local) = pkg.local_source else {
            continue;
        };
        // Credit every local dep against the overall progress total —
        // the total was seeded with `graph.packages.len()`, which
        // includes `link:` packages even though they have no
        // store-backed index. Skipping the `inc` for `None` would
        // stall the bar below 100% for any project with a link dep.
        if let Some(index) = import_local_source(
            store,
            project_root,
            local,
            client_slot.as_ref(),
            ignore_scripts,
            git_prepare_depth,
            inherited_build_policy.clone(),
            &git_shallow_hosts,
            &pkg.name,
            &pkg.version,
        )
        .await?
        {
            indices.insert(dep_path.clone(), index);
        }
        if let Some(p) = progress {
            p.inc_reused(1);
        }
    }

    // Cap by check_results upper bound. Worst case fits in one alloc.
    let mut to_fetch = Vec::with_capacity(check_results.len());
    let mut cached_count = 0usize;

    for (dep_path, pkg, result) in check_results {
        match result {
            CheckResult::AlreadyLinked => {
                // No `indices` entry: the linker takes the
                // already-materialized fast path and never touches the
                // index map for this dep_path.
                cached_count += 1;
            }
            CheckResult::Cached(index) => {
                // Don't stream Cached items through the materializer.
                // The link phase fast-paths them via pkg_nm_dir.exists()
                // anyway, so the per-pkg spawn pair was pure overhead
                // on warm-cache installs (1400-pkg fixture saw +66%
                // wall time before the skip).
                indices.insert(dep_path, index);
                cached_count += 1;
            }
            CheckResult::NeedsFetch => {
                // `registry_name` is the real package name on the
                // registry — equal to `name` for the common case, and
                // the aliased-real-name for npm-alias entries. The
                // tarball URL override is only present for aliased
                // entries where `client.tarball_url(&name, ...)` would
                // 404 the alias-qualified name; the lockfile reader
                // populated it from `resolved:` at parse time.
                to_fetch.push((
                    dep_path,
                    pkg.name.clone(),
                    pkg.registry_name().to_string(),
                    pkg.version.clone(),
                    pkg.tarball_url.clone(),
                    pkg.integrity.clone(),
                ));
            }
        }
    }

    // Credit cached packages against the overall counter immediately — only
    // the to_fetch set produces visible child rows.
    if let Some(p) = progress {
        p.inc_reused(cached_count);
    }

    // Critical-path fetch order: float likely-native-build packages
    // (sharp, esbuild, @swc/*, sqlite3, lmdb, bcrypt, etc) to the
    // front of the queue. These packages run prebuild/node-gyp at
    // install time, and starting their fetch first lets the build
    // step pipeline with subsequent fetches instead of blocking on
    // the tail. `sort_by_key` is stable so non-native packages keep
    // their lockfile-discovery order; only the natives jump ahead.
    // `AUBE_DISABLE_CRITICAL_PATH=1` reverts to the previous order
    // for byte-identity comparison runs.
    if std::env::var_os("AUBE_DISABLE_CRITICAL_PATH").is_none() {
        to_fetch
            .sort_by_key(|(_, _, registry_name, _, _, _)| !is_likely_native_build(registry_name));
    }
    let fetch_count = to_fetch.len();

    let mut lockfile_persist_handle: Option<(
        std::sync::Arc<aube_util::adaptive::PersistentState>,
        std::sync::Arc<aube_util::adaptive::AdaptiveLimit>,
    )> = None;

    if !to_fetch.is_empty() {
        // Only build the reqwest+TLS client now that we know we
        // actually need to fetch tarballs. On a warm no-op install
        // everything classifies as `AlreadyLinked` / `Cached` and this
        // closure is never called — the previous eager construction
        // cost ~22 ms on Linux just to create a client that never
        // sent a single request.
        let client = match client_slot.take() {
            Some(c) => c,
            None => (client_builder.take().unwrap())(),
        };
        /*
         * Adaptive concurrency on the lockfile driven fetch path
         * (frozen / fetch / ci / matched lockfile). Same gradient
         * controller as the streaming resolver fetch path.
         * `networkConcurrency` setting acts as the seed when set.
         * Cross run persisted under `tarball:default` so this path
         * shares its converged operating point with the streaming
         * tarball path.
         */
        let sem_seed = network_concurrency.unwrap_or_else(default_lockfile_network_concurrency);
        let lockfile_persistent = aube_util::adaptive::global_persistent_state();
        let semaphore = match lockfile_persistent.as_ref() {
            Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
                state,
                "tarball:default",
                sem_seed.clamp(64, 128),
                4,
                256,
            ),
            None => aube_util::adaptive::AdaptiveLimit::new(sem_seed.clamp(64, 128), 4, 256),
        };
        if let Some(state) = lockfile_persistent.clone() {
            lockfile_persist_handle = Some((state, std::sync::Arc::clone(&semaphore)));
        }
        // Hoist env-driven flags out of the per-tarball closure so
        // the libc lock fires once instead of N times on a 1000-pkg
        // install.
        let streaming_sha512_enabled = std::env::var_os("AUBE_DISABLE_STREAMING_SHA512").is_none();
        let tarball_stream_enabled = std::env::var_os("AUBE_DISABLE_TARBALL_STREAM").is_none();
        // JoinSet so a first-error path aborts the sibling fetches
        // instead of detaching them into the background. Detached
        // tasks keep writing to the CAS after the install command
        // has already errored out.
        let mut handles: tokio::task::JoinSet<miette::Result<(String, aube_store::PackageIndex)>> =
            tokio::task::JoinSet::new();

        for (dep_path, display_name, registry_name, version, tarball_url_override, integrity) in
            to_fetch
        {
            let sem = semaphore.clone();
            let store = store.clone();
            let client = client.clone();
            let row = progress.map(|p| p.start_fetch(&display_name, &version));
            let bytes_progress = progress.cloned();

            handles.spawn(async move {
                let _row = row;
                let task_start = std::time::Instant::now();
                let permit = sem.acquire().await;
                let wait_time = task_start.elapsed();
                // Aliased entries (`"h3-v2": "npm:h3@..."`) carry the
                // resolved tarball URL verbatim from the lockfile so
                // we skip re-deriving it from `registry_name` — the
                // lockfile captured the exact URL at write time
                // against whatever registry was active then.
                let url = tarball_url_override
                    .clone()
                    .unwrap_or_else(|| client.tarball_url(&registry_name, &version));

                let dl_start = std::time::Instant::now();

                // Stream when env enabled and SRI is SHA-512 (or
                // absent). Streaming verify can't re-hash with
                // another algo, so non-SHA-512 takes the buffered
                // path below.
                let stream_eligible = tarball_stream_enabled
                    && integrity
                        .as_deref()
                        .is_none_or(|s| s.starts_with("sha512-"));
                if stream_eligible {
                    let streamed = crate::commands::install::lifecycle::fetch_and_import_tarball_streaming(
                        &client,
                        &store,
                        &url,
                        &display_name,
                        &registry_name,
                        &version,
                        integrity.as_deref(),
                        verify_integrity,
                        strict_integrity,
                        strict_pkg_content_check,
                    )
                    .await;
                    let (index, bytes_len) = match streamed {
                        Ok(v) => {
                            permit.record_success();
                            v
                        }
                        Err(e) => {
                            if e.is_throttle {
                                permit.record_throttle();
                            } else {
                                permit.record_cancelled();
                            }
                            return Err(e.into());
                        }
                    };
                    let dl_time = dl_start.elapsed();
                    if let Some(p) = bytes_progress.as_ref() {
                        p.inc_downloaded_bytes(bytes_len);
                    }
                    tracing::trace!(
                        "fetch (stream) {display_name}@{version}: wait={:.0?} total={:.0?} ({} bytes)",
                        wait_time,
                        dl_time,
                        bytes_len
                    );
                    return Ok::<_, miette::Report>((dep_path, index));
                }

                // Buffered SHA-512 path. Streaming SHA-512 hashes
                // chunks during the read loop, so import_verified
                // skips its hash pass and compares directly.
                // AUBE_DISABLE_STREAMING_SHA512=1 reverts to the
                // buffered-then-hash path.
                let fetch_outcome = if streaming_sha512_enabled {
                    client
                        .fetch_tarball_bytes_streaming_sha512(&url)
                        .await
                        .map(|(b, d)| (b, Some(d)))
                        .map_err(|e| {
                            let throttled = e.is_throttle();
                            (
                                miette!(
                                    "failed to fetch {display_name}@{version}: {e}{}",
                                    crate::dep_chain::format_chain_for(&display_name, &version)
                                ),
                                throttled,
                            )
                        })
                } else {
                    client.fetch_tarball_bytes(&url).await.map(|b| (b, None)).map_err(|e| {
                        let throttled = e.is_throttle();
                        (
                            miette!(
                                "failed to fetch {display_name}@{version}: {e}{}",
                                crate::dep_chain::format_chain_for(&display_name, &version)
                            ),
                            throttled,
                        )
                    })
                };
                let (bytes, streamed_digest) = match fetch_outcome {
                    Ok(v) => {
                        permit.record_success();
                        v
                    }
                    Err((report, throttled)) => {
                        if throttled {
                            permit.record_throttle();
                        } else {
                            permit.record_cancelled();
                        }
                        return Err(report);
                    }
                };
                let dl_time = dl_start.elapsed();

                if let Some(p) = bytes_progress.as_ref() {
                    p.inc_downloaded_bytes(bytes.len() as u64);
                }

                // Keep the semaphore permit through import, not just
                // download. `import_tarball` fans out into gzip/tar
                // decode, SHA-512, CAS writes, and index writes; on
                // macOS/APFS, letting hundreds of completed downloads
                // pile into Tokio's large blocking pool turns the
                // cold-cache path into metadata contention. The
                // semaphore is therefore the install-wide "download +
                // import" pressure valve: enough concurrency to keep
                // the network busy, but not enough to swamp the
                // filesystem.
                //
                // Move CPU/blocking work (SHA-512 verify, tar extract,
                // file writes, index cache write) onto the blocking
                // thread pool so it doesn't starve the async runtime
                // workers used for concurrent network I/O.
                let bytes_len = bytes.len();
                let (index, import_time) = run_import_on_blocking(
                    store.clone(),
                    bytes,
                    streamed_digest,
                    display_name.clone(),
                    registry_name.clone(),
                    version.clone(),
                    integrity.clone(),
                    verify_integrity,
                    strict_integrity,
                    strict_pkg_content_check,
                )
                .await?;

                tracing::trace!(
                    "fetch {display_name}@{version}: wait={:.0?} dl={:.0?} ({} bytes) import={:.0?}",
                    wait_time,
                    dl_time,
                    bytes_len,
                    import_time
                );

                Ok::<_, miette::Report>((dep_path, index))
            });
        }

        while let Some(joined) = handles.join_next().await {
            let (dep_path, index) = joined.into_diagnostic()??;
            if let Some(tx) = materialize_tx.as_ref() {
                // Time channel send so back-pressure events show up in
                // the trace. The materialize channel is bounded; if the
                // consumer falls behind, `send().await` blocks until a
                // permit frees, which is otherwise invisible in
                // `fetch.tarball` totals.
                let send_t0 = aube_util::diag::enabled().then(std::time::Instant::now);
                tx.send((dep_path.clone(), index.clone()))
                    .await
                    .map_err(|_| {
                        miette!("materializer task exited before fetch_packages finished")
                    })?;
                if let Some(t0) = send_t0 {
                    let elapsed = t0.elapsed();
                    if elapsed.as_millis() >= 1 {
                        aube_util::diag::event(
                            aube_util::diag::Category::Channel,
                            "materialize_send_wait",
                            elapsed,
                            None,
                        );
                    }
                }
            }
            indices.insert(dep_path, index);
        }
    }

    // Without explicit drop, consumer's rx.recv() loop hangs.
    drop(materialize_tx);

    if let Some((state, sem)) = lockfile_persist_handle {
        sem.persist(&state, "tarball:default");
    }

    Ok((indices, cached_count, fetch_count))
}

/// Heuristic: is this registry name likely to run a native build
/// (`node-gyp`, `prebuild-install`, `cmake-js`, `@napi-rs/cli`) at
/// install time? Used by the critical-path fetch reorder to float
/// these packages to the front of the download queue so their build
/// step can pipeline with the remaining tarball downloads.
///
/// Curated allowlist: covers the long-tail native packages that
/// dominate cold-install wall time on a 1000-pkg graph. False
/// negatives are fine (the package fetches in normal order); false
/// positives are also fine (the package fetches earlier, no harm).
/// Update this list when bench data shows a new heavy native dep.
fn is_likely_native_build(registry_name: &str) -> bool {
    // Exact-match heavy hitters. `ws` and `sass` deliberately
    // omitted: pure-JS by default; only `node-sass` is the
    // deprecated native build.
    const EXACT: &[&str] = &[
        "sharp",
        "esbuild",
        "fsevents",
        "canvas",
        "bcrypt",
        "node-sass",
        "sqlite3",
        "better-sqlite3",
        "lmdb",
        "msgpackr-extract",
        "sodium-native",
        "node-gyp",
        "prebuild-install",
        "node-gyp-build",
    ];
    if EXACT.contains(&registry_name) {
        return true;
    }
    // Scoped prefixes: `@swc/*`, `@parcel/*`, `@napi-rs/*`,
    // `@next/swc-*`, `@rollup/rollup-*`, `@esbuild/*` all ship
    // platform-specific native binaries.
    const PREFIXES: &[&str] = &[
        "@swc/",
        "@parcel/",
        "@napi-rs/",
        "@next/swc-",
        "@rollup/rollup-",
        "@esbuild/",
        "@playwright/",
        "@biomejs/",
    ];
    PREFIXES.iter().any(|p| registry_name.starts_with(p))
}

/// Pull the canonical version off a dep_path for display purposes. The
/// dep_path looks like `name@1.2.3(peer@x)` — we strip the `name@` prefix
/// and any peer suffix so the warning shows `1.2.3` not `1.2.3(peer@x)`.
pub(super) fn version_from_dep_path(dep_path: &str, name: &str) -> String {
    let tail = dep_path
        .strip_prefix(&format!("{name}@"))
        .unwrap_or(dep_path);
    tail.split('(').next().unwrap_or(tail).to_string()
}

/// Re-key a canonical-indexed indices map to match the peer-contextualized
/// dep_paths in `graph`. Each contextualized entry points at the same
/// underlying files as its canonical name@version, so we look each graph
/// entry up by canonical and clone the index — a no-op when canonical ==
/// contextualized (i.e. the package has no peer deps).
fn remap_indices_to_contextualized(
    canonical_indices: &BTreeMap<String, aube_store::PackageIndex>,
    graph: &aube_lockfile::LockfileGraph,
) -> BTreeMap<String, aube_store::PackageIndex> {
    let mut out = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        let canonical_key = pkg.spec_key();
        if let Some(idx) = canonical_indices
            .get(dep_path)
            .or_else(|| canonical_indices.get(&canonical_key))
        {
            out.insert(dep_path.clone(), idx.clone());
        }
    }
    out
}

pub async fn run(opts: InstallOptions) -> miette::Result<()> {
    let mode = opts.mode;
    let cwd = if let Some(project_dir) = &opts.project_dir {
        project_dir.clone()
    } else {
        // `workspace_or_project_root` gives us workspace-first
        // precedence: `aube install` from inside a workspace member
        // installs against the workspace root (not the member as a
        // standalone project), so members don't get their own
        // `aube-lock.yaml` / `.aube/` virtual store. Yaml-only roots
        // install with a synthesized empty manifest at the read site
        // below.
        crate::dirs::workspace_or_project_root()?
    };
    let _lock = super::take_project_lock(&cwd)?;
    let start = std::time::Instant::now();
    let mut phase_timings = InstallPhaseTimings::from_env();
    aube_util::diag::spawn_concurrency_sampler();
    aube_util::diag::instant(aube_util::diag::Category::Install, "begin", None);
    let _diag_install = aube_util::diag::Span::new(aube_util::diag::Category::Install, "total");

    // `--force`: wipe the auto-install state file so the freshness
    // check in `ensure_installed` can't short-circuit the next run,
    // and fall through to the normal resolve/link path (which
    // `into_options` has already flipped to `FrozenMode::No` when
    // no explicit frozen flag is set). Keeps node_modules in place —
    // the linker is idempotent, so the relink pass is fast.
    if opts.force {
        // Silent swallow lets a permission-denied or Windows-locked
        // sidecar survive. Next run reads it, matches, short-circuits.
        // remove_state already maps NotFound to Ok.
        state::remove_state(&cwd)
            .map_err(|e| miette!("--force: failed to remove install state: {e}"))?;
    }

    // `modulesCacheMaxAge` drives the orphan sweep that runs at the
    // end of every successful install. When users explicitly tune
    // this setting (e.g. `modulesCacheMaxAge=1` to force sweeping on
    // every run), the sweep is load-bearing — skipping the full
    // pipeline would leave planted orphans in place until a dep
    // change forced a re-install. The default (10080 min = 7 days)
    // is effectively a no-op on a state-matched warm install (no
    // orphans accumulate when deps are unchanged), so keep install
    // fast paths only when the setting is at its default.
    let modules_cache_sweep_default = super::with_settings_ctx(&cwd, |ctx| {
        aube_settings::resolved::modules_cache_max_age(ctx) == 10080
    });

    let missing_lockfile_restore_eligible = matches!(opts.mode, FrozenMode::No)
        && !opts.force
        && !opts.lockfile_only
        && !opts.dep_selection.is_filtered()
        && !opts.merge_git_branch_lockfiles
        && !opts.strict_no_lockfile
        && !opts.dangerously_allow_all_builds
        && opts.workspace_filter.is_empty()
        && modules_cache_sweep_default
        && state::restore_missing_lockfile_if_fresh(&cwd, &opts.cli_flags);

    if missing_lockfile_restore_eligible {
        emit_unreviewed_builds_warning(&state::read_state_unreviewed_builds(&cwd));
        print_already_up_to_date();
        return Ok(());
    }

    // Warm-path short-circuit: when the state file says the tree is
    // fresh and no flag demands a full re-run, skip the resolve → fetch
    // → link pipeline entirely and emit the same "Already up to date"
    // line the full path would print. Mirrors the check already wired
    // into `ensure_installed` (see `commands::mod.rs::ensure_installed`).
    // Gated so any flag that implies real work falls through to the
    // main pipeline.
    let warm_path_eligible = matches!(opts.mode, FrozenMode::Frozen | FrozenMode::Prefer)
        && !opts.force
        && !opts.lockfile_only
        && !opts.dep_selection.is_filtered()
        && !opts.merge_git_branch_lockfiles
        && !opts.strict_no_lockfile
        && !opts.dangerously_allow_all_builds
        && opts.workspace_filter.is_empty()
        && modules_cache_sweep_default
        && state::check_needs_install_with_flags(&cwd, &opts.cli_flags).is_none();

    if warm_path_eligible {
        // Gate on the same condition as `InstallProgress::try_new`:
        // line-oriented reporters (`--reporter=ndjson`, `--reporter=json`)
        // and text mode (`-v` / `--silent`) stay silent on no-op installs,
        // matching the full-path behavior where `prog_ref` is `None` and
        // `print_install_summary` is never called. `--silent` additionally
        // has its `SilentStderrGuard` redirect fd 2 to /dev/null, so this
        // check is belt-and-suspenders for `-v` and the JSON reporters.
        emit_unreviewed_builds_warning(&state::read_state_unreviewed_builds(&cwd));
        print_already_up_to_date();
        let _ = start;
        return Ok(());
    }

    // 1. Read package.json
    //
    // Yaml-only workspace roots (`pnpm-workspace.yaml` only, no root
    // `package.json`) install with a synthesized empty manifest so
    // every workspace member is installed without the root carrying
    // any deps or scripts itself. The synthesized manifest naturally
    // skips root lifecycle hooks, has no required-scripts to validate,
    // and threads through the rest of the pipeline as a manifest with
    // no direct deps would.
    let manifest = super::load_manifest_or_default(&cwd)?;
    let project_name = manifest.name.as_deref().unwrap_or("(unnamed)");

    // Pre-resolver packument prefetch. Reads `package.json` keys and
    // fires fire-and-forget GETs for every registry-shaped direct dep
    // before workspace yaml load, settings resolve, lockfile parse,
    // pnpmfile setup, and resolver construction run. By the time the
    // resolver pops its first task, the packument is in the on-disk
    // cache and the reqwest pool is hot. Honors `AUBE_DISABLE_PREFETCH=1`.
    prefetch::spawn_direct_dep_prefetch(&manifest, &cwd, opts.network_mode);

    // Load the workspace yaml *once* — both as the typed
    // `WorkspaceConfig` (used below for `allow_builds_raw` and
    // friends) and as a raw `BTreeMap` (used by
    // `aube_settings::resolved::*` for metadata-driven lookups).
    // Errors propagate here rather than silently defaulting later,
    // so a malformed workspace file surfaces before we start
    // resolving the dep graph. Also load `.npmrc` entries once so
    // the same borrow feeds both the resolve-time settings and the
    // later engine-check settings.
    let files = crate::commands::FileSources::load(&cwd);
    let (ws_config_shared, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    // Catalog discovery walks up for the workspace yaml and also pulls
    // from package.json's `workspaces.catalog` / `pnpm.catalog`, so
    // `aube install` run from a monorepo subpackage still sees the root
    // workspace's catalog. See `discover_catalogs` for the precedence
    // order.
    let workspace_catalogs = super::discover_catalogs(&cwd)?;
    let settings_ctx = files.ctx(&raw_workspace, &opts.env_snapshot, &opts.cli_flags);
    super::configure_script_settings(&settings_ctx);

    // `--lockfile-dir` / `lockfileDir`: relocate `aube-lock.yaml` to a
    // different directory than the project root. The project becomes
    // an importer keyed by its relative path from the lockfile dir.
    // Defaults to the project root → importer key `.` → back-compat
    // with every existing install. Multi-project shared lockfiles
    // (`pnpm-workspace.yaml`, `sharedWorkspaceLockfile`) are out of
    // scope here — see the read-side guard in
    // `parse_lockfile_dir_remapped`.
    //
    // Relative paths resolve against the project root, not cwd
    // (pnpm convention). Both sides are canonicalized so equality and
    // `pathdiff` work regardless of symlinks or `./project/..` style
    // inputs (`cwd` itself originates from `find_project_root`, which
    // doesn't canonicalize).
    let (lockfile_dir, lockfile_importer_key): (std::path::PathBuf, String) =
        match aube_settings::resolved::lockfile_dir(&settings_ctx) {
            Some(raw) => {
                let raw_path = std::path::Path::new(&raw);
                let resolved = if raw_path.is_absolute() {
                    raw_path.to_path_buf()
                } else {
                    cwd.join(raw_path)
                };
                // pnpm creates the lockfile directory on demand; mirror that
                // so users can point at a not-yet-materialized shared dir.
                std::fs::create_dir_all(&resolved)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("--lockfile-dir: {}", resolved.display()))?;
                let canon = std::fs::canonicalize(&resolved)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("--lockfile-dir: {}", resolved.display()))?;
                let canon_cwd = std::fs::canonicalize(&cwd).into_diagnostic()?;
                if canon == canon_cwd {
                    (cwd.clone(), ".".to_string())
                } else {
                    let key = pathdiff::diff_paths(&canon_cwd, &canon)
                        .map(|p| {
                            // Lockfile importer keys use forward slashes on every
                            // platform so committed lockfiles stay portable across
                            // Windows ↔ Unix CI.
                            let s = p.to_string_lossy().into_owned();
                            if std::path::MAIN_SEPARATOR == '/' {
                                s
                            } else {
                                s.replace(std::path::MAIN_SEPARATOR, "/")
                            }
                        })
                        .ok_or_else(|| {
                            miette!(
                                "lockfile-dir {} cannot be related to project {}",
                                canon.display(),
                                canon_cwd.display()
                            )
                        })?;
                    (canon, key)
                }
            }
            None => (cwd.clone(), ".".to_string()),
        };

    // Fail fast on multi-project shared lockfiles (see
    // `guard_against_foreign_importers`). The downstream lockfile-read
    // sites only fire on `Fix`/`Prefer`/`--lockfile-only` paths, so a
    // `--no-frozen-lockfile` install pointed at someone else's lockfile
    // dir would silently overwrite their entries — this guard moves
    // the check ahead of the resolver so it fires regardless of
    // FrozenMode. `NotFound` means we're the first project writing
    // here; that's exactly the supported case.
    if lockfile_importer_key != "." {
        match aube_lockfile::parse_lockfile(&lockfile_dir, &manifest) {
            Ok(graph) => {
                guard_against_foreign_importers(&lockfile_dir, &lockfile_importer_key, &graph)
                    .map_err(miette::Report::new)?;
            }
            Err(aube_lockfile::Error::NotFound(_)) => {}
            Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
        }
    }

    // `modulesDir` controls the project-level directory name that
    // holds the top-level `<name>` entries. Defaults to
    // `"node_modules"` — Node's own module resolution algorithm still
    // walks up looking for a literal `node_modules/`, so users who
    // change this need to point `NODE_PATH` at the new directory
    // themselves. Resolved once here and threaded into the linker,
    // scripts runner, and every command helper that touches the
    // project-level directory — the inner virtual-store paths
    // (`.aube/<dep>/node_modules/<name>`) keep the literal name that
    // Node requires when walking up from inside a package.
    //
    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    // `virtualStoreDir` controls the per-project `.aube/<dep>/node_modules/`
    // tree. Resolved once here and threaded into the linker (via
    // `with_aube_dir_override`), the engines check,
    // `fetch_packages_with_root`'s "already linked" fast path,
    // `materialized_pkg_dir`, and the orphan sweep — every read-side
    // and write-side caller needs to land on the same path so a user
    // who sets `virtualStoreDir` to a custom location still gets a
    // coherent install. Relative paths and `~` are expanded against
    // the project root inside `resolve_virtual_store_dir`; unset
    // values derive from `modulesDir` (matching pnpm's
    // `<modulesDir>/.pnpm` default).
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);

    // Whether this install reads or writes a lockfile. Defaults to
    // true (npm/pnpm parity). Set `lockfile=false` in `.npmrc` /
    // `pnpm-workspace.yaml` to run a pure resolver-driven install with
    // no `aube-lock.yaml` write — equivalent to `npm install
    // --no-package-lock`. Combined with `--lockfile-only` the two
    // options contradict, so we reject that combination up front.
    //
    // `--frozen-lockfile` (which sets `strict_no_lockfile=true`) is a
    // similar contradiction: "fail hard if the lockfile doesn't match"
    // makes no sense without a lockfile. Reject that too so the error
    // points at the actual conflict instead of falling through to the
    // generic "no lockfile found and --frozen-lockfile is set" path.
    let lockfile_enabled = aube_settings::resolved::lockfile(&settings_ctx);
    // `sharedWorkspaceLockfile=false` flips the workspace-install layout:
    // each member writes its own lockfile next to its `package.json`
    // instead of a single root lockfile recording every importer. Only
    // affects the lockfile *write* phase — the resolver still runs once
    // over the whole workspace so `workspace:*` deps resolve correctly.
    // The auto-install state file and frozen-lockfile fast path stay
    // anchored at the workspace root, so installs under this layout
    // re-resolve more eagerly than shared installs do.
    let shared_workspace_lockfile =
        aube_settings::resolved::shared_workspace_lockfile(&settings_ctx);
    // `enableModulesDir=false` is pnpm's persistent equivalent of
    // `--lockfile-only`: resolve + write the lockfile, but don't
    // populate `node_modules/` (no virtual store, no top-level
    // symlinks, no lifecycle scripts). We collapse it onto the
    // existing `lockfile_only` flag so every downstream branch stays
    // in one place.
    let modules_dir_enabled = aube_settings::resolved::enable_modules_dir(&settings_ctx);
    let lockfile_only_effective = opts.lockfile_only || !modules_dir_enabled;
    if !lockfile_enabled && opts.lockfile_only {
        return Err(miette!(
            "--lockfile-only is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    if !lockfile_enabled && !modules_dir_enabled {
        // Both resolved-side and link-side suppression active — there
        // is literally nothing to do. Error out so users see the
        // conflict instead of staring at a silent no-op install.
        return Err(miette!(
            "enableModulesDir=false is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    if !lockfile_enabled && opts.strict_no_lockfile {
        return Err(miette!(
            "--frozen-lockfile is incompatible with lockfile=false; \
             remove one or the other"
        ));
    }
    let lockfile_include_tarball_url =
        aube_settings::resolved::lockfile_include_tarball_url(&settings_ctx);
    tracing::debug!(
        "lockfile: enabled={lockfile_enabled}, include-tarball-url={lockfile_include_tarball_url}"
    );

    // Branch-lockfile merge — run *before* any lockfile parsing so the
    // normal read path picks up the merged `aube-lock.yaml`. Triggered
    // by either the `--merge-git-branch-lockfiles` flag (one-shot,
    // ignores patterns) or by the current git branch matching
    // `mergeGitBranchLockfilesBranchPattern`. Skipped when `lockfile`
    // is off, since there's nothing to merge into.
    if lockfile_enabled {
        let patterns =
            aube_settings::resolved::merge_git_branch_lockfiles_branch_pattern(&settings_ctx)
                .unwrap_or_default();
        let should_merge = opts.merge_git_branch_lockfiles
            || aube_lockfile::merge::current_branch_matches(&cwd, &patterns);
        if should_merge {
            match aube_lockfile::merge_branch_lockfiles(&cwd, &manifest) {
                Ok(report) => {
                    if !report.merged_files.is_empty() {
                        let filenames: Vec<String> = report
                            .merged_files
                            .iter()
                            .filter_map(|p| {
                                p.file_name()
                                    .and_then(|n| n.to_str())
                                    .map(|s| s.to_string())
                            })
                            .collect();
                        tracing::info!(
                            "merged {} branch lockfile(s) into aube-lock.yaml: {}",
                            report.merged_files.len(),
                            filenames.join(", ")
                        );
                        if !report.conflicts.is_empty() {
                            // Surface conflicts to the user, not just
                            // at warn level. Without this, branch
                            // lockfile merges silently dropped data:
                            // override divergences, catalog drift,
                            // importer pin mismatches, integrity
                            // differences. All logged at debug only.
                            // Users saw "N conflicts" with zero
                            // detail and no hint what lost. Dump
                            // each conflict on its own line through
                            // the progress-safe writer so the list
                            // does not smear the install bar.
                            crate::progress::safe_eprintln(&format!(
                                "warn: {} conflict(s) resolved during branch-lockfile merge:",
                                report.conflicts.len()
                            ));
                            for c in &report.conflicts {
                                crate::progress::safe_eprintln(&format!("warn:   {c}"));
                            }
                        }
                    } else {
                        tracing::debug!(
                            "branch-lockfile merge triggered but no aube-lock.*.yaml files were found"
                        );
                    }
                }
                Err(err) => {
                    return Err(miette!("failed to merge branch lockfiles: {err}"));
                }
            }
        }
    }

    // Resolve the install-wide networking / integrity knobs once up
    // front so every downstream fetch site (the lockfile path, the
    // streaming-resolver path, and the forthcoming `aube fetch`
    // bridge) reads the same values. `network_concurrency_setting`
    // stays `Option<usize>` so each site can apply the dynamic
    // built-in fallback when the setting is absent.
    //
    // `sideEffectsCache` controls whether allowlisted dependency
    // lifecycle scripts can reuse a previously-cached post-build
    // package directory. It still respects aube's security model:
    // packages that are not allowed by BuildPolicy never run scripts
    // and never populate the side-effects cache.
    let network_concurrency_setting = resolve_network_concurrency(&settings_ctx);
    let link_concurrency_setting = resolve_link_concurrency(&settings_ctx);
    let verify_store_integrity_setting = resolve_verify_store_integrity(&settings_ctx);
    let strict_store_integrity_setting = settings::resolve_strict_store_integrity(&settings_ctx);
    let strict_store_pkg_content_check_setting =
        resolve_strict_store_pkg_content_check(&settings_ctx);
    let side_effects_cache_setting = resolve_side_effects_cache(&settings_ctx);
    let side_effects_cache_readonly_setting = resolve_side_effects_cache_readonly(&settings_ctx);
    // `paranoid=true` forces unreviewed dep build scripts to error
    // instead of being silently skipped.
    let strict_dep_builds_setting = aube_settings::resolved::strict_dep_builds(&settings_ctx)
        || aube_settings::resolved::paranoid(&settings_ctx);
    let required_scripts =
        aube_settings::resolved::required_scripts(&settings_ctx).unwrap_or_default();
    validate_required_scripts(&cwd, &manifest, &required_scripts)?;
    // `useRunningStoreServer`: pnpm-only setting. aube has no
    // store-daemon, so honoring the strict semantics ("refuse install
    // unless the daemon is up") would just fail every install for
    // users with a pnpm-shaped `.npmrc`. Warn once and continue —
    // matches the docs in `settings.toml`. The warning is emitted
    // before `InstallProgress::try_new` runs (a few dozen lines down)
    // so writing straight to stderr can't collide with the animated
    // progress display.
    if resolve_use_running_store_server(&settings_ctx) {
        eprintln!(
            "warning: aube has no store server; useRunningStoreServer=true is accepted but has no effect"
        );
    }
    // `symlink`: pnpm-parity setting. aube's isolated layout is the
    // symlink graph under `node_modules/.aube/`, so a hard-copy layout
    // isn't a supported alternative. Warn once when the user asks for
    // `symlink=false` and keep building the symlink graph — same
    // accept-and-warn pattern as `useRunningStoreServer` above, and for
    // the same reason: a `.npmrc` ported from a pnpm setup should keep
    // loading instead of failing every install. Emitted before
    // `InstallProgress::try_new` below so stderr can't collide with the
    // animated progress display.
    if !resolve_symlink(&settings_ctx) {
        eprintln!(
            "warning: aube's isolated layout requires symlinks; symlink=false is accepted but has no effect"
        );
    }
    // `dlxCacheMaxAge` has no consumer yet (aube `dlx` uses a
    // tempdir per invocation) but resolving it here keeps the value
    // exercised through the same `ResolveCtx` the rest of the install
    // uses, so a future persistent-dlx-cache change can pick it up
    // without revisiting the resolver wiring.
    let _ = aube_settings::resolved::dlx_cache_max_age(&settings_ctx);
    tracing::debug!(
        "settings: network-concurrency={:?}, link-concurrency={:?}, verify-store-integrity={}, strict-store-pkg-content-check={}, side-effects-cache={}, side-effects-cache-readonly={}, strict-dep-builds={}",
        network_concurrency_setting,
        link_concurrency_setting,
        verify_store_integrity_setting,
        strict_store_pkg_content_check_setting,
        side_effects_cache_setting,
        side_effects_cache_readonly_setting,
        strict_dep_builds_setting,
    );

    // Resolve once for the whole install: both the fetch phase's
    // `AlreadyLinked` fast path and the linker's `aube_dir_entry_name`
    // need to encode `dep_path` into the same `.aube/<name>` filename.
    // Pinning the value here and threading it through both call sites
    // keeps them in lockstep, and the same resolved cap is re-read by
    // `aube list` / `aube why` / `aube patch` / `aube rebuild` so the
    // read-side encoding agrees with what the linker actually wrote.
    let virtual_store_dir_max_length = super::resolve_virtual_store_dir_max_length(&settings_ctx);

    // 2. Detect workspace
    let workspace_packages = aube_workspace::find_workspace_packages(&cwd)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let recursive_install = aube_settings::resolved::recursive_install(&settings_ctx);
    let has_workspace = !workspace_packages.is_empty();
    // Distinct from `has_workspace`: `is_workspace_project` stays
    // true when every workspace sub-package was just removed from
    // disk but the workspace yaml / `workspaces` field is still in
    // place. The lockfile drift check needs this stronger signal so
    // it still prunes orphan importer entries on the all-packages-
    // gone boundary, where `manifests` collapses to `[(".", root)]`
    // and looks indistinguishable from a non-workspace install.
    let is_workspace_project = aube_workspace::is_workspace_project_root(&cwd);
    let link_all_workspace_importers =
        has_workspace && (recursive_install || !opts.workspace_filter.is_empty());

    let mut manifests: Vec<(String, aube_manifest::PackageJson)> =
        vec![(".".to_string(), manifest.clone())];
    let mut ws_package_versions: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut ws_dirs: BTreeMap<String, std::path::PathBuf> = BTreeMap::new();

    if has_workspace {
        tracing::debug!(
            "Workspace: {} packages for {project_name}",
            workspace_packages.len()
        );
        for pkg_dir in &workspace_packages {
            let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
                .map_err(miette::Report::new)
                .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;

            // Importer key uses forward slash. pnpm lockfile convention
            // is always `/`. `workspace_importer_path` also returns `/`,
            // so a Windows `\` key here would never match filter lookups
            // and silently drop the importer from `--filter` installs.
            // Second risk: Linux CI reading a Windows-written lockfile
            // sees unknown keys and forces a re-resolve drift.
            //
            // `pathdiff` is used (rather than `strip_prefix`) so a
            // workspace whose `pnpm-workspace.yaml#packages` glob
            // reaches into the parent tree (`../**`) writes the
            // importer key as `../sibling` instead of an absolute
            // path. The lockfile and the linker both read these keys
            // back through `workspace_importer_path`, which uses the
            // same relative form.
            let rel_path = pathdiff::diff_paths(pkg_dir, &cwd)
                .unwrap_or_else(|| pkg_dir.clone())
                .to_string_lossy()
                .replace('\\', "/");

            if let Some(ref name) = pkg_manifest.name {
                // `version` is optional. pnpm accepts workspace
                // members without one (real-world: build-only design
                // systems consumed by an external toolchain, like
                // tuist's `noora`). When absent, fall back to "0.0.0":
                // siblings pinning via `workspace:*` / `workspace:^` /
                // `workspace:~` or bare `*` still link locally
                // (those branches in resolve_workspace accept any
                // ws version), and a specific range like
                // `workspace:^2.0.0` correctly fails to satisfy.
                let version = pkg_manifest.version.as_deref().unwrap_or("0.0.0");
                ws_package_versions.insert(name.clone(), version.to_string());
                ws_dirs.insert(name.clone(), pkg_dir.clone());
                tracing::debug!("  {name}@{version} ({rel_path})");
            }

            // `pnpm-workspace.yaml: packages: ["."]` expands to the
            // root itself; push would produce a duplicate importer
            // entry (`""` alongside `"."`) since `"."` is seeded at
            // the top of `manifests`. The resolver would then emit
            // two `graph.importers` entries mapping to the same
            // directory, and the linker would race to create the same
            // top-level symlinks twice. Collapse it here.
            if !rel_path.is_empty() {
                manifests.push((rel_path, pkg_manifest));
            }
        }
    }

    let lifecycle_manifests: Vec<(String, aube_manifest::PackageJson)> =
        if has_workspace && link_all_workspace_importers {
            order_lifecycle_manifests(
                manifests
                    .iter()
                    .filter(|(importer, _)| aube_linker::is_physical_importer(importer))
                    .cloned()
                    .collect(),
            )
        } else {
            vec![(".".to_string(), manifest.clone())]
        };
    let (mut build_policy, policy_warnings) = build_policy_from_manifest_sources(
        lifecycle_manifests.iter().map(|(_, manifest)| manifest),
        &ws_config_shared,
        opts.dangerously_allow_all_builds,
    );
    if let Some(inherited) = opts.inherited_build_policy.as_deref() {
        build_policy.merge(inherited);
    }
    let inherited_build_policy_for_git_prepare = Some(std::sync::Arc::new(build_policy.clone()));

    // 1b. Project `preinstall` lifecycle hooks.
    //     Workspace installs run the hook for every physical importer
    //     that will be linked, matching pnpm's recursive install
    //     behavior. Runs before the progress UI starts so script output
    //     cannot collide with the progress display.
    if !opts.ignore_scripts && !lockfile_only_effective && !opts.skip_root_lifecycle {
        let phase_start = std::time::Instant::now();
        for (importer_path, importer_manifest) in &lifecycle_manifests {
            let project_dir = importer_project_dir(&cwd, importer_path);
            run_root_lifecycle(
                &project_dir,
                &modules_dir_name,
                importer_manifest,
                aube_scripts::LifecycleHook::PreInstall,
            )
            .await?;
        }
        phase_timings.record("root_preinstall", phase_start.elapsed());
    }
    // Progress UI. `None` on non-TTY stderr, in text mode (e.g. `-v`), or
    // when progress output is otherwise disabled. A normal install produces
    // *no* output other than the bar itself — everything else is tracing at
    // debug level, visible with `aube -v install`. Must be constructed after
    // any lifecycle script that writes to stderr.
    let prog = InstallProgress::try_new();
    let prog_ref = prog.as_ref();

    // Auto-disable the global virtual store when any importer depends
    // on a package listed in `disableGlobalVirtualStoreForPackages`
    // (default: Next.js, Nuxt, Vite, VitePress, Parcel). Those
    // resolvers follow `node_modules/<pkg>` symlinks to real paths and
    // then walk up the directory tree looking for configs, app-router
    // roots, or hoisted deps; gvs makes `.aube/<pkg>` an absolute
    // symlink into `~/.cache/aube/virtual-store/`, so the walk escapes
    // the project and can't reach the top-level `node_modules/` where
    // direct deps live. Plain Webpack and Rollup are deliberately
    // *not* in the default list — Webpack resolves via the sibling
    // symlinks aube places inside `.aube/<pkg>/node_modules/`, and
    // Rollup is rarely a direct dep. The list is the extension
    // point — add them back (or other tools) here as their failures
    // surface. `CI=1` already forces per-project mode in `Linker::new`,
    // so we don't warn in that case (behavior wouldn't change and the
    // message would just be noise). `virtualStoreOnly` installs skip
    // the final top-level symlink pass, so the incompatible resolver
    // never sees the gvs path — suppress the warning there too.
    let gvs_triggers =
        aube_settings::resolved::disable_global_virtual_store_for_packages(&settings_ctx);
    let explicit_global_virtual_store =
        aube_settings::resolved::enable_global_virtual_store(&settings_ctx);
    let use_global_virtual_store_override = explicit_global_virtual_store.or_else(|| {
        let triggered_by = find_gvs_incompatible_trigger(&manifests, &gvs_triggers);
        // Match `Linker::new`'s exact gvs check — it keys off the `CI`
        // env var alone, not `npm_config_ci` / `NPM_CONFIG_CI`. Using a
        // broader set here would silently skip the override (and the
        // warning) in a scenario where the linker still turns gvs on,
        // leaving the Turbopack symlink error unmitigated. The snapshot
        // is populated from `std::env` at `InstallOptions::from_cli`
        // time, so it reflects the same environment the linker reads.
        let ci_mode = opts.env_snapshot.iter().any(|(k, _)| k == "CI");
        let virtual_store_only_setting = aube_settings::resolved::virtual_store_only(&settings_ctx);
        if let Some(name) = triggered_by
            && !ci_mode
            && !virtual_store_only_setting
        {
            tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_GVS_INCOMPATIBLE,
                "`{name}` isn't compatible with aube's global virtual store — \
                 installing per-project instead. Install still succeeds; repeat \
                 installs of this project just won't share materialized packages \
                 across projects. Fixing this requires an upstream change in \
                 `{name}` itself (please file it with that project, not aube). \
                 To silence this warning, run `aube config set \
                 enableGlobalVirtualStore false --location project` — or set \
                 `disableGlobalVirtualStoreForPackages=[]` to opt out of this \
                 auto-detection entirely. \
                 Details: https://aube.en.dev/package-manager/global-virtual-store"
            );
            Some(false)
        } else {
            None
        }
    });

    // Remember which lockfile format the project currently uses so
    // every downstream write site (the `--lockfile-only` short-circuit
    // below *and* the re-resolve branch further down) can preserve it
    // instead of quietly converting the project to another filename.
    // Must happen before the `--lockfile-only` block so that path
    // doesn't bypass the format-preserving write logic. Skipped when
    // `lockfile=false` — no lockfile is read and no format is
    // preserved, so the install always writes nothing (see below).
    let source_kind_before = if lockfile_enabled {
        aube_lockfile::detect_existing_lockfile_kind(&lockfile_dir)
    } else {
        None
    };

    // Hand any parseable lockfile to the resolver as `existing` so
    // unchanged specs reuse their already-pinned versions and only
    // entries whose spec actually drifted get re-resolved. Without
    // this, `aube install` after any manifest edit re-resolves every
    // transitive against the latest packument and silently bumps
    // versions that the previous lockfile had pinned (e.g.
    // `electron-to-chromium@1.5.344` → `1.5.343`), which is the
    // opposite of what pnpm/bun's default `install` does.
    //
    // Scope:
    //   - Fix: existing behavior (`--fix-lockfile`).
    //   - Prefer: default mode; the bug above lives here.
    //   - Frozen: short-circuits to the lockfile-as-truth branch and
    //     never calls the resolver, so parsing is wasted work.
    //   - No (`--no-frozen-lockfile`): kept as fresh-resolve so users
    //     who reach for that flag to bump transitives still get a
    //     fresh pass. Matching pnpm's "lockfile may drift but locked
    //     versions are still preferred" semantics is a separate
    //     decision and would change observable behavior on this path.
    //
    // We parse once and keep both the graph and its kind so the
    // `--lockfile-only` block below can reuse the same result for its
    // freshness check instead of re-reading + re-parsing the same file.
    //
    // Hard-fail on a real parse error: the prior in-arm parse in
    // `FrozenMode::Prefer` propagated parse errors out of
    // `lockfile_result`, and silently swallowing them here would leave
    // a corrupt lockfile masquerading as "no lockfile" and trigger a
    // full re-resolve without surfacing the actionable diagnostic.
    // `NotFound` is the one error we treat as expected — it just means
    // the lockfile is absent, which the downstream arms already handle.
    let lockfile_pre_parse: Option<(aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind)> =
        if lockfile_enabled && matches!(mode, FrozenMode::Fix | FrozenMode::Prefer) {
            match parse_lockfile_dir_remapped_with_kind(
                &lockfile_dir,
                &lockfile_importer_key,
                &manifest,
            ) {
                Ok(parsed) => Some(parsed),
                Err(aube_lockfile::Error::NotFound(_)) => None,
                Err(e) => {
                    return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
                }
            }
        } else {
            None
        };
    let existing_for_resolver: Option<&aube_lockfile::LockfileGraph> =
        lockfile_pre_parse.as_ref().map(|(g, _)| g);

    // `--lockfile-only` short-circuit. Resolves (or reuses a fresh
    // lockfile), writes the new lockfile, and exits before any tarball
    // fetch / link / lifecycle work. Runs *before* the FrozenMode
    // match so it bypasses drift hard-errors entirely — pnpm's
    // `--lockfile-only` regenerates regardless of frozen mode, and
    // we'd otherwise be preempted by the auto-CI Frozen default.
    // `enableModulesDir=false` follows the same short-circuit so
    // projects that persistently disable node_modules materialization
    // share the exact same control flow.
    if lockfile_only_effective {
        // `--no-frozen-lockfile` means "always re-resolve", so skip the
        // freshness check entirely in that mode. Otherwise (Prefer, Fix,
        // or CI's auto-Frozen) a fresh lockfile is a no-op — for Fix
        // specifically, "fresh" means "nothing to fix."
        let force_resolve = matches!(mode, FrozenMode::No);
        // Reuse the up-front pre-parse when we already have it so we
        // don't read and parse the same lockfile twice on
        // `--lockfile-only`. The borrowed form is all the freshness
        // check needs — `existing_for_resolver` still points at the
        // same graph for the resolver call below.
        let parsed_owned;
        let parsed: Result<
            (&aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind),
            &aube_lockfile::Error,
        > = if let Some((g, k)) = lockfile_pre_parse.as_ref() {
            Ok((g, *k))
        } else {
            parsed_owned = parse_lockfile_dir_remapped_with_kind(
                &lockfile_dir,
                &lockfile_importer_key,
                &manifest,
            );
            match &parsed_owned {
                Ok((g, k)) => Ok((g, *k)),
                Err(e) => Err(e),
            }
        };
        if let Err(e) = parsed
            && !matches!(e, aube_lockfile::Error::NotFound(_))
        {
            // Can't hand &Error to miette::Report since Diagnostic
            // is only implemented on owned Error. Re-parse once to
            // get an owned Error value for the Diagnostic path.
            // Slightly wasteful on the error branch, install is
            // about to abort anyway so speed does not matter here.
            // What matters: keeping the source span and miette help
            // text instead of flattening to one line via `{e}`.
            match parse_lockfile_dir_remapped_with_kind(
                &lockfile_dir,
                &lockfile_importer_key,
                &manifest,
            ) {
                Ok(_) => {
                    // Race: second parse succeeded while first failed.
                    // Surface the observed error text as a best
                    // effort flat message. Extremely unlikely.
                    return Err(miette!("failed to parse lockfile: {e}"));
                }
                Err(owned) => {
                    return Err(miette::Report::new(owned)).wrap_err("failed to parse lockfile");
                }
            }
        }
        let fresh = !force_resolve
            && matches!(
                parsed,
                Ok((g, _))
                    if matches!(
                        g.check_drift_workspace(&manifests, &ws_config_shared.overrides, &ws_config_shared.ignored_optional_dependencies, &workspace_catalogs, is_workspace_project),
                        DriftStatus::Fresh,
                    )
                        && matches!(g.check_catalogs_drift(&workspace_catalogs), DriftStatus::Fresh)
            );
        if fresh {
            tracing::debug!("--lockfile-only: lockfile already up to date");
            if let Some(p) = prog_ref {
                p.finish(true);
            }
            eprintln!("Lockfile is up to date, resolution step is skipped");
            return Ok(());
        }
        if let Some(p) = prog_ref {
            p.set_phase("resolving");
        }
        let client = std::sync::Arc::new(make_client(&cwd).with_network_mode(opts.network_mode));
        let pnpmfile_paths = if opts.ignore_pnpmfile {
            Vec::new()
        } else {
            crate::pnpmfile::ordered_paths(
                crate::pnpmfile::detect_global(&cwd, opts.global_pnpmfile.as_deref()).as_deref(),
                crate::pnpmfile::detect(
                    &cwd,
                    opts.pnpmfile.as_deref(),
                    ws_config_shared.pnpmfile_path.as_deref(),
                )
                .as_deref(),
            )
        };
        super::run_pnpmfile_pre_resolution(&pnpmfile_paths, &cwd, existing_for_resolver).await?;
        let (read_package_host, read_package_forwarders) =
            match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, &cwd)
                .await
                .wrap_err("failed to start pnpmfile readPackage host")?
            {
                Some((h, f)) => (Some(h), f),
                None => (None, Vec::new()),
            };
        let read_package_hook: Option<Box<dyn aube_resolver::ReadPackageHook>> =
            read_package_host.map(|h| Box::new(h) as Box<dyn aube_resolver::ReadPackageHook>);
        let mut resolver = configure_resolver(
            aube_resolver::Resolver::new(client.clone()),
            &cwd,
            &manifest,
            ResolverConfigInputs {
                settings_ctx: &settings_ctx,
                workspace_config: &ws_config_shared,
                workspace_catalogs: &workspace_catalogs,
                opts: &opts,
                // `lockfile=false` collapses to `None` so the resolver
                // doesn't waste a fetch widening a lockfile that will
                // never be written. With lockfiles enabled, a missing
                // `source_kind_before` means "we'll create the default
                // aube-lock.yaml", so the aube-native wide default
                // applies.
                target_lockfile_kind: lockfile_enabled
                    .then(|| source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube)),
            },
            read_package_hook,
        );
        let mut graph = if has_workspace {
            resolver
                .resolve_workspace(&manifests, existing_for_resolver, &ws_package_versions)
                .await
        } else {
            resolver.resolve(&manifest, existing_for_resolver).await
        }
        .map_err(miette::Report::new)
        .wrap_err("failed to resolve dependencies")?;
        drop(resolver);
        // Drain the readPackage stderr forwarders so every `ctx.log`
        // record they captured during resolve flushes to stdout before
        // afterAllResolved emits its own pnpm:hook records — keeps
        // resolve-time logs strictly ahead of post-resolve logs in the
        // ndjson stream.
        crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
        crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, &cwd, &mut graph).await?;
        // Same tarball-URL population pass as the main fetch branch —
        // keeps `--lockfile-only` and regular installs byte-identical.
        // Reuses the resolver's `client` (already built above) to avoid
        // re-walking `.npmrc` and rebuilding the rustls client just to
        // construct registry URLs.
        if lockfile_include_tarball_url {
            let lo_client = client.as_ref();
            graph.settings.lockfile_include_tarball_url = true;
            for pkg in graph.packages.values_mut() {
                if pkg.local_source.is_some() {
                    continue;
                }
                // Preserve any URL the parser already captured from an
                // aliased `resolved:` field — deriving from
                // `(registry_name, version)` would also work for
                // aliases but skips a redundant allocation.
                if pkg.tarball_url.is_none() {
                    pkg.tarball_url =
                        Some(lo_client.tarball_url(pkg.registry_name(), &pkg.version));
                }
            }
        }
        let lo_write_kind = source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube);
        if shared_workspace_lockfile || !has_workspace {
            let lo_written = write_lockfile_dir_remapped(
                &lockfile_dir,
                &lockfile_importer_key,
                &graph,
                &manifest,
                lo_write_kind,
            )
            .into_diagnostic()
            .wrap_err("failed to write lockfile")?;
            tracing::debug!(
                "--lockfile-only: wrote {}",
                lo_written
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| lo_written.display().to_string())
            );
        } else {
            write_per_project_lockfiles(&cwd, &graph, &manifests, lo_write_kind)?;
        }
        // Prune unused catalog entries *after* the lockfile hits disk —
        // same ordering as the main install path below, so a
        // workspace-yaml write error can't block the command's
        // primary output.
        maybe_cleanup_unused_catalogs(&cwd, &settings_ctx, &workspace_catalogs, &graph.catalogs)?;
        if let Some(p) = prog_ref {
            p.finish(true);
        }
        eprintln!(
            "Lockfile written ({} packages); skipped node_modules linking",
            graph.packages.len()
        );
        return Ok(());
    }

    // Global-virtual-store transition guard. The linker can't reconcile
    // a mode switch in place — a non-gvs pass landing on a gvs tree
    // silently re-uses stale symlinks into the shared store, and a gvs
    // pass landing on a per-project tree fails to unlink the populated
    // directories before creating its symlinks. When the existing
    // `.aube/` tree's layout disagrees with the mode this install will
    // produce, wipe `node_modules/` (and, if `virtualStoreDir` points
    // outside it, the standalone `.aube/` tree) so the linker rebuilds
    // from scratch. Matches pnpm's behavior modulo the prompt: pnpm
    // asks, aube warns and proceeds. `state` goes too so an interrupted
    // wipe can't leave a half-rebuilt tree behind a stale warm-path
    // "up to date" verdict. Skipped in `--lockfile-only` /
    // `enableModulesDir=false` mode (the return above already handled
    // that case — no node_modules to reconcile).
    let planned_gvs = use_global_virtual_store_override.unwrap_or_else(|| {
        // Match `Linker::new`'s default: `CI` unset → gvs on. Reads the
        // same env snapshot `find_gvs_incompatible_trigger` checked
        // above, so the two sites can't disagree mid-install.
        !opts.env_snapshot.iter().any(|(k, _)| k == "CI")
    });
    if let Some(existing_gvs) = detect_aube_dir_gvs_mode(&aube_dir)
        && existing_gvs != planned_gvs
    {
        let from = if existing_gvs { "enabled" } else { "disabled" };
        let to = if planned_gvs { "enabled" } else { "disabled" };
        let modules_dir_path = cwd.join(&modules_dir_name);
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_GVS_MODE_CHANGED,
            "global virtual store {from} → {to}; removing {} and reinstalling from scratch",
            modules_dir_path.display()
        );
        // Hard-fail the install on a wipe failure instead of swallowing
        // the error. We've already told the user a wipe was happening,
        // so proceeding past a half-complete removal would land on the
        // exact stale mixed-mode tree this guard exists to prevent —
        // worse than aborting with a clear error the user can act on
        // (locked file on Windows, permissions, busy mount). A
        // `NotFound` race (concurrent removal, user deleted the tree
        // between our classification and the wipe) is benign and stays
        // silent so the install can proceed.
        if let Err(e) = std::fs::remove_dir_all(&modules_dir_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(miette!(
                "global virtual store transition: failed to remove {}: {e}",
                modules_dir_path.display()
            ));
        }
        if !aube_dir.starts_with(&modules_dir_path)
            && let Err(e) = std::fs::remove_dir_all(&aube_dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(miette!(
                "global virtual store transition: failed to remove {}: {e}",
                aube_dir.display()
            ));
        }
        // Stale sidecar after GVS transition would match against the
        // pre-transition tree on next install and short-circuit. Need
        // to surface remove failure not swallow it.
        state::remove_state(&cwd).map_err(|e| {
            miette!("global virtual store transition: failed to remove install state: {e}")
        })?;
    }

    // 3. Parse or resolve lockfile, streaming tarball fetches during resolution
    let phase_start = std::time::Instant::now();
    let store = std::sync::Arc::new(super::open_store(&cwd)?);
    // Pre-create all 256 two-char shard directories in the CAS root.
    // `import_bytes` is called once per stored file (~7.5k for a medium
    // install) and previously did `mkdirp(parent)` per call — a stat
    // syscall that was the #1 hotspot in a dtrace/fs_usage profile.
    // With the shard tree pre-created, every `import_bytes` skips the
    // mkdirp entirely and lets its `create_new` open handle the
    // existence check atomically. Best-effort: a failure here is not
    // fatal because `import_bytes` retains the slow-path mkdirp
    // fallback when shards are missing.
    if let Err(e) = store.ensure_shards_exist() {
        tracing::debug!("ensure_shards_exist failed (slow path will cover): {e}");
    }
    // macOS fast-path gate: take an exclusive `try_lock` on
    // `<store>/v1/.install.lock`. If we get it, no other aube install is
    // running against this store right now, so the CAS write path can
    // skip the tempfile + persist_noclobber dance and write straight to
    // the final content-addressed path (`Store::enable_fast_path`). The
    // guard is held in `_store_lock` for the rest of this `run` call;
    // dropping it at function exit releases the lock. Contention falls
    // back to the safe tempfile path — concurrent installers still
    // proceed, just at the existing speed.
    //
    // Linux is unaffected: `create_cas_file` always uses O_TMPFILE+linkat
    // there, which is already atomic-by-construction and faster than
    // both options. Windows keeps the tempfile path; the fast-path branch
    // in `aube-store` is unix-only (`OpenOptionsExt::mode`), so gating
    // the lock acquisition on macOS too avoids opening a lock file that
    // nothing would consult.
    #[cfg(target_os = "macos")]
    let _store_lock = {
        let lock_dir = store
            .root()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| store.root().to_path_buf());
        let _ = std::fs::create_dir_all(&lock_dir);
        let lock_path = lock_dir.join(".install.lock");
        match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
        {
            Ok(file) => match file.try_lock() {
                Ok(()) => {
                    store.enable_fast_path();
                    tracing::debug!("CAS fast path enabled (exclusive store lock acquired)");
                    Some(file)
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    tracing::debug!(
                        "another aube install is using this store; staying on tempfile path"
                    );
                    None
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    tracing::debug!("store lock probe failed ({e}); staying on tempfile path");
                    None
                }
            },
            Err(e) => {
                tracing::debug!(
                    "could not open store lock at {} ({e}); staying on tempfile path",
                    lock_path.display()
                );
                None
            }
        }
    };

    // Decide what to do with whatever lockfile is on disk based on FrozenMode + drift.
    // Returns either:
    //   - Ok((graph, kind))           → use the lockfile as-is
    //   - Err(NotFound)                → fall through to the resolver
    //   - Err(other) / early return    → hard fail
    //
    // When `lockfile=false`, skip the lockfile layer entirely: we
    // always fall through to the resolver. This explicitly overrides
    // FrozenMode::Frozen, since "use the lockfile strictly" contradicts
    // "don't use a lockfile" — the canonical interpretation is that
    // frozen mode is a no-op without a lockfile to freeze against.
    let lockfile_result = if !lockfile_enabled {
        tracing::debug!("lockfile=false: skipping lockfile parse, re-resolving");
        Err(aube_lockfile::Error::NotFound(cwd.clone()))
    } else {
        match mode {
            FrozenMode::No => {
                // Always re-resolve.
                Err(aube_lockfile::Error::NotFound(cwd.clone()))
            }
            FrozenMode::Fix => {
                // Always fall through to re-resolve; `existing_for_resolver`
                // carries the current lockfile (if any) so the resolver
                // reuses locked versions for unchanged specs and only
                // re-resolves entries whose spec drifted.
                Err(aube_lockfile::Error::NotFound(cwd.clone()))
            }
            FrozenMode::Frozen => {
                // Use the lockfile, but error out on any drift across all workspace importers.
                let parsed = parse_lockfile_dir_remapped_with_kind(
                    &lockfile_dir,
                    &lockfile_importer_key,
                    &manifest,
                );
                if let Ok((ref graph, _)) = parsed {
                    if let DriftStatus::Stale { reason } =
                        graph.check_catalogs_drift(&workspace_catalogs)
                    {
                        return Err(miette!(
                            "lockfile is out of date with pnpm-workspace.yaml: {reason}\n\
                         help: run without --frozen-lockfile to update the lockfile"
                        ));
                    }
                    if let DriftStatus::Stale { reason } = graph.check_drift_workspace(
                        &manifests,
                        &ws_config_shared.overrides,
                        &ws_config_shared.ignored_optional_dependencies,
                        &workspace_catalogs,
                        is_workspace_project,
                    ) {
                        return Err(miette!(
                            "lockfile is out of date with package.json: {reason}\n\
                         help: run without --frozen-lockfile to update the lockfile, \
                         or run `aube install --no-frozen-lockfile` to regenerate it"
                        ));
                    }
                }
                parsed
            }
            FrozenMode::Prefer => {
                // Use the lockfile when fresh, otherwise pretend there isn't one
                // so the existing "no lockfile → resolve" branch handles it.
                // Reuse `lockfile_pre_parse` instead of parsing the same file
                // a second time — on Prefer-fresh we clone the graph so the
                // borrow held by `existing_for_resolver` keeps pointing at
                // the original (unused on the fresh path, but safe to leave).
                match lockfile_pre_parse.as_ref() {
                    Some((graph, kind)) => {
                        if let DriftStatus::Stale { reason } =
                            graph.check_catalogs_drift(&workspace_catalogs)
                        {
                            tracing::debug!(
                                "Lockfile out of date with workspace catalogs ({reason}), re-resolving..."
                            );
                            Err(aube_lockfile::Error::NotFound(cwd.clone()))
                        } else {
                            match graph.check_drift_workspace(
                                &manifests,
                                &ws_config_shared.overrides,
                                &ws_config_shared.ignored_optional_dependencies,
                                &workspace_catalogs,
                                is_workspace_project,
                            ) {
                                DriftStatus::Fresh => Ok((graph.clone(), *kind)),
                                DriftStatus::Stale { reason } => {
                                    tracing::debug!(
                                        "Lockfile out of date ({reason}), re-resolving..."
                                    );
                                    Err(aube_lockfile::Error::NotFound(cwd.clone()))
                                }
                            }
                        }
                    }
                    None => Err(aube_lockfile::Error::NotFound(cwd.clone())),
                }
            }
        }
    };

    // Deprecation messages from freshly-resolved packages. Only the
    // no-lockfile branch below populates this; the lockfile-reuse branch
    // has no packument in hand. Rendered right before the install summary
    // once `filter_graph` has culled dropped packages.
    let deprecations: std::sync::Arc<
        std::sync::Mutex<Vec<crate::deprecations::DeprecationRecord>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    // Per-direct-dep packument snapshot rendered inline by the install
    // summary printer (`+ name@version  deprecated · latest …`). Only
    // populated by the resolve-from-packuments branch — the frozen
    // lockfile reuse path has no cache to read from, so badges silently
    // degrade to empty rather than triggering extra network.
    let mut direct_dep_info: std::collections::HashMap<String, aube_resolver::DirectDepInfo> =
        std::collections::HashMap::new();

    // Captures the prewarm task's `compute_graph_hashes` output so the
    // link phase can reuse it instead of recomputing the same 4-pass
    // BLAKE3 walk over `graph.packages`. Populated by the no-lockfile
    // branch when the prewarm task uses GVS; left `None` on the
    // frozen-lockfile path or when the prewarm short-circuits.
    let mut prewarm_graph_hashes: Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>> =
        None;
    let (graph, package_indices, cached_count, fetch_count) = match lockfile_result {
        Ok((mut graph, kind)) => {
            // Drop optional deps that don't match the current platform
            // (or are in `pnpm.ignoredOptionalDependencies`) before we
            // start fetching tarballs. The resolver's inline filter
            // never runs on the lockfile-happy path, so this pass is
            // what makes cross-platform lockfile installs work.
            let (sup_os, sup_cpu, sup_libc) =
                aube_manifest::effective_supported_architectures(&manifest, &ws_config_shared);
            let supported_architectures = aube_resolver::SupportedArchitectures {
                os: sup_os,
                cpu: sup_cpu,
                libc: sup_libc,
                ..Default::default()
            };
            let ignored_optional_deps = aube_manifest::effective_ignored_optional_dependencies(
                &manifest,
                &ws_config_shared,
            );
            // npm/bun lockfiles serialize a flat, pre-hoisted tree
            // with no peer context — they rely on Node's upward
            // `node_modules/` walk to find peer deps, which the
            // isolated virtual store breaks. Fresh resolves flow
            // through `Resolver::resolve_workspace`, which runs
            // `hoist_auto_installed_peers` + `apply_peer_contexts` on
            // its way out; the lockfile path has to replicate those
            // two steps explicitly or peer-dependent packages
            // (e.g. `@tanstack/devtools-vite` peering on `vite`)
            // install with no sibling peer link and die at runtime
            // with `Cannot find package`.
            //
            // `aube-lock.yaml` / `pnpm-lock.yaml` already carry
            // peer-context suffixes and peer edges merged into
            // `dependencies`, so we skip them — re-running the pass
            // would double-suffix every key.
            //
            // Yarn v1 has the same flat shape but is intentionally
            // omitted: real-world `yarn.lock` files don't record
            // `peerDependencies` per entry (yarn 1.22 emits them
            // only on the workspace root), so running the pass would
            // be a no-op. Making yarn v1 imports peer-correct needs
            // a packument fetch on the import path to graft peer
            // ranges back onto each `LockedPackage` — a deeper
            // change than this match arm.
            //
            // The hoist must run *before* `filter_graph`: bun records
            // peer-only-installed packages (e.g. `@mui/material` when
            // the importer only depends on `@textea/json-viewer`, which
            // peers on MUI) in its packages map, but our bun parser
            // doesn't merge those into the consumer's `dependencies`
            // map. `filter_graph`'s GC walk only follows `dependencies`,
            // so without the hoist running first it prunes every
            // peer-only package as unreachable — and a post-prune hoist
            // has nothing left to promote.
            let needs_peer_pass = matches!(
                kind,
                aube_lockfile::LockfileKind::Npm
                    | aube_lockfile::LockfileKind::NpmShrinkwrap
                    | aube_lockfile::LockfileKind::Bun
            );
            let peer_pass_start = std::time::Instant::now();
            let pkgs_before = graph.packages.len();
            if needs_peer_pass {
                graph = aube_resolver::hoist_auto_installed_peers(graph);
            }
            aube_resolver::platform::filter_graph(
                &mut graph,
                &supported_architectures,
                &ignored_optional_deps,
            );
            if needs_peer_pass {
                let peer_options = aube_resolver::PeerContextOptions {
                    dedupe_peer_dependents: resolve_dedupe_peer_dependents(&settings_ctx),
                    dedupe_peers: resolve_dedupe_peers(&settings_ctx),
                    resolve_from_workspace_root: resolve_peers_from_workspace_root(&settings_ctx),
                    peers_suffix_max_length: resolve_peers_suffix_max_length(&settings_ctx),
                };
                graph = aube_resolver::apply_peer_contexts(graph, &peer_options)
                    .map_err(|e| miette!("peer-context pass failed: {e}"))?;
                tracing::debug!(
                    "peer-context pass (lockfile={:?}) {} → {} packages in {:.1?}",
                    kind,
                    pkgs_before,
                    graph.packages.len(),
                    peer_pass_start.elapsed()
                );
            }
            let source_label = match kind {
                aube_lockfile::LockfileKind::Aube => "Lockfile",
                aube_lockfile::LockfileKind::Pnpm => "pnpm-lock.yaml",
                aube_lockfile::LockfileKind::Yarn | aube_lockfile::LockfileKind::YarnBerry => {
                    "yarn.lock"
                }
                aube_lockfile::LockfileKind::Npm => "package-lock.json",
                aube_lockfile::LockfileKind::NpmShrinkwrap => "npm-shrinkwrap.json",
                aube_lockfile::LockfileKind::Bun => "bun.lock",
            };
            tracing::debug!(
                "{source_label}: {} packages for {project_name}",
                graph.packages.len()
            );
            tracing::debug!(
                "phase:resolve (from lockfile) {:.1?}",
                phase_start.elapsed()
            );
            phase_timings.record("resolve", phase_start.elapsed());

            // Lockfile path: the total is known upfront, so seed the overall
            // bar with the full package count and enter the fetch phase.
            if let Some(p) = prog_ref {
                p.set_total(graph.packages.len());
                p.set_phase("fetching");
            }
            // Seed the chain index for diagnostic enrichment on the
            // lockfile fast path. Same effect as the resolve-fresh
            // branch above — error wrappers in `dep_chain` now know
            // each package's ancestor path.
            crate::dep_chain::set_active(&graph);
            aube_registry::slow_metadata::flush_summary();

            // Check index cache, fetch missing tarballs. Tarball client
            // is lazy because eager construction costs ~20ms even when
            // no request gets sent, dominating no-op install time.
            //
            // Pipeline GVS materialization into the fetch tail. Same
            // shape as the no-lockfile branch. Channel feeds a
            // concurrent materializer that reflinks into GVS, hiding
            // link-step-1 cost behind the fetch tail.
            let phase_start = std::time::Instant::now();
            let network_mode = opts.network_mode;
            let cwd_for_client = cwd.clone();

            let lock_node_version = crate::engines::resolve_node_version(
                aube_settings::resolved::node_version(&settings_ctx).as_deref(),
            );
            let lock_build_policy = std::sync::Arc::new(build_policy.clone());
            let lock_strategy = resolve_link_strategy(&cwd, &settings_ctx, planned_gvs)?;
            let (lock_patches, lock_patch_hashes) = crate::patches::load_patches_for_linker(&cwd)?;
            let (lock_materialize_tx, lock_materialize_rx) = materialize_channel();
            let lock_prewarm_inputs = GvsPrewarmInputs {
                graph: std::sync::Arc::new(graph.clone()),
                store: store.clone(),
                cwd: cwd.clone(),
                virtual_store_dir_max_length,
                link_strategy: lock_strategy,
                link_concurrency: link_concurrency_setting,
                patches: lock_patches,
                patch_hashes: lock_patch_hashes,
                node_version: lock_node_version,
                build_policy: lock_build_policy,
                use_global_virtual_store_override,
            };
            let lock_materialize_handle =
                spawn_gvs_prewarm(lock_prewarm_inputs, lock_materialize_rx);

            let fetch_result = fetch_packages_with_root(
                &graph.packages,
                &store,
                || {
                    std::sync::Arc::new(
                        make_client(&cwd_for_client).with_network_mode(network_mode),
                    )
                },
                prog_ref,
                &cwd,
                &aube_dir,
                Some(lock_materialize_tx),
                /*skip_already_linked_shortcut=*/ has_workspace,
                virtual_store_dir_max_length,
                opts.ignore_scripts,
                network_concurrency_setting,
                verify_store_integrity_setting,
                strict_store_integrity_setting,
                strict_store_pkg_content_check_setting,
                opts.git_prepare_depth,
                inherited_build_policy_for_git_prepare.clone(),
                resolve_git_shallow_hosts(&settings_ctx),
            )
            .await;
            // Don't abort the materializer on fetch err: the failing
            // fetch task drops its `tx`, so the materializer's `rx`
            // closes and it exits naturally. Awaiting first lets a real
            // materializer error (the likely root cause of a generic
            // "materializer task exited..." fetch err) surface instead.
            let (indices, cached, fetched) = match fetch_result {
                Ok(t) => t,
                Err(e) => {
                    return Err(combine_install_pipeline_errors(lock_materialize_handle, e).await);
                }
            };
            // Materializer stats roll into link via GVS-already-linked
            // fast path. Errors abort install.
            let _ = lock_materialize_handle.await.into_diagnostic()??;
            tracing::debug!(
                "phase:fetch {:.1?} ({fetched} packages)",
                phase_start.elapsed()
            );
            phase_timings.record("fetch", phase_start.elapsed());

            (graph, indices, cached, fetched)
        }
        Err(aube_lockfile::Error::NotFound(_))
            if !(matches!(mode, FrozenMode::Frozen) && opts.strict_no_lockfile) =>
        {
            // No lockfile — resolve + fetch tarballs concurrently
            tracing::debug!("No lockfile found, resolving dependencies for {project_name}...");
            if let Some(p) = prog_ref {
                // Seed the resolving-phase denominator floor from any
                // existing lockfile on disk. In FrozenMode::Fix /
                // Prefer we already parsed it into
                // `existing_for_resolver`; in FrozenMode::No the
                // pre-parse is skipped (we always re-resolve), so peek
                // the disk lockfile inline. The cost is one extra
                // parse on the fresh-resolve path, dwarfed by the
                // resolve itself — and the resulting estimate lets
                // the resolving bar show real progress instead of an
                // empty placeholder.
                let lockfile_estimate =
                    existing_for_resolver.map(|g| g.packages.len()).or_else(|| {
                        parse_lockfile_dir_remapped_with_kind(
                            &lockfile_dir,
                            &lockfile_importer_key,
                            &manifest,
                        )
                        .ok()
                        .map(|(g, _)| g.packages.len())
                    });
                if let Some(n) = lockfile_estimate {
                    p.set_total_floor(n);
                }
                p.set_phase("resolving");
            }
            // Resolve node version + build policy up front so the
            // GVS-prewarm materializer (spawned below the resolver
            // await) can compute the same graph hashes the link phase
            // will. Keeping a single source of truth avoids any
            // subdir-name drift between prewarm and link step 1.
            let node_version_for_prewarm = crate::engines::resolve_node_version(
                aube_settings::resolved::node_version(&settings_ctx).as_deref(),
            );
            let build_policy_for_prewarm = std::sync::Arc::new(build_policy.clone());
            let client =
                std::sync::Arc::new(make_client(&cwd).with_network_mode(opts.network_mode));
            // Speculative TLS + TCP + HTTP/2 handshake. Fires while the
            // rest of this function builds the resolver, parses the
            // manifest, and reads the lockfile. By the time the
            // resolver requests its first packument the connection
            // pool is already warm, hiding ~50-150 ms of handshake on
            // cold installs. `AUBE_DISABLE_SPECULATIVE_TLS=1` opts
            // out.
            client.prewarm_connection();
            let tarball_client = client.clone();

            // Set up streaming resolver with disk-backed packument cache.
            // Resolver options are applied via `configure_resolver` so the
            // `--lockfile-only` short-circuit produces an identical lockfile.
            // `AUBE_CONCURRENCY` is an emergency override for users on slow
            // private registries (Artifactory, Nexus) where the default
            // 128 in-flight tarballs trigger 429/503 throttling. Honored
            // ahead of `network_concurrency_setting` so the env var wins
            // over npmrc + workspace yaml.
            let env_concurrency =
                aube_util::concurrency::parse_concurrency_env().map(|n| n as usize);
            let fetch_network_concurrency = env_concurrency
                .or(network_concurrency_setting)
                .unwrap_or_else(default_streaming_network_concurrency);
            // Channel capacity is decoupled from fetch concurrency: the
            // mpsc just buffers ResolvedPackage handoffs so the BFS
            // never blocks on send() while the fetch coordinator is
            // mid-tarball. Sized to absorb deep-tree bursts without
            // backpressure on graphs into the tens of thousands of
            // packages; fetch parallelism is still gated by
            // `fetch_network_concurrency` downstream.
            let stream_capacity = fetch_network_concurrency.saturating_mul(16).max(1024);
            let (resolver, mut resolved_rx) =
                aube_resolver::Resolver::with_stream_capacity(client, stream_capacity);
            let pnpmfile_paths = if opts.ignore_pnpmfile {
                Vec::new()
            } else {
                crate::pnpmfile::ordered_paths(
                    crate::pnpmfile::detect_global(&cwd, opts.global_pnpmfile.as_deref())
                        .as_deref(),
                    crate::pnpmfile::detect(
                        &cwd,
                        opts.pnpmfile.as_deref(),
                        ws_config_shared.pnpmfile_path.as_deref(),
                    )
                    .as_deref(),
                )
            };
            super::run_pnpmfile_pre_resolution(&pnpmfile_paths, &cwd, existing_for_resolver)
                .await?;
            let (read_package_host, read_package_forwarders) =
                match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, &cwd)
                    .await
                    .wrap_err("failed to start pnpmfile readPackage host")?
                {
                    Some((h, f)) => (Some(h), f),
                    None => (None, Vec::new()),
                };
            let read_package_hook: Option<Box<dyn aube_resolver::ReadPackageHook>> =
                read_package_host.map(|h| Box::new(h) as Box<dyn aube_resolver::ReadPackageHook>);
            let mut resolver = configure_resolver(
                resolver,
                &cwd,
                &manifest,
                ResolverConfigInputs {
                    settings_ctx: &settings_ctx,
                    workspace_config: &ws_config_shared,
                    workspace_catalogs: &workspace_catalogs,
                    opts: &opts,
                    // Same disambiguation as the `--lockfile-only` path:
                    // `None` only when no lockfile will be written, so
                    // widening to every common platform doesn't happen
                    // just to be discarded.
                    target_lockfile_kind: lockfile_enabled
                        .then(|| source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube)),
                },
                read_package_hook,
            );

            // Spawn the tarball fetch coordinator — it starts fetching as
            // packages arrive from the resolver, overlapping network I/O.
            // Clone the registry client up front so the post-fetch
            // lockfile-write step (below) can still use it to derive
            // tarball URLs when `lockfileIncludeTarballUrl=true` — the
            // `tokio::spawn` below moves one clone into the fetch
            // coordinator's task.
            let post_fetch_client = tarball_client.clone();
            let fetch_store = store.clone();
            let fetch_progress = prog.clone();
            let fetch_project_root = cwd.clone();
            let fetch_local_client = tarball_client.clone();
            let fetch_ignore_scripts = opts.ignore_scripts;
            let fetch_git_prepare_depth = opts.git_prepare_depth;
            let fetch_inherited_build_policy = inherited_build_policy_for_git_prepare.clone();
            let fetch_verify_integrity = verify_store_integrity_setting;
            let fetch_strict_integrity = strict_store_integrity_setting;
            let fetch_strict_pkg_content_check = strict_store_pkg_content_check_setting;
            let fetch_git_shallow_hosts = resolve_git_shallow_hosts(&settings_ctx);
            // Host-side platform filter for the streaming fetch. The
            // resolver widens its graph filter for aube-lock.yaml so
            // the committed lockfile carries native optionals for every
            // common platform, but that widening mustn't make us
            // download every foreign-platform tarball up front — most
            // of them will disappear when `filter_graph` trims optional
            // edges below, and only a vanishingly rare broken-package
            // shape (required dep with platform constraints) actually
            // needs the fetch. A post-resolve catch-up pass picks up
            // those stragglers from the finalized graph; here we just
            // defer. `filter_graph` keys off the same narrow manifest
            // set, so a deferred package that survives the trim is
            // exactly one the catch-up must fetch.
            let (fetch_sup_os, fetch_sup_cpu, fetch_sup_libc) =
                aube_manifest::effective_supported_architectures(&manifest, &ws_config_shared);
            let fetch_supported_arch = aube_resolver::SupportedArchitectures {
                os: fetch_sup_os,
                cpu: fetch_sup_cpu,
                libc: fetch_sup_libc,
                ..Default::default()
            };
            // Each imported (dep_path, index) feeds the GVS-prewarm
            // materializer running concurrently with the rest of fetch.
            /*
             * Materialize channel sized from the cross run learned
             * recommendation when available, falling back to the
             * static default. Tokio mpsc cap is fixed at
             * construction so the only knob we can turn here is
             * the initial size for this process. Bounds 256 to
             * 16384 cap RAM and floor progress.
             */
            let (materialize_tx, materialize_rx) = materialize_channel();
            // Clone the shared deprecations accumulator into the
            // spawned task. The install command reads it back after
            // `filter_graph` prunes the post-resolve graph.
            let fetch_deprecations_tx = deprecations.clone();
            let fetch_handle = tokio::spawn(async move {
                /*
                 * Adaptive tarball concurrency. Loaded from the
                 * cross run persistent store when available so the
                 * limiter starts where a previous run converged
                 * instead of cold ramping from the ceiling. Falls
                 * back to seed 256 (h2 stream cap) on first ever
                 * run. Floor 4 keeps progress under continuous
                 * 429 / 503. Persisted back at end of fetch phase
                 * so the next invocation benefits.
                 */
                // Honor user-configured `networkConcurrency` (or
                // `AUBE_NETWORK_CONCURRENCY` env override) as the
                // seed. Adaptive grow/shrink still operate around
                // it. Floor 4 keeps progress under continuous
                // throttling regardless of seed.
                let tarball_seed = fetch_network_concurrency.max(4);
                let tarball_max = tarball_seed.max(256);
                let persistent = aube_util::adaptive::global_persistent_state();
                let semaphore = match persistent.as_ref() {
                    Some(state) => aube_util::adaptive::AdaptiveLimit::from_persistent(
                        state,
                        "tarball:default",
                        tarball_seed,
                        4,
                        tarball_max,
                    ),
                    None => aube_util::adaptive::AdaptiveLimit::new(tarball_seed, 4, tarball_max),
                };
                let semaphore_for_persist = std::sync::Arc::clone(&semaphore);
                let persistent_for_save = persistent.clone();
                // Hoist env-driven flags out of the per-tarball loop.
                let streaming_sha512_enabled =
                    std::env::var_os("AUBE_DISABLE_STREAMING_SHA512").is_none();
                let tarball_stream_enabled =
                    std::env::var_os("AUBE_DISABLE_TARBALL_STREAM").is_none();
                // JoinSet over bare Vec<JoinHandle>. If the first
                // fetch errors and we return via `?`, a plain Vec
                // drops the remaining JoinHandles which detaches the
                // tasks. They keep fetching tarballs and writing
                // to the CAS while the CLI has already errored.
                // JoinSet aborts every outstanding task on drop,
                // matches the pattern ensure_dep_scripts uses.
                let mut handles: tokio::task::JoinSet<
                    miette::Result<(String, aube_store::PackageIndex)>,
                > = tokio::task::JoinSet::new();
                let mut indices: BTreeMap<String, aube_store::PackageIndex> = BTreeMap::new();
                let mut cached_count = 0usize;
                // Drives the resolving-phase denominator estimate.
                // `received + pkg.pending` is a non-strict lower bound
                // on the final resolved-package count; raising it via
                // `set_total_floor` makes the bar fill as the
                // BFS-frontier high-water mark grows. Tracked locally
                // because the resolver's view is per-send, not a
                // single shared atomic.
                let mut resolved_received: usize = 0;

                while let Some(pkg) = resolved_rx.recv().await {
                    if let Some(ref msg) = pkg.deprecated {
                        fetch_deprecations_tx.lock().unwrap().push(
                            crate::deprecations::DeprecationRecord {
                                name: pkg.name.clone(),
                                version: pkg.version.clone(),
                                dep_path: pkg.dep_path.clone(),
                                message: msg.clone(),
                            },
                        );
                    }
                    // Each resolved package bumps the overall denominator by
                    // one. Cached packages are immediately credited against
                    // the numerator; missing ones get a transient child row.
                    //
                    // Bumping the denominator *before* the platform-deferred
                    // skip below is intentional: the catch-up pass (after
                    // `filter_graph`) credits surviving deferred packages
                    // against the numerator, and skipping the increment
                    // here would let the numerator overrun the denominator
                    // (the historical "2/1 packages" display bug). The
                    // overcount on dropped optionals is reconciled by a
                    // single `set_total(graph.packages.len())` after
                    // `filter_graph` runs.
                    resolved_received += 1;
                    if let Some(p) = fetch_progress.as_ref() {
                        p.inc_total(1);
                        // Raise the resolving-phase denominator floor
                        // toward the resolver's current frontier so
                        // the bar fills against a meaningful target
                        // instead of an empty placeholder. Stamping
                        // the frontier on each `ResolvedPackage`
                        // keeps the protocol shape unchanged.
                        p.set_total_floor(resolved_received + pkg.pending);
                        if let Some(sz) = pkg.unpacked_size {
                            p.inc_estimated_bytes(&pkg.dep_path, sz);
                        }
                    }

                    // Defer platform-mismatched registry packages to
                    // the post-filter_graph catch-up pass: almost all
                    // of them are optional natives that `filter_graph`
                    // is about to drop, so fetching up front would just
                    // waste bandwidth. Local `file:`/`link:` deps
                    // always fetch here — they carry empty platform
                    // arrays and `is_supported` treats them as
                    // unconstrained.
                    if pkg.local_source.is_none()
                        && !aube_resolver::is_supported(
                            &pkg.os,
                            &pkg.cpu,
                            &pkg.libc,
                            &fetch_supported_arch,
                        )
                    {
                        tracing::debug!(
                            "deferring tarball fetch for {}@{}: platform mismatch (catch-up will cover survivors)",
                            pkg.name,
                            pkg.version
                        );
                        continue;
                    }

                    // Local (`file:` / `link:`) deps materialize from
                    // disk, not the registry — short-circuit the
                    // tarball pipeline.
                    if let Some(ref local) = pkg.local_source {
                        match import_local_source(
                            &fetch_store,
                            &fetch_project_root,
                            local,
                            Some(&fetch_local_client),
                            fetch_ignore_scripts,
                            fetch_git_prepare_depth,
                            fetch_inherited_build_policy.clone(),
                            &fetch_git_shallow_hosts,
                            &pkg.name,
                            &pkg.version,
                        )
                        .await
                        {
                            Ok(Some(index)) => {
                                // Send failure means the materializer
                                // task died. Bail now instead of
                                // continuing to import tarballs into a
                                // half-wired virtual store.
                                materialize_tx
                                    .send((pkg.dep_path.clone(), index.clone()))
                                    .await
                                    .map_err(|_| {
                                        miette!("materializer task exited before fetch finished")
                                    })?;
                                indices.insert(pkg.dep_path, index);
                                cached_count += 1;
                                if let Some(p) = fetch_progress.as_ref() {
                                    p.inc_reused(1);
                                }
                            }
                            Ok(None) => {
                                if let Some(p) = fetch_progress.as_ref() {
                                    p.inc_reused(1);
                                }
                            }
                            Err(e) => return Err(e),
                        }
                        continue;
                    }

                    // Check index cache first. `registry_name()` is
                    // the real package name on the registry — equal
                    // to `name` for the common case, and the alias's
                    // real target for npm-alias entries (where the
                    // alias-qualified name would miss the cache and
                    // later 404 the tarball fetch). Integrity is part
                    // of the cache key so a github-sourced tarball
                    // under the same (name, version) can't return the
                    // registry-cached file list.
                    let pkg_registry_name = pkg.registry_name().to_string();
                    if let Some(index) = fetch_store.load_index(
                        &pkg_registry_name,
                        &pkg.version,
                        pkg.integrity.as_deref(),
                    ) {
                        materialize_tx
                            .send((pkg.dep_path.clone(), index.clone()))
                            .await
                            .map_err(|_| {
                                miette!("materializer task exited before fetch finished")
                            })?;
                        indices.insert(pkg.dep_path, index);
                        cached_count += 1;
                        if let Some(p) = fetch_progress.as_ref() {
                            p.inc_reused(1);
                        }
                        continue;
                    }

                    let sem = semaphore.clone();
                    let store = fetch_store.clone();
                    let client = tarball_client.clone();
                    let row = fetch_progress
                        .as_ref()
                        .map(|p| p.start_fetch(&pkg.name, &pkg.version));
                    let bytes_progress = fetch_progress.clone();

                    handles.spawn(async move {
                        let _row = row;
                        let _diag_tar = aube_util::diag::Span::new(aube_util::diag::Category::Fetch, "tarball")
                            .with_meta_fn(|| format!(r#"{{"name":{},"version":{}}}"#,
                                aube_util::diag::jstr(&pkg.name), aube_util::diag::jstr(&pkg.version)));
                        let _diag_tar_inflight = aube_util::diag::inflight(aube_util::diag::Slot::Tar);
                        let permit_wait = std::time::Instant::now();
                        let permit = sem.acquire().await;
                        let permit_wait_ms = permit_wait.elapsed();
                        let pkg_id_for_diag = format!("{}@{}", pkg.name, pkg.version);
                        if permit_wait_ms.as_millis() > 1 {
                            aube_util::diag::event_lazy(aube_util::diag::Category::Fetch, "tarball_permit_wait", permit_wait_ms, || format!(r#"{{"name":{}}}"#, aube_util::diag::jstr(&pkg.name)));
                        }
                        aube_util::diag::attribute_wait(
                            aube_util::diag::Slot::Tar,
                            &pkg_id_for_diag,
                            permit_wait_ms,
                        );
                        let _tar_holder = aube_util::diag::register_holder(
                            aube_util::diag::Slot::Tar,
                            &pkg_id_for_diag,
                        );
                        let url = pkg.tarball_url.clone().unwrap_or_else(|| {
                            client.tarball_url(&pkg_registry_name, &pkg.version)
                        });

                        tracing::trace!("Fetching {}@{}", pkg.name, pkg.version);

                        let pkg_display_name = pkg.name.clone();
                        let pkg_version = pkg.version.clone();
                        let dep_path = pkg.dep_path.clone();
                        let integrity = pkg.integrity.clone();

                        let stream_eligible = tarball_stream_enabled
                            && integrity
                                .as_deref()
                                .is_none_or(|s| s.starts_with("sha512-"));
                        aube_util::diag::instant_lazy(aube_util::diag::Category::Fetch, "tarball_path", || format!(r#"{{"streaming":{},"name":{}}}"#, stream_eligible, aube_util::diag::jstr(&pkg.name)));
                        if stream_eligible {
                            let streamed = crate::commands::install::lifecycle::fetch_and_import_tarball_streaming(
                                &client,
                                &store,
                                &url,
                                &pkg_display_name,
                                &pkg_registry_name,
                                &pkg_version,
                                integrity.as_deref(),
                                fetch_verify_integrity,
                                fetch_strict_integrity,
                                fetch_strict_pkg_content_check,
                            )
                            .await;
                            let (index, bytes_len) = match streamed {
                                Ok(v) => {
                                    permit.record_success();
                                    v
                                }
                                Err(e) => {
                                    if e.is_throttle {
                                        permit.record_throttle();
                                    } else {
                                        permit.record_cancelled();
                                    }
                                    return Err(e.into());
                                }
                            };
                            if let Some(p) = bytes_progress.as_ref() {
                                p.inc_downloaded_bytes(bytes_len);
                            }
                            return Ok::<_, miette::Report>((dep_path, index));
                        }

                        let fetch_outcome = if streaming_sha512_enabled {
                            client
                                .fetch_tarball_bytes_streaming_sha512(&url)
                                .await
                                .map(|(b, d)| (b, Some(d)))
                                .map_err(|e| {
                                    let throttled = e.is_throttle();
                                    (
                                        miette!(
                                            "failed to fetch {}@{}: {e}{}",
                                            pkg.name,
                                            pkg.version,
                                            crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                                        ),
                                        throttled,
                                    )
                                })
                        } else {
                            client.fetch_tarball_bytes(&url).await.map(|b| (b, None)).map_err(|e| {
                                let throttled = e.is_throttle();
                                (
                                    miette!(
                                        "failed to fetch {}@{}: {e}{}",
                                        pkg.name,
                                        pkg.version,
                                        crate::dep_chain::format_chain_for(&pkg.name, &pkg.version)
                                    ),
                                    throttled,
                                )
                            })
                        };
                        let (bytes, streamed_digest) = match fetch_outcome {
                            Ok(v) => {
                                permit.record_success();
                                v
                            }
                            Err((report, throttled)) => {
                                if throttled {
                                    permit.record_throttle();
                                } else {
                                    permit.record_cancelled();
                                }
                                return Err(report);
                            }
                        };
                        if let Some(p) = bytes_progress.as_ref() {
                            p.inc_downloaded_bytes(bytes.len() as u64);
                        }

                        let (index, _) = run_import_on_blocking(
                            store,
                            bytes,
                            streamed_digest,
                            pkg_display_name,
                            pkg_registry_name,
                            pkg_version,
                            integrity,
                            fetch_verify_integrity,
                            fetch_strict_integrity,
                            fetch_strict_pkg_content_check,
                        )
                        .await?;

                        Ok::<_, miette::Report>((dep_path, index))
                    });
                }

                // Collect all fetch results via JoinSet. Drop on
                // error aborts outstanding siblings.
                let fetch_count = handles.len();
                while let Some(joined) = handles.join_next().await {
                    let (dep_path, index) = joined.into_diagnostic()??;
                    materialize_tx
                        .send((dep_path.clone(), index.clone()))
                        .await
                        .map_err(|_| miette!("materializer task exited before fetch finished"))?;
                    indices.insert(dep_path, index);
                }
                // Explicitly drop the materialize sender so the
                // materializer consumer sees the channel close and
                // exits its receive loop.
                drop(materialize_tx);
                if let Some(state) = persistent_for_save.as_ref() {
                    semaphore_for_persist.persist(state, "tarball:default");
                }
                Ok::<_, miette::Report>((indices, cached_count, fetch_count))
            });

            // Run resolution (this streams packages to the fetch coordinator).
            // `existing_for_resolver` is `Some` when Fix / Prefer parsed a
            // lockfile cleanly; the resolver reuses already-pinned versions
            // for unchanged specs and only re-resolves entries whose spec
            // drifted. `No` mode (`--no-frozen-lockfile`) intentionally
            // stays at `None` so the user gets the fresh resolve they
            // asked for.
            aube_util::diag::instant(aube_util::diag::Category::Install, "resolve_begin", None);
            let _diag_resolve =
                aube_util::diag::Span::new(aube_util::diag::Category::Install, "phase_resolve");
            let resolve_result = if has_workspace {
                resolver
                    .resolve_workspace(&manifests, existing_for_resolver, &ws_package_versions)
                    .await
            } else {
                resolver.resolve(&manifest, existing_for_resolver).await
            }
            .map_err(miette::Report::new)
            .wrap_err("failed to resolve dependencies");

            if resolve_result.is_err() {
                fetch_handle.abort();
                return resolve_result.map(|_| unreachable!());
            }
            let mut graph = resolve_result.unwrap();
            // Snapshot per-direct-dep packument facts before dropping the
            // resolver — its `cache` field owns the only copy and the
            // install summary printer runs much later, well after the
            // channel-closing drop below.
            direct_dep_info = resolver.direct_dep_info(&graph);
            // Drop the resolver to close the channel, signaling the fetch
            // coordinator to finish, then drain the readPackage stderr
            // forwarders so every `ctx.log` record from resolve flushes
            // to stdout before afterAllResolved emits its own pnpm:hook
            // records. Doing this in the order drop → drain → hook keeps
            // resolve-time logs strictly ahead of afterAllResolved-time
            // logs in the ndjson stream.
            drop(resolver);
            crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
            crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, &cwd, &mut graph)
                .await?;
            // Overlay per-package metadata the resolver can't recover
            // from abbreviated (corgi) packuments — `license`,
            // `funding_url`, bun's `configVersion` — from the
            // existing lockfile when one was on disk. Without this,
            // `aube install --no-frozen-lockfile` drops those fields
            // on every re-resolve even though the resolved versions
            // didn't change, which churns the lockfile diff against
            // formats (npm, bun) that preserve them.
            // Reuse the pre-parsed lockfile when the resolver already
            // loaded it for seeding (Fix/Prefer modes). Skips a second
            // YAML parse pass over the same 5-50 KB file.
            if let Some((prior, _)) = lockfile_pre_parse.as_ref() {
                graph.overlay_metadata_from(prior);
            } else if let Ok(prior) =
                parse_lockfile_dir_remapped(&lockfile_dir, &lockfile_importer_key, &manifest)
            {
                graph.overlay_metadata_from(&prior);
            }
            tracing::debug!("Resolved {} packages", graph.packages.len());
            // Seed the chain index for diagnostic enrichment. Any
            // post-resolver error wrapping `(name, version)` via
            // `crate::dep_chain::format_chain_for` now sees a
            // chain back to the importer.
            crate::dep_chain::set_active(&graph);
            aube_registry::slow_metadata::flush_summary();
            if let Some(p) = prog_ref {
                p.set_phase("fetching");
            }
            tracing::debug!("phase:resolve (fresh) {:.1?}", phase_start.elapsed());
            phase_timings.record("resolve", phase_start.elapsed());
            drop(_diag_resolve);
            aube_util::diag::instant(aube_util::diag::Category::Install, "resolve_end", None);

            // fetch_handle streams imported (dep_path, index) tuples
            // into the materializer, which reflinks each into
            // ~/.cache/aube/virtual-store. Used to run serially after
            // fetch as link step 1. Now overlaps with in-flight
            // downloads and post-resolve bookkeeping. Link step 1
            // below hits pkg_nm_dir.exists() fast path and only writes
            // the per-project .aube/<dep_path> symlink.
            let materialize_phase_start = std::time::Instant::now();
            let materialize_graph_arc = std::sync::Arc::new(graph.clone());
            let materialize_strategy = resolve_link_strategy(&cwd, &settings_ctx, planned_gvs)?;
            let (materialize_patches, materialize_patch_hashes) =
                crate::patches::load_patches_for_linker(&cwd)?;
            let materialize_inputs = GvsPrewarmInputs {
                graph: materialize_graph_arc.clone(),
                store: store.clone(),
                cwd: cwd.clone(),
                virtual_store_dir_max_length,
                link_strategy: materialize_strategy,
                link_concurrency: link_concurrency_setting,
                patches: materialize_patches,
                patch_hashes: materialize_patch_hashes,
                node_version: node_version_for_prewarm.clone(),
                build_policy: build_policy_for_prewarm.clone(),
                use_global_virtual_store_override,
            };
            aube_util::diag::instant(
                aube_util::diag::Category::Install,
                "materialize_spawn",
                None,
            );
            let materialize_handle = spawn_gvs_prewarm(materialize_inputs, materialize_rx);

            // On fetch err, await the materializer (don't abort): the
            // failing fetch task drops its `tx`, so the materializer's
            // `rx` closes and it exits naturally. Awaiting first lets a
            // real materializer error (the likely root cause of a
            // generic "materializer task exited..." fetch err) surface
            // instead.
            let _diag_fetch_wait =
                aube_util::diag::Span::new(aube_util::diag::Category::Install, "phase_fetch_await");
            let fetch_phase_start = std::time::Instant::now();
            let fetch_result = match fetch_handle.await.into_diagnostic()? {
                Ok(v) => v,
                Err(e) => {
                    return Err(combine_install_pipeline_errors(materialize_handle, e).await);
                }
            };
            let (canonical_indices, mut cached, mut fetched) = fetch_result;
            tracing::debug!(
                "phase:fetch {:.1?} ({fetched} packages, {cached} cached)",
                fetch_phase_start.elapsed()
            );
            phase_timings.record("fetch", fetch_phase_start.elapsed());
            drop(_diag_fetch_wait);
            aube_util::diag::instant(aube_util::diag::Category::Install, "fetch_await_end", None);
            // Drain the materializer; its stats get rolled into the
            // final link stats below. Errors abort the install just like
            // a failing link phase would.
            let _diag_mat_wait = aube_util::diag::Span::new(
                aube_util::diag::Category::Install,
                "phase_materialize_await",
            );
            let (prewarm_stats, prewarm_hashes_from_task) =
                materialize_handle.await.into_diagnostic()??;
            drop(_diag_mat_wait);
            aube_util::diag::instant(
                aube_util::diag::Category::Install,
                "materialize_await_end",
                None,
            );
            prewarm_graph_hashes = prewarm_hashes_from_task;
            tracing::debug!(
                "phase:prewarm-gvs {:.1?} ({} packages, {} files)",
                materialize_phase_start.elapsed(),
                prewarm_stats.packages_linked,
                prewarm_stats.files_linked,
            );
            phase_timings.record("prewarm_gvs", materialize_phase_start.elapsed());

            // The fetch coordinator streamed `ResolvedPackage`s from the
            // resolver's *first pass*, which uses canonical `name@version`
            // dep_paths. After the resolver's peer-context post-pass, the
            // graph has contextualized dep_paths — same underlying files,
            // but the indices map needs to be re-keyed to match so the
            // linker can find each variant by the dep_path on its
            // `LockedPackage`. Multiple contextualized variants of the
            // same canonical package share a single set of files, so
            // cloning the PackageIndex is cheap relative to re-extraction.
            let mut indices = remap_indices_to_contextualized(&canonical_indices, &graph);

            // Write the lockfile in whatever format the project was
            // already using. If no lockfile existed, create aube's
            // default `aube-lock.yaml`. Skipped entirely when
            // `lockfile=false`.
            if lockfile_enabled {
                // When `lockfileIncludeTarballUrl=true`, record the
                // registry tarball URL on every registry-sourced
                // package so the writer can embed it in
                // `resolution.tarball:`. The client's `tarball_url`
                // helper honors per-scope registry overrides read
                // from `.npmrc`, so a `@mycorp:registry=...` override
                // still routes scoped packages through the right host.
                // Non-registry packages (local_source Some) already
                // carry their own URL and are left alone.
                if lockfile_include_tarball_url {
                    graph.settings.lockfile_include_tarball_url = true;
                    for pkg in graph.packages.values_mut() {
                        if pkg.local_source.is_some() {
                            continue;
                        }
                        // Preserve any URL already present — the npm
                        // lockfile reader stashes the `resolved:` URL
                        // for aliased entries at parse time because
                        // `(alias, version)` doesn't resolve against
                        // the registry.
                        if pkg.tarball_url.is_none() {
                            pkg.tarball_url = Some(
                                post_fetch_client.tarball_url(pkg.registry_name(), &pkg.version),
                            );
                        }
                    }
                }
                let write_kind = source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube);
                if shared_workspace_lockfile || !has_workspace {
                    let written_path = write_lockfile_dir_remapped(
                        &lockfile_dir,
                        &lockfile_importer_key,
                        &graph,
                        &manifest,
                        write_kind,
                    )
                    .into_diagnostic()
                    .wrap_err("failed to write lockfile")?;
                    // Log the basename (matches the format resolve.bats and
                    // similar tests assert against — e.g. "Wrote aube-lock.yaml").
                    tracing::debug!(
                        "Wrote {}",
                        written_path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| written_path.display().to_string())
                    );
                } else {
                    write_per_project_lockfiles(&cwd, &graph, &manifests, write_kind)?;
                }
            } else {
                tracing::debug!("lockfile=false: skipping lockfile write");
            }

            // Trim the in-memory graph down to host-installable optionals
            // before it reaches the linker. When the resolver widened its
            // platform filter for aube-lock.yaml, the graph (and now the
            // lockfile) carries native packages for every major platform;
            // `node_modules` must still only get the host's. Mirrors the
            // filter pass the lockfile-happy branch above runs against a
            // parsed lockfile. A no-op when the manifest didn't trigger
            // widening (graph was already host-only).
            let (sup_os, sup_cpu, sup_libc) =
                aube_manifest::effective_supported_architectures(&manifest, &ws_config_shared);
            let install_supported_architectures = aube_resolver::SupportedArchitectures {
                os: sup_os,
                cpu: sup_cpu,
                libc: sup_libc,
                ..Default::default()
            };
            let install_ignored_optional = aube_manifest::effective_ignored_optional_dependencies(
                &manifest,
                &ws_config_shared,
            );
            aube_resolver::platform::filter_graph(
                &mut graph,
                &install_supported_architectures,
                &install_ignored_optional,
            );

            // Reconcile the progress denominator and the running
            // estimated-download total. The streaming pass bumped
            // `inc_total` once per *resolved* package and recorded
            // each `unpacked_size`; `filter_graph` just dropped the
            // platform-mismatched optionals, so both totals overcount
            // by the culled entries (the historical "stays at 90%"
            // and over-inflated `~X MB` segments). Resetting against
            // the surviving graph produces a stable cur/total ratio
            // and a size estimate that reflects only what will
            // actually install.
            if let Some(p) = prog_ref {
                p.set_total(graph.packages.len());
                p.reconcile_estimated_bytes(graph.packages.keys());
            }

            // Catch-up fetch: the streaming coordinator deferred
            // platform-mismatched registry tarballs on the assumption
            // `filter_graph` would drop them. Anything still in
            // `graph.packages` without a store index is a survivor
            // (i.e. reached via a non-optional edge) and needs its
            // tarball before the linker runs. In practice this set is
            // usually empty: platform-constrained packages are almost
            // always `optionalDependencies`, and `filter_graph` culls
            // those. The rare non-empty case is a broken package that
            // declares `os`/`cpu` without marking itself optional — we
            // still install it with a warning, matching pnpm's
            // `packageIsInstallable` behavior.
            let missing_packages: BTreeMap<String, aube_lockfile::LockedPackage> = graph
                .packages
                .iter()
                .filter(|(dep_path, _)| !indices.contains_key(*dep_path))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !missing_packages.is_empty() {
                tracing::debug!(
                    "catch-up fetch for {} package(s) deferred by the streaming filter but kept by filter_graph",
                    missing_packages.len()
                );
                let catchup_start = std::time::Instant::now();
                let cwd_for_catchup_client = cwd.clone();
                let catchup_network_mode = opts.network_mode;
                let (catchup_indices, catchup_cached, catchup_fetched) = fetch_packages_with_root(
                    &missing_packages,
                    &store,
                    || {
                        std::sync::Arc::new(
                            make_client(&cwd_for_catchup_client)
                                .with_network_mode(catchup_network_mode),
                        )
                    },
                    prog_ref,
                    &cwd,
                    &aube_dir,
                    /*materialize_tx=*/ None,
                    /*skip_already_linked_shortcut=*/ has_workspace,
                    virtual_store_dir_max_length,
                    opts.ignore_scripts,
                    network_concurrency_setting,
                    verify_store_integrity_setting,
                    strict_store_integrity_setting,
                    strict_store_pkg_content_check_setting,
                    opts.git_prepare_depth,
                    inherited_build_policy_for_git_prepare.clone(),
                    resolve_git_shallow_hosts(&settings_ctx),
                )
                .await?;
                indices.extend(catchup_indices);
                cached += catchup_cached;
                fetched += catchup_fetched;
                phase_timings.record("catchup_fetch", catchup_start.elapsed());
            }

            (graph, indices, cached, fetched)
        }
        Err(aube_lockfile::Error::NotFound(_)) => {
            // Reachable when mode == Frozen, strict_no_lockfile == true,
            // and no lockfile is on disk. Today that's `aube ci` /
            // `aube clean-install`, which match `npm ci` semantics.
            return Err(miette!(
                "no lockfile found and --frozen-lockfile is set\n\
                 help: commit pnpm-lock.yaml to your repository, or run \
                 `aube install --no-frozen-lockfile` to generate one"
            ));
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    tracing::debug!("Packages: {cached_count} cached, {fetch_count} fetched");

    // `cleanupUnusedCatalogs` (gated by the setting) rewrites
    // `aube-workspace.yaml` / `pnpm-workspace.yaml` to drop entries no
    // importer references. Runs once after we have the final graph so
    // the same helper covers both lockfile-read and fresh-resolve
    // paths (the `--lockfile-only` short-circuit above already handled
    // its own return). Pruning is independent of the lockfile write
    // below since the resolver already recorded the used subset in
    // `graph.catalogs`.
    maybe_cleanup_unused_catalogs(&cwd, &settings_ctx, &workspace_catalogs, &graph.catalogs)?;

    // 5a. Under `strict-peer-dependencies=true`, scan the resolved
    //     graph for unmet required peers and fail the install with the
    //     list. Default (strict=false) is silent, matching bun/npm/yarn
    //     — the previous pnpm-style warn-on-every-mismatch default
    //     produced a lot of noise on real-world trees and buried the
    //     genuinely actionable ones. Optional peers
    //     (peerDependenciesMeta.optional) are skipped either way, and
    //     `peerDependencyRules` escape hatches filter out matches
    //     before the strict check fires.
    //
    //     The `PeerDependencyRules::resolve` call is gated on strict
    //     because it reads across package.json / .npmrc /
    //     pnpm-workspace.yaml to build the three escape-hatch lists —
    //     allocation + file-source iteration nobody consumes on the
    //     silent default path.
    if resolve_strict_peer_dependencies(&settings_ctx) {
        let peer_rules = PeerDependencyRules::resolve(&manifest, &settings_ctx);
        check_unmet_peers(&graph, &peer_rules)?;
    }

    // 5b. Apply --prod / --dev / --no-optional filters. Drops the corresponding
    //     direct dep roots from every importer and prunes transitive packages
    //     only reachable through them. The filtered graph is what gets passed
    //     to the linker, so node_modules won't contain the excluded deps.
    //     The lockfile on disk is untouched.
    let mut graph_for_link = if opts.dep_selection.is_filtered() {
        let before = graph.packages.len();
        let sel = opts.dep_selection;
        let filtered = graph.filter_deps(|d| {
            if sel.prod_only() && d.dep_type == aube_lockfile::DepType::Dev {
                return false;
            }
            if sel.dev_only() && d.dep_type != aube_lockfile::DepType::Dev {
                return false;
            }
            if sel.skip_optional() && d.dep_type == aube_lockfile::DepType::Optional {
                return false;
            }
            true
        });
        let dropped = before - filtered.packages.len();
        if dropped > 0 {
            tracing::debug!("{}: skipping {dropped} packages", sel.label());
        }
        filtered
    } else {
        graph.clone()
    };
    if !opts.workspace_filter.is_empty() {
        graph_for_link = filter_graph_to_workspace_selection(
            &cwd,
            &workspace_packages,
            &graph_for_link,
            &opts.workspace_filter,
        )?;
    } else if has_workspace && !link_all_workspace_importers {
        graph_for_link = filter_graph_to_importers(&graph_for_link, ["."]);
    }

    // 5c. Validate root + dependency `engines.node` constraints against
    //     the current Node version. Runs against `graph_for_link` so
    //     `--prod` / `--no-optional` excluded packages don't trip
    //     `engine-strict`: a dev-only dep pinning Node >=20 should not
    //     block a Node 18 production install. Defaults to warning on
    //     mismatch; fails the install when `engine-strict` is set in
    //     `.npmrc`. Packages with unparseable versions or ranges are
    //     treated as "no opinion" so malformed fields or unusual Node
    //     builds don't block installs.
    // 5c. Resolve node version, build policy, and validate engines.
    //     All three go through the `settings_ctx` loaded once at the
    //     top of `run`, so there's a single `.npmrc` read and a
    //     single workspace-yaml parse for the whole install.
    let engine_strict = aube_settings::resolved::engine_strict(&settings_ctx);
    // `childConcurrency` caps how many dep lifecycle scripts run in
    // parallel during the post-link allowBuilds phase. Matches pnpm's
    // default of 5 when unset. Zero gets clamped up to 1 inside
    // `run_dep_lifecycle_scripts` so a malformed config can't wedge
    // the install.
    let child_concurrency = aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
    let (jail_policy, jail_policy_warnings) =
        JailBuildPolicy::from_settings(&settings_ctx, &ws_config_shared);
    let node_version_override = aube_settings::resolved::node_version(&settings_ctx);
    let node_version = crate::engines::resolve_node_version(node_version_override.as_deref());
    crate::engines::run_checks(
        &aube_dir,
        &manifest,
        &manifests,
        &graph_for_link,
        &package_indices,
        node_version.as_deref(),
        engine_strict,
        virtual_store_dir_max_length,
    )?;

    // Emit policy-config warnings regardless of `--ignore-scripts`.
    // User wants to know about typos in `allowBuilds` even if scripts
    // will not run, otherwise they reenable scripts later and wonder
    // why nothing runs. Bar is active here (set_phase=linking comes
    // soon, set_phase=fetching already ran). Raw eprintln smears
    // output across bar frames. Route through safe_eprintln which
    // pauses the bar and holds the terminal lock for atomic output.
    for w in &policy_warnings {
        crate::progress::safe_eprintln(&format!("warn: {w}"));
    }
    for w in &jail_policy_warnings {
        crate::progress::safe_eprintln(&format!("warn: {w}"));
    }

    // 6. Link node_modules
    let phase_start = std::time::Instant::now();
    // Resolve `packageImportMethod`. CLI override wins, then
    // `.npmrc` / `pnpm-workspace.yaml`, then `auto` (detect). Unknown
    // CLI values hard-error (preserving the explicit `--package-import-method`
    // diagnostic). Settings-file values flow through the generated typed
    // accessor, which collapses unknown values to `None` so they behave
    // like an absent setting.
    let strategy = resolve_link_strategy(&cwd, &settings_ctx, planned_gvs)?;
    if let Some(p) = prog_ref {
        p.set_phase("linking");
    }
    tracing::debug!("Link strategy: {strategy:?}");

    let shamefully_hoist = aube_settings::resolved::shamefully_hoist(&settings_ctx);
    let public_hoist_pattern = aube_settings::resolved::public_hoist_pattern(&settings_ctx);
    let hoist = aube_settings::resolved::hoist(&settings_ctx);
    let hoist_pattern = aube_settings::resolved::hoist_pattern(&settings_ctx);
    let hoist_workspace_packages = aube_settings::resolved::hoist_workspace_packages(&settings_ctx);
    let dedupe_direct_deps = aube_settings::resolved::dedupe_direct_deps(&settings_ctx);
    let virtual_store_only = aube_settings::resolved::virtual_store_only(&settings_ctx);
    // Resolve the layout mode. CLI override wins, then `.npmrc` /
    // `pnpm-workspace.yaml`, then default (Isolated). `pnp` is a
    // hard error regardless of source — we don't ship a PnP runtime,
    // so accepting it would silently mislead. The CLI path hard-errors
    // on an unknown value so typos surface immediately; settings-file
    // values with an unknown spelling fall through to the generated
    // default today, so a `.npmrc` typo degrades to `isolated`
    // without a warning. Worth revisiting if that ever bites.
    let reject_pnp =
        miette!("node-linker=pnp is not supported by aube; use `isolated` (default) or `hoisted`");
    let node_linker_cli = aube_settings::values::string_from_cli("nodeLinker", settings_ctx.cli);
    let node_linker = if let Some(cli) = node_linker_cli.as_deref() {
        let trimmed = cli.trim();
        if trimmed.eq_ignore_ascii_case("pnp") {
            return Err(reject_pnp);
        }
        trimmed.parse::<aube_linker::NodeLinker>().map_err(|_| {
            miette!("unknown --node-linker value `{cli}`; expected `isolated` or `hoisted`")
        })?
    } else {
        match aube_settings::resolved::node_linker(&settings_ctx) {
            aube_settings::resolved::NodeLinker::Pnp => return Err(reject_pnp),
            aube_settings::resolved::NodeLinker::Hoisted => aube_linker::NodeLinker::Hoisted,
            aube_settings::resolved::NodeLinker::Isolated => aube_linker::NodeLinker::Isolated,
        }
    };
    tracing::debug!("node-linker: {:?}", node_linker);

    let mut linker = aube_linker::Linker::new(store.as_ref(), strategy)
        .with_shamefully_hoist(shamefully_hoist)
        .with_public_hoist_pattern(&public_hoist_pattern)
        .with_hoist(hoist)
        .with_hoist_pattern(&hoist_pattern)
        .with_hoist_workspace_packages(hoist_workspace_packages)
        .with_dedupe_direct_deps(dedupe_direct_deps)
        .with_virtual_store_dir_max_length(virtual_store_dir_max_length)
        .with_node_linker(node_linker)
        .with_link_concurrency(link_concurrency_setting)
        .with_virtual_store_only(virtual_store_only)
        .with_modules_dir_name(modules_dir_name.clone())
        .with_aube_dir_override(aube_dir.clone());
    if let Some(enabled) = use_global_virtual_store_override {
        linker = linker.with_use_global_virtual_store(enabled);
    }

    // Patches for delta-fingerprint folding and linker injection.
    // Hoisted ahead of subtree-hash so re-patched packages land in
    // the `changed` bucket and side-effects skip can't trust a stale
    // marker.
    let (patches_for_linker, patch_hashes) = crate::patches::load_patches_for_linker(&cwd)?;

    // Compute leaf + subtree hashes together when both are needed.
    // Linker invalidation reads `current_subtree_hashes`; the late
    // state writeback reads the leaf map. Sharing the BLAKE3 leaf
    // pass cuts a duplicate `compute_package_hashes` traversal.
    let (current_leaf_hashes, current_subtree_hashes) = if !virtual_store_only
        && matches!(node_linker, aube_linker::NodeLinker::Isolated)
        && !opts.dep_selection.is_filtered()
        && opts.workspace_filter.is_empty()
    {
        let (leaf, subtree) =
            delta::compute_leaf_and_subtree_hashes(&graph_for_link, &patch_hashes);
        (Some(leaf), Some(subtree))
    } else {
        (None, None)
    };
    if !linker.uses_global_virtual_store()
        && let Some(current_subtree_hashes) = current_subtree_hashes.as_ref()
        && let Some(prior_subtrees) = state::read_state_subtree_hashes(&cwd)
    {
        let touched = delta::changed_subtree_roots(&prior_subtrees, current_subtree_hashes);
        let invalidated =
            invalidate_changed_aube_entries(&aube_dir, &touched, virtual_store_dir_max_length);
        if invalidated > 0 {
            tracing::debug!("delta: invalidated {invalidated} changed .aube entry/entries");
        }
    }

    // 6a. Pre-compute content-addressed virtual-store hashes.
    //     Only necessary when linking into the shared global virtual
    //     store — in per-project mode (`CI=1`) the `.aube/<dep_path>`
    //     directories are already isolated so there's nothing to
    //     address. Folding engine state into the subdir name for any
    //     build-allowed package (plus every ancestor in its dep
    //     graph) keeps two projects resolving the same `(integrity,
    //     deps)` under different node / arch combos from stomping on
    //     each other; pure-JS packages with no build-allowed
    //     descendants get engine-agnostic hashes and stay shared.
    let patch_hash_fn = |name: &str, version: &str| -> Option<String> {
        let key = format!("{name}@{version}");
        patch_hashes.get(&key).cloned()
    };

    if linker.uses_global_virtual_store() {
        // Reuse the prewarm task's `compute_graph_hashes` output when
        // the link-phase graph matches what the prewarm hashed. The
        // prewarm hashed the unfiltered post-resolve graph; if no
        // dep-selection or workspace filter applied, `graph_for_link`
        // == that graph by node count + key set, so the cached
        // hashes are byte-identical to a fresh compute. Falling
        // through to a fresh compute keeps the contract simple
        // whenever the graphs diverge.
        let cached_hashes = prewarm_graph_hashes.as_ref().filter(|arc| {
            arc.node_hash.len() == graph_for_link.packages.len()
                && graph_for_link
                    .packages
                    .keys()
                    .all(|k| arc.node_hash.contains_key(k))
        });
        let graph_hashes = if let Some(arc) = cached_hashes {
            (**arc).clone()
        } else {
            let engine = node_version
                .as_deref()
                .map(aube_lockfile::graph_hash::engine_name_default);
            let allow = |name: &str, version: &str| {
                matches!(
                    build_policy.decide(name, version),
                    aube_scripts::AllowDecision::Allow
                )
            };
            aube_lockfile::graph_hash::compute_graph_hashes_with_patches(
                &graph_for_link,
                &allow,
                engine.as_ref(),
                &patch_hash_fn,
            )
        };
        linker = linker.with_graph_hashes(graph_hashes);
    }
    if !patches_for_linker.is_empty() {
        linker = linker.with_patches(patches_for_linker);
    }
    let stats = if has_workspace {
        linker
            .link_workspace(&cwd, &graph_for_link, &package_indices, &ws_dirs)
            .into_diagnostic()
            .wrap_err("failed to link workspace node_modules")?
    } else {
        linker
            .link_all(&cwd, &graph_for_link, &package_indices)
            .into_diagnostic()
            .wrap_err("failed to link node_modules")?
    };

    tracing::debug!(
        "phase:link {:.1?} ({} files)",
        phase_start.elapsed(),
        stats.files_linked
    );
    phase_timings.record("link", phase_start.elapsed());

    // Apply `dependenciesMeta.<name>.injected` overrides. Only runs in
    // workspace + isolated mode: hoisted layouts don't have a
    // `.aube/<dep_path>/` virtual store for `apply_injected` to
    // sibling-link against, and hoisted resolution already walks the
    // consumer's root-level tree so the peer-context guarantee
    // injection is meant to give is already in place. Timed
    // separately so the `phase:link` metric isn't polluted with copy
    // work. Skipped under `virtualStoreOnly` — the workspace member
    // trees that `apply_injected` writes into don't exist.
    if has_workspace
        && matches!(node_linker, aube_linker::NodeLinker::Isolated)
        && !virtual_store_only
    {
        let inject_start = std::time::Instant::now();
        let injected_count = super::inject::apply_injected(
            &cwd,
            &modules_dir_name,
            &aube_dir,
            virtual_store_dir_max_length,
            &graph_for_link,
            &manifests,
            &ws_dirs,
        )?;
        if injected_count > 0 {
            tracing::debug!(
                "phase:inject {:.1?} ({injected_count} workspace deps injected)",
                inject_start.elapsed()
            );
        }
        phase_timings.record("inject", inject_start.elapsed());
    }

    // 7. Link .bin entries (root + each workspace package).
    //    Use graph_for_link so dev-only bins aren't linked under --prod.
    //    In hoisted mode, the placement map returned from linking
    //    tells bin-resolution where each dep ended up on disk
    //    instead of assuming the `.aube/<dep_path>` convention.
    //    Skipped under `virtualStoreOnly` — the top-level
    //    `node_modules/.bin` directory is not meant to exist in that
    //    mode.
    let placements_ref = stats.hoisted_placements.as_ref();
    let phase_start = std::time::Instant::now();
    // `extendNodePath` controls whether shim scripts export `NODE_PATH`.
    // `preferSymlinkedExecutables` only matters on POSIX: `Some(true)`
    // keeps the symlink layout, `Some(false)` swaps in a shell shim so
    // `extendNodePath` can actually take effect (bare symlinks can't set
    // env vars). When the user leaves it unset, default to shim under the
    // isolated linker (NODE_PATH matters there so transitives hoisted to
    // `.aube/node_modules/` resolve from a shimmed bin) and symlink under
    // hoisted (every dep is already on the root `node_modules/` walk-up
    // path, so NODE_PATH is unnecessary). Mirrors pnpm's effective
    // default. Windows always writes cmd/ps1/sh wrappers regardless,
    // since real symlinks there need Developer Mode.
    let extend_node_path = aube_settings::resolved::extend_node_path(&settings_ctx);
    let isolated = !matches!(node_linker, aube_linker::NodeLinker::Hoisted);
    let prefer_symlinked_executables =
        aube_settings::resolved::prefer_symlinked_executables(&settings_ctx)
            .or(isolated.then_some(false));
    // Only the isolated layout has a hidden modules dir worth exposing
    // via NODE_PATH — under `node-linker=hoisted` every dep is already
    // on the top-level `node_modules/` walk-up path, so appending
    // `.aube/node_modules/` would just stuff a non-existent entry into
    // every shim. `add.rs` (global install, hoisted-shaped) passes
    // `None` for the same reason.
    let hidden_modules_dir = aube_dir.join("node_modules");
    let shim_opts = aube_linker::BinShimOptions {
        extend_node_path,
        prefer_symlinked_executables,
        hidden_modules_dir: isolated.then_some(hidden_modules_dir.as_path()),
    };
    if !virtual_store_only {
        let mut pkg_json_cache = bin_linking::PkgJsonCache::new();
        let mut ws_pkg_json_cache = bin_linking::WsPkgJsonCache::new();
        let ws_dirs_for_bins = has_workspace.then_some(&ws_dirs);
        link_bins(
            &cwd,
            &modules_dir_name,
            &aube_dir,
            &graph_for_link,
            virtual_store_dir_max_length,
            placements_ref,
            shim_opts,
            &mut pkg_json_cache,
            ws_dirs_for_bins,
            &mut ws_pkg_json_cache,
        )?;
        // Root importer's own `bin` (discussion #228). Runs after
        // `link_bins` so a self-bin overrides a same-named dep bin.
        // Self-bin targets are files in the importer's own tree — often
        // build outputs that don't exist at install time, or are
        // later restored from an `actions/upload-artifact` round-trip
        // that strips the POSIX exec bit. A POSIX shim (shell script
        // that invokes `node`) is itself `+x` and does not rely on
        // the target's exec bit, so `aube run` works in both flows.
        if let Some(bin) = manifest.extra.get("bin") {
            let root_bin_dir = cwd.join(&modules_dir_name).join(".bin");
            let self_shim_opts = aube_linker::BinShimOptions {
                prefer_symlinked_executables: Some(false),
                ..shim_opts
            };
            link_bin_entries(
                &root_bin_dir,
                &cwd,
                manifest.name.as_deref(),
                bin,
                self_shim_opts,
            )?;
        }
        if has_workspace {
            for (importer_path, deps) in &graph_for_link.importers {
                if importer_path == "." {
                    continue;
                }
                // pnpm v9 emits nested peer-context importer entries
                // (e.g. `a/node_modules/@scope/b`). Those paths are
                // reached through the workspace-to-workspace symlink
                // chain, not distinct directories to receive their own
                // `.bin`. Walking them here duplicates work on the
                // physical workspace and, at monorepo depth, pushes the
                // kernel's per-lookup symlink budget over SYMLOOP_MAX.
                if !aube_linker::is_physical_importer(importer_path) {
                    continue;
                }
                let pkg_dir = cwd.join(importer_path);
                let bin_dir = pkg_dir.join(&modules_dir_name).join(".bin");
                std::fs::create_dir_all(&bin_dir).into_diagnostic()?;
                for dep in deps {
                    if let Some(ws_dir) = ws_dirs.get(&dep.name) {
                        bin_linking::link_bins_for_workspace_dep(
                            &mut ws_pkg_json_cache,
                            &bin_dir,
                            ws_dir,
                            &dep.name,
                            shim_opts,
                        )?;
                    } else {
                        link_bins_for_dep(
                            &mut pkg_json_cache,
                            &aube_dir,
                            &bin_dir,
                            &graph_for_link,
                            &dep.dep_path,
                            &dep.name,
                            virtual_store_dir_max_length,
                            placements_ref,
                            shim_opts,
                        )?;
                    }
                }
                // Workspace member's own `bin` (discussion #228). `manifests`
                // was parsed once upstream and keys by importer relpath.
                // See the root self-bin call site for why this forces a
                // POSIX shim instead of a symlink.
                if let Some((_, member_manifest)) =
                    manifests.iter().find(|(p, _)| p == importer_path)
                    && let Some(bin) = member_manifest.extra.get("bin")
                {
                    let self_shim_opts = aube_linker::BinShimOptions {
                        prefer_symlinked_executables: Some(false),
                        ..shim_opts
                    };
                    link_bin_entries(
                        &bin_dir,
                        &pkg_dir,
                        member_manifest.name.as_deref(),
                        bin,
                        self_shim_opts,
                    )?;
                }
            }
        }
        if !opts.ignore_scripts && build_policy.has_any_allow_rule() {
            link_dep_bins(
                &aube_dir,
                &graph_for_link,
                virtual_store_dir_max_length,
                placements_ref,
                shim_opts,
                &mut pkg_json_cache,
            )?;
        }
        tracing::debug!("phase:link_bins {:.1?}", phase_start.elapsed());
        phase_timings.record("link_bins", phase_start.elapsed());
    }

    // Tear down the progress display before running post-link lifecycle
    // scripts or printing the final summary — scripts write directly to
    // stdout/stderr and would collide with an active progress bar.
    //
    // Skip the CI-mode framed summary on a no-op install: `print_install_summary`
    // below will print the "Already up to date" branded line, and we don't
    // want CI users to see both the framed `[ ✓ … resolved N · reused N ]`
    // block and the branded line as redundant twins.
    let install_is_noop = stats.packages_linked == 0 && stats.top_level_linked == 0;
    if let Some(p) = prog_ref {
        p.finish(!install_is_noop);
    }

    if !opts.ignore_scripts && strict_dep_builds_setting && !virtual_store_only {
        let unreviewed = unreviewed_dep_builds(
            &aube_dir,
            &graph_for_link,
            &build_policy,
            virtual_store_dir_max_length,
            placements_ref,
        )?;
        if !unreviewed.is_empty() {
            let review_names = allow_build_review_names(&unreviewed);
            aube_manifest::workspace::add_to_allow_builds(
                &cwd,
                &review_names,
                aube_manifest::workspace::AllowBuildsWriteMode::ReviewPlaceholder,
            )
            .into_diagnostic()
            .wrap_err("failed to update allowBuilds review entries")?;
            return Err(miette!(
                "dependencies with build scripts must be reviewed before install:\n{}\nhelp: set them to true or false in `allowBuilds`, or set `strictDepBuilds=false`",
                unreviewed
                    .into_iter()
                    .map(|pkg| format!("  - {pkg}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    // 7a. Dependency lifecycle scripts (allowBuilds).
    //     Every dep that the `BuildPolicy` explicitly allows runs its
    //     `preinstall` / `install` / `postinstall` scripts from inside
    //     its linked directory under `node_modules/.aube`. Reuses the
    //     already-constructed `build_policy` from above. Skipped
    //     entirely under `--ignore-scripts` (pnpm parity) and when the
    //     policy has no allow rules at all (fast path: no config, no
    //     cost). A failing dep script fails the whole install —
    //     matching pnpm's fail-fast default. No cross-project
    //     collision warning here: step 6a content-addresses the
    //     global store so two projects resolving the same
    //     `(dep-graph, engine)` share a safe directory and divergent
    //     resolutions land at distinct paths.
    if !opts.ignore_scripts && build_policy.has_any_allow_rule() && !virtual_store_only {
        let phase_start = std::time::Instant::now();
        let side_effects_cache_root =
            side_effects_cache_setting.then(|| side_effects_cache_root(store.as_ref()));
        let side_effects_cache = side_effects_cache_root
            .as_deref()
            .map(|root| {
                if side_effects_cache_readonly_setting {
                    SideEffectsCacheConfig::RestoreOnly(root)
                } else {
                    SideEffectsCacheConfig::RestoreAndSave(root)
                }
            })
            .unwrap_or(SideEffectsCacheConfig::Disabled);
        let ran = run_dep_lifecycle_scripts(
            &cwd,
            &modules_dir_name,
            &aube_dir,
            &graph_for_link,
            &build_policy,
            virtual_store_dir_max_length,
            child_concurrency,
            placements_ref,
            side_effects_cache,
            &jail_policy,
            None,
        )
        .await?;
        if ran > 0 {
            tracing::debug!("allowBuilds: ran {ran} dep lifecycle script(s)");
        }
        phase_timings.record("dep_lifecycle", phase_start.elapsed());
    }

    // 7b. Post-link root lifecycle hooks: install → postinstall → prepare.
    //     npm and pnpm run these in this order after deps are linked so the
    //     scripts can use anything they depend on. Skipped with --ignore-scripts
    //     and under `virtualStoreOnly` — scripts typically resolve
    //     binaries via `node_modules/.bin`, which doesn't exist in
    //     that mode.
    //     A hook that's not defined in package.json is a silent no-op.
    //     A hook that exits non-zero fails the install (fail-fast, matching pnpm).
    if !opts.ignore_scripts && !virtual_store_only && !opts.skip_root_lifecycle {
        let phase_start = std::time::Instant::now();
        for (importer_path, importer_manifest) in &lifecycle_manifests {
            let project_dir = importer_project_dir(&cwd, importer_path);
            for hook in [
                aube_scripts::LifecycleHook::Install,
                aube_scripts::LifecycleHook::PostInstall,
                aube_scripts::LifecycleHook::Prepare,
            ] {
                run_root_lifecycle(&project_dir, &modules_dir_name, importer_manifest, hook)
                    .await?;
            }
        }
        phase_timings.record("root_lifecycle", phase_start.elapsed());
    }

    // 8. Write state file for auto-install tracking.
    //    Record whether this was a --prod install so ensure_installed knows
    //    to re-install the full graph before running dev tooling.
    //    Skipped under `virtualStoreOnly` — the state sidecar is
    //    keyed off a materialized node_modules tree that doesn't
    //    exist, and writing it would lie on the next auto-install
    //    freshness check. Same skip when a workspace filter scoped the
    //    run to a subset of importers. State hash is derived from full
    //    manifest + lockfile inputs, so writing it after a partial
    //    materialize would let the next unfiltered `aube install` hit
    //    the warm path while unfiltered importers are still empty.
    //    Observed via `aube add <pkg> --filter <ws>` leaving the new
    //    dep unmaterialized.
    let filtered_install = !opts.workspace_filter.is_empty() || opts.dep_selection.is_filtered();

    // Walk the linked graph once for the unreviewed-builds set; reused
    // by both the state writer (so warm-path repeats keep nudging) and
    // the post-install warning emission below. The walk does a stat per
    // package, so collapsing two callers into one cuts the linker-tail
    // cost on large graphs roughly in half.
    let unreviewed_builds =
        if !opts.ignore_scripts && !strict_dep_builds_setting && !virtual_store_only {
            unreviewed_dep_builds(
                &aube_dir,
                &graph_for_link,
                &build_policy,
                virtual_store_dir_max_length,
                placements_ref,
            )?
        } else {
            Vec::new()
        };

    if !virtual_store_only && !filtered_install {
        let phase_start = std::time::Instant::now();
        // Fingerprint every package in the final graph so the next
        // install can diff and skip unchanged entries. Missing or
        // stale fingerprints fall back to a full install on the
        // read side. Safe for older readers that ignore the field.
        // When the early pass already produced the leaf map for
        // `graph_for_link`, reuse it here as long as it covers every
        // dep_path in the writeback `graph`. Filtered installs short-
        // circuit before this branch so the two graphs are normally
        // identical, but verifying every key keeps the reuse safe
        // against any future code path that diverges them. Any miss
        // falls back to a fresh compute over `graph`.
        let package_content_hashes = current_leaf_hashes
            .filter(|leaf| {
                leaf.len() == graph.packages.len()
                    && graph.packages.keys().all(|k| leaf.contains_key(k))
            })
            .unwrap_or_else(|| delta::compute_package_hashes(&graph, &patch_hashes));
        let package_subtree_hashes = current_subtree_hashes.unwrap_or_else(|| {
            delta::compute_subtree_hashes_from_leaf(&graph, &package_content_hashes)
        });
        let graph_lthash = hex::encode(delta::lthash_of(&package_content_hashes).digest());
        let package_json_hashes =
            state::collect_package_json_hashes_from_manifests(&cwd, &manifests);
        // Diff against the previous install. Logs delta counts at
        // debug so `-v` installs surface what actually moved. A
        // later pass feeds the plan into fetch and link as a
        // pre-filter.
        if let Some(prior) = state::read_state_package_content_hashes(&cwd) {
            let plan = delta::diff(&prior, &package_content_hashes);
            if !plan.is_empty() {
                // Touched set built once. Doubles as a membership
                // probe so future wiring exercises the same shape
                // of predicate shipped in production.
                let touched = plan.touched_set();
                tracing::debug!(
                    "delta: +{} ~{} -{} ({} touched vs {} total, should_touch(first-added)={})",
                    plan.added.len(),
                    plan.changed.len(),
                    plan.removed.len(),
                    touched.len(),
                    package_content_hashes.len(),
                    plan.added.first().is_some_and(|dp| plan.should_touch(dp)),
                );
            }
            // Incremental LtHash self-check. Start from the prior
            // accumulator, apply the observed delta, confirm the
            // result matches a from-scratch hash of the new graph.
            // Cheap sanity on the homomorphic add/remove ops. The
            // future causal scheduler needs these two to stay in
            // lockstep with the full recompute.
            if let Some(prior_lthash_hex) = state::read_state_graph_lthash(&cwd)
                && let Ok(prior_bytes) = hex::decode(&prior_lthash_hex)
                && prior_bytes.len() == 32
            {
                let mut incr = delta::lthash_of(&prior);
                for dp in &plan.removed {
                    if let Some(fp) = prior.get(dp) {
                        incr.remove(fp);
                    }
                }
                for dp in &plan.added {
                    if let Some(fp) = package_content_hashes.get(dp) {
                        incr.add(fp);
                    }
                }
                for dp in &plan.changed {
                    if let Some(old_fp) = prior.get(dp) {
                        incr.remove(old_fp);
                    }
                    if let Some(new_fp) = package_content_hashes.get(dp) {
                        incr.add(new_fp);
                    }
                }
                if hex::encode(incr.digest()) != graph_lthash {
                    // Real bug signal, not routine noise. `debug`
                    // hides it behind `-v` so CI would silently
                    // ship broken homomorphic bookkeeping.
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_LTHASH_MISMATCH,
                        "lthash: incremental/full mismatch, homomorphic invariant broken"
                    );
                }
            }
        }
        // LtHash diagnostic. One 32-byte compare proves graph
        // equivalence with the last install. Beats the map diff
        // when both sides are known good.
        if let Some(prior_lthash) = state::read_state_graph_lthash(&cwd)
            && prior_lthash != graph_lthash
        {
            tracing::debug!(
                "lthash: graph content digest changed ({}..{} -> {}..{})",
                &prior_lthash[..8.min(prior_lthash.len())],
                &prior_lthash[prior_lthash.len().saturating_sub(8)..],
                &graph_lthash[..8],
                &graph_lthash[graph_lthash.len() - 8..],
            );
        }
        // Merkle subtree diagnostic. How many subtree roots moved
        // vs how many leaves moved. Fewer roots means tighter
        // re-link scope once the delta linker lands.
        if let Some(prior_subtrees) = state::read_state_subtree_hashes(&cwd) {
            let changed_subtrees = package_subtree_hashes
                .iter()
                .filter(|(k, v)| prior_subtrees.get(*k).is_none_or(|old| old != *v))
                .count();
            if changed_subtrees > 0 {
                tracing::debug!(
                    "merkle: {} subtree hashes changed of {}",
                    changed_subtrees,
                    package_subtree_hashes.len()
                );
            }
        }
        // Persist the unreviewed-builds set so the warm-path
        // short-circuit can re-emit the warning on repeat installs.
        // Computed once above and reused here.
        let unreviewed_builds_for_state = unreviewed_builds.clone();
        state::write_state(
            &cwd,
            state::WriteStateInput {
                section_filtered: opts.dep_selection.prod_or_dev_axis(),
                package_json_hashes,
                cli_flags: &opts.cli_flags,
                package_content_hashes,
                graph_lthash,
                package_subtree_hashes,
                layout: state::WriteStateLayout {
                    graph: &graph_for_link,
                    node_linker,
                    modules_dir_name: &modules_dir_name,
                    aube_dir: &aube_dir,
                    virtual_store_dir_max_length,
                    placements: placements_ref,
                },
                unreviewed_builds: unreviewed_builds_for_state,
            },
        )
        .into_diagnostic()
        .wrap_err("failed to write install state")?;
        phase_timings.record("state", phase_start.elapsed());
    }

    // 8a. Sweep orphaned `.aube/<dep_path>` entries older than
    //     `modulesCacheMaxAge`. The "in use" set is built from the
    //     **unfiltered** `graph`, not `graph_for_link`, so that a
    //     `--prod` / `--dev` / `--no-optional` / `--filter` install
    //     doesn't treat the deps it skipped this run as orphans —
    //     a subsequent full install would otherwise have to re-fetch
    //     them. Runs best-effort: I/O errors are logged and swallowed
    //     so a partial sweep never fails an install that otherwise
    //     succeeded.
    let modules_cache_max_age_minutes =
        aube_settings::resolved::modules_cache_max_age(&settings_ctx);
    if modules_cache_max_age_minutes > 0 && !virtual_store_only {
        let phase_start = std::time::Instant::now();
        let removed = sweep_orphaned_aube_entries(
            &aube_dir,
            &graph,
            virtual_store_dir_max_length,
            std::time::Duration::from_secs(modules_cache_max_age_minutes.saturating_mul(60)),
        );
        if removed > 0 {
            tracing::debug!("modulesCacheMaxAge: swept {removed} orphaned .aube entry/entries");
        }
        phase_timings.record("sweep", phase_start.elapsed());
    }

    let elapsed = start.elapsed();
    tracing::debug!(
        "Done in {:.0?}: {} packages ({} cached), {} files linked, {} top-level",
        elapsed,
        stats.packages_linked + stats.packages_cached,
        stats.packages_cached,
        stats.files_linked,
        stats.top_level_linked
    );
    phase_timings.write(
        &cwd,
        elapsed,
        graph_for_link.packages.len(),
        cached_count,
        fetch_count,
    );

    if stats.packages_linked == 0
        && stats.packages_cached == 0
        && graph_for_link
            .packages
            .values()
            .any(|p| p.local_source.is_none())
    {
        return Err(miette!("no packages were linked — something went wrong"));
    }

    // Deprecation warnings, gated by the `deprecationWarnings` setting.
    // Prune to packages still in the finalized graph so we don't warn
    // on platform-mismatched optionals that `filter_graph` trimmed,
    // then dedupe across peer-context dep_path variants.
    {
        let mut records = std::mem::take(&mut *deprecations.lock().unwrap());
        crate::deprecations::retain_in_graph(&mut records, &graph_for_link);
        let records = crate::deprecations::dedupe(records);
        if !records.is_empty() {
            let mode = aube_settings::resolved::deprecation_warnings(&settings_ctx);
            crate::deprecations::render_install_warnings(&records, &graph_for_link, mode);
        }
    }

    // Final user-facing output. When linking did real work, print the
    // direct deps that now exist at the top level before the green
    // `✓ installed N packages in Xs` line. Text modes such as `-v` and
    // `--reporter=append-only` skip the progress object but still get the
    // dependency summary; silent and ndjson stay machine-clean.
    if !install_is_noop && should_print_human_install_summary() {
        print_direct_dependency_summary(&graph_for_link, &manifests, &direct_dep_info);
    }
    if let Some(p) = prog_ref {
        p.print_install_summary(
            stats.packages_linked,
            stats.top_level_linked,
            graph_for_link.packages.len(),
            elapsed,
        );
    }

    // Surface packages whose build scripts were skipped because they're
    // not on the `allowBuilds` / `onlyBuiltDependencies` allowlist. Without
    // this, a fresh install of a project that depends on native bindings
    // (`better-sqlite3`, `esbuild`, napi-rs packages, etc.) looks like it
    // succeeded but leaves those packages unbuilt — the failure only
    // surfaces later when something tries to `require` the binding.
    // Skipped under `--ignore-scripts`, `virtualStoreOnly`, and
    // `strictDepBuilds=true` (the strict path already errored above).
    if !unreviewed_builds.is_empty() {
        let review_names = allow_build_review_names(&unreviewed_builds);
        aube_manifest::workspace::add_to_allow_builds(
            &cwd,
            &review_names,
            aube_manifest::workspace::AllowBuildsWriteMode::ReviewPlaceholder,
        )
        .into_diagnostic()
        .wrap_err("failed to update allowBuilds review entries")?;
        emit_unreviewed_builds_warning(&unreviewed_builds);
    }

    Ok(())
}

/// Emit the `ignored build scripts for ...` warning. Same wording
/// fires from the full install path and the warm-path short-circuit
/// so users see the nudge on every repeat install while
/// `allowBuilds` placeholders are still pending review.
fn emit_unreviewed_builds_warning(unreviewed: &[String]) {
    if unreviewed.is_empty() {
        return;
    }
    // Cap the inline list so a napi-rs / prebuilt-variants tree
    // (tens of per-platform binding packages) doesn't splat into
    // one hard-to-scan line. Users who want the full list run
    // `aube ignored-builds`.
    const MAX_INLINE: usize = 5;
    let list = if unreviewed.len() <= MAX_INLINE {
        unreviewed.join(", ")
    } else {
        format!(
            "{}, and {} more",
            unreviewed[..MAX_INLINE].join(", "),
            unreviewed.len() - MAX_INLINE
        )
    };
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_IGNORED_BUILD_SCRIPTS,
        count = unreviewed.len(),
        packages = ?unreviewed,
        "ignored build scripts for {} package(s): {}. Run `aube approve-builds` to review and enable them, or set `strictDepBuilds=true` to fail installs that have unreviewed builds.",
        unreviewed.len(),
        list
    );
}

fn allow_build_review_names(specs: &[String]) -> Vec<String> {
    specs
        .iter()
        .map(|spec| package_name_from_spec_key(spec))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn package_name_from_spec_key(spec: &str) -> String {
    if spec.starts_with('@') {
        if let Some((name, _)) = spec.rsplit_once('@')
            && !name.is_empty()
        {
            return name.to_string();
        }
        return spec.to_string();
    }
    spec.split_once('@')
        .map(|(name, _)| name.to_string())
        .unwrap_or_else(|| spec.to_string())
}

/// Read a lockfile from `lockfile_dir` and remap its importer key
/// for the current project from the project's relative-path key to
/// `"."`, so the rest of the install pipeline can keep treating the
/// project as the root importer. No-op when `importer_key == "."`.
fn parse_lockfile_dir_remapped(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<aube_lockfile::LockfileGraph, aube_lockfile::Error> {
    let mut graph = aube_lockfile::parse_lockfile(lockfile_dir, manifest)?;
    if importer_key != "."
        && let Some(deps) = graph.importers.remove(importer_key)
    {
        graph.importers.insert(".".to_string(), deps);
    }
    Ok(graph)
}

/// Same as [`parse_lockfile_dir_remapped`] but preserves the detected
/// kind for callers that need format-aware behavior.
fn parse_lockfile_dir_remapped_with_kind(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<(aube_lockfile::LockfileGraph, aube_lockfile::LockfileKind), aube_lockfile::Error> {
    let (mut graph, kind) = aube_lockfile::parse_lockfile_with_kind(lockfile_dir, manifest)?;
    if importer_key != "."
        && let Some(deps) = graph.importers.remove(importer_key)
    {
        graph.importers.insert(".".to_string(), deps);
    }
    Ok((graph, kind))
}

/// Refuse to operate on a `--lockfile-dir` lockfile that already
/// records other importers besides the current project. This PR
/// scopes `--lockfile-dir` to single-project relocation; multi-
/// project shared lockfiles need workspace coordination (resolve
/// every importer's deps in one pass, prune packages by union of all
/// importers) which is out of scope. Without this guard, a second
/// project pointed at the same dir would silently orphan-strip the
/// first project's package entries on the next install. Loud-fail
/// here so the user can move to a workspace setup or pick a
/// different `lockfileDir`.
fn guard_against_foreign_importers(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
) -> Result<(), aube_lockfile::Error> {
    // Caller gates on `importer_key != "."`, so any `"."` entry on
    // disk is itself a project that ran `aube install` directly in
    // `lockfile_dir` without `--lockfile-dir`. That entry would be
    // dropped on write, so it counts as foreign.
    let foreign: Vec<&str> = graph
        .importers
        .keys()
        .map(String::as_str)
        .filter(|k| *k != importer_key)
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    Err(aube_lockfile::Error::Parse(
        lockfile_dir.to_path_buf(),
        format!(
            "lockfile already records importers from other projects ({}); \
             aube does not yet support multi-project shared lockfiles outside a workspace. \
             Use a `pnpm-workspace.yaml` workspace, or point each project at its own `--lockfile-dir`.",
            foreign.join(", ")
        ),
    ))
}

/// Write `graph` to `lockfile_dir`, remapping the project's `"."`
/// importer key to its relative-path key from `lockfile_dir`.
/// No-op remap when `importer_key == "."`.
fn write_lockfile_dir_remapped(
    lockfile_dir: &std::path::Path,
    importer_key: &str,
    graph: &aube_lockfile::LockfileGraph,
    manifest: &aube_manifest::PackageJson,
    kind: aube_lockfile::LockfileKind,
) -> Result<std::path::PathBuf, aube_lockfile::Error> {
    if importer_key == "." {
        return aube_lockfile::write_lockfile_as(lockfile_dir, graph, manifest, kind);
    }
    let mut remapped = graph.clone();
    let deps = remapped.importers.remove(".").ok_or_else(|| {
        aube_lockfile::Error::Parse(
            lockfile_dir.to_path_buf(),
            format!(
                "in-memory lockfile graph missing `.` importer; cannot write under key `{importer_key}`"
            ),
        )
    })?;
    remapped.importers.insert(importer_key.to_string(), deps);
    aube_lockfile::write_lockfile_as(lockfile_dir, &remapped, manifest, kind)
}

fn print_already_up_to_date() {
    if clx::progress::output() == clx::progress::ProgressOutput::Text {
        return;
    }
    use clx::style;
    use std::io::Write;
    // Routed through the shared `aube_prefix_line` helper so this
    // site and `print_install_summary`'s no-op branch can't drift —
    // both produce `aube VERSION by en.dev · ✓ Already up to date`.
    let msg = format!(
        "{} {}",
        style::egreen("✓").bold(),
        style::ebold("Already up to date"),
    );
    let line = crate::progress::aube_prefix_line(&msg);
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn print_direct_dependency_summary(
    graph: &aube_lockfile::LockfileGraph,
    manifests: &[(String, aube_manifest::PackageJson)],
    direct_dep_info: &std::collections::HashMap<String, aube_resolver::DirectDepInfo>,
) {
    use clx::style;
    let importers: Vec<(&String, &Vec<aube_lockfile::DirectDep>)> = graph
        .importers
        .iter()
        .filter(|(_, deps)| !deps.is_empty())
        .collect();
    if importers.is_empty() {
        return;
    }
    let show_importer_headers = importers.len() > 1;
    for (idx, (importer, deps)) in importers.iter().enumerate() {
        if idx > 0 {
            eprintln!();
        }
        if show_importer_headers {
            let label = direct_dependency_importer_label(importer, manifests);
            eprintln!("{}{}", style::ebold(&label), style::edim(":"));
        }
        print_direct_dependency_section(
            graph,
            deps,
            aube_lockfile::DepType::Production,
            direct_dep_info,
        );
        print_direct_dependency_section(
            graph,
            deps,
            aube_lockfile::DepType::Optional,
            direct_dep_info,
        );
        print_direct_dependency_section(graph, deps, aube_lockfile::DepType::Dev, direct_dep_info);
    }
    eprintln!();
}

fn direct_dependency_importer_label(
    importer: &str,
    manifests: &[(String, aube_manifest::PackageJson)],
) -> String {
    manifests
        .iter()
        .find(|(path, _)| path == importer)
        .and_then(|(_, manifest)| manifest.name.clone())
        .unwrap_or_else(|| importer.to_string())
}

fn should_print_human_install_summary() -> bool {
    let flags = super::global_output_flags();
    !flags.silent && !flags.ndjson
}

fn print_direct_dependency_section(
    graph: &aube_lockfile::LockfileGraph,
    deps: &[aube_lockfile::DirectDep],
    dep_type: aube_lockfile::DepType,
    direct_dep_info: &std::collections::HashMap<String, aube_resolver::DirectDepInfo>,
) {
    use clx::style;
    let mut deps: Vec<&aube_lockfile::DirectDep> =
        deps.iter().filter(|dep| dep.dep_type == dep_type).collect();
    if deps.is_empty() {
        return;
    }
    deps.sort_by(|a, b| a.name.cmp(&b.name));
    let label = aube_lockfile::dep_type_label(dep_type);
    eprintln!("{}{}", style::ebold(label), style::edim(":"));
    for dep in deps {
        let version = graph
            .get_package(&dep.dep_path)
            .map(|pkg| pkg.version.as_str())
            .unwrap_or("?");
        let badges = render_direct_dep_badges(direct_dep_info.get(&dep.dep_path));
        eprintln!(
            "{} {}{}{}",
            style::egreen("+").bold(),
            dep.name,
            style::edim(format!("@{version}")),
            badges,
        );
    }
}

/// Render the trailing badge column for a direct-dep line. Empty string
/// when there's nothing to flag, otherwise a leading two-space gap and
/// one or more dim-separated tags (`deprecated`, `latest 2.0.0`). The
/// caller passes `direct_dep_info.get(dep_path)`, and `direct_dep_info`
/// only carries entries with at least one signal set — so when `info`
/// is `Some(...)`, `parts` is guaranteed non-empty.
fn render_direct_dep_badges(info: Option<&aube_resolver::DirectDepInfo>) -> String {
    use clx::style;
    let Some(info) = info else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    if info.deprecated {
        parts.push(style::eyellow("deprecated").to_string());
    }
    if let Some(latest) = &info.latest {
        parts.push(style::eyellow(format!("latest {latest}")).to_string());
    }
    let sep = style::edim(" · ").to_string();
    format!("  {}", parts.join(&sep))
}

fn invalidate_changed_aube_entries(
    aube_dir: &std::path::Path,
    dep_paths: &[String],
    virtual_store_dir_max_length: usize,
) -> usize {
    let mut removed = 0usize;
    for dep_path in dep_paths {
        let path = aube_dir.join(dep_path_to_filename(dep_path, virtual_store_dir_max_length));
        let result = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        match result {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(
                code = aube_codes::warnings::WARN_AUBE_DELTA_INVALIDATE_FAILED,
                "delta: failed to invalidate {}: {e}",
                path.display()
            ),
        }
    }
    removed
}

/// Remove `node_modules/.aube/<encoded_dep_path>` entries that aren't
/// referenced by the current lockfile graph AND whose last-modified
/// time is older than `max_age`. The `.aube/` directory accumulates
/// orphaned entries as dependencies are upgraded or removed; this
/// pass enforces `modulesCacheMaxAge` (default 7 days) so stale
/// packages don't live forever.
///
/// Runs best-effort: I/O errors are logged and swallowed so a partial
/// sweep never fails an install that otherwise succeeded. Returns the
/// number of entries successfully removed so the caller can decide
/// whether to emit a tracing line.
///
/// The sweep identifies orphans by **name**: it builds `in_use` from
/// `dep_path_to_filename` over the unfiltered lockfile graph, then
/// removes any entry whose encoded name is absent from that set AND
/// whose mtime is older than `max_age`. The linker does not refresh
/// mtimes on cache hits — the `in_use` name check is what guarantees
/// graph-referenced entries are never removed, regardless of how
/// stale their on-disk mtime is. Mtime is read via `symlink_metadata`
/// so that, in global-virtual-store mode where `.aube/<dep_path>` is
/// a symlink into the shared `~/.cache/aube/virtual-store/`, the
/// orphan age reflects "when did *this project* last write the
/// link" rather than the global target's last-materialized time
/// (which any other project's install can refresh, indefinitely
/// preserving an otherwise-orphaned entry). Entries we don't
/// recognize are always preserved: dotfiles (`.patches`, future
/// sidecars) and the `.aube/node_modules/` hidden hoist tree
/// populated by `link_hidden_hoist` — that one isn't a
/// `dep_path_to_filename` output, so it never appears in `in_use`,
/// and the linker manages its lifecycle on every run independent
/// of this sweep.
fn sweep_orphaned_aube_entries(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    max_age: std::time::Duration,
) -> usize {
    use aube_lockfile::dep_path_filename::dep_path_to_filename;

    let entries = match std::fs::read_dir(aube_dir) {
        Ok(e) => e,
        // No `.aube` directory = nothing to sweep (e.g. fresh CI
        // install). Not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => {
            tracing::debug!(
                "modulesCacheMaxAge: cannot read {}: {e}; skipping sweep",
                aube_dir.display()
            );
            return 0;
        }
    };

    let in_use: std::collections::HashSet<String> = graph
        .packages
        .keys()
        .map(|dep_path| dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .collect();

    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Dotfiles (`.patches`, future sidecars) are always preserved.
        if name_str.starts_with('.') {
            continue;
        }
        // `.aube/node_modules/` is the hidden hoist tree populated
        // by `link_hidden_hoist`, not a `dep_path_to_filename`
        // output, so it never appears in `in_use`. Removing it
        // would break Node's parent-walk resolution for packages
        // inside the virtual store. The hoist is fully managed by
        // the linker (it sweeps stale entries on every run when
        // `hoist=false`), so the modulesCacheMaxAge sweep has no
        // business touching it.
        if name_str == "node_modules" {
            continue;
        }
        if in_use.contains(name_str.as_ref()) {
            continue;
        }
        // `symlink_metadata` so the mtime reflects the local
        // `.aube/<dep>` symlink (or directory in CI mode) and not
        // the shared virtual-store target — see the function-level
        // docs for why following the symlink is wrong here.
        let metadata = match entry.path().symlink_metadata() {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(
                    "modulesCacheMaxAge: cannot stat {}: {e}",
                    entry.path().display()
                );
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue, // platform doesn't expose mtime; keep.
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age < max_age {
            continue;
        }
        let path = entry.path();
        // `.aube/<dep>` is typically a symlink into the shared
        // virtual store (global-store mode) or a real directory
        // containing a materialized copy (CI mode). On older Linux
        // kernels (pre-5.6, before `openat2`), `remove_dir_all`
        // can follow a symlink and recursively delete the link's
        // *target* — which here would be the shared
        // `~/.cache/aube/virtual-store/<entry>` that other projects
        // depend on. Route symlinks straight to `remove_file` so
        // only the local link is unlinked; only call
        // `remove_dir_all` for real directories, with `remove_file`
        // as a safety net for Windows junctions / platforms where
        // either call may decline the other's file type.
        let file_type = metadata.file_type();
        let result = if file_type.is_symlink() {
            std::fs::remove_file(&path)
        } else {
            std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path))
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => tracing::debug!(
                "modulesCacheMaxAge: failed to remove {}: {e}",
                path.display()
            ),
        }
    }
    removed
}

fn filter_graph_to_workspace_selection(
    workspace_root: &std::path::Path,
    workspace_packages: &[std::path::PathBuf],
    graph: &aube_lockfile::LockfileGraph,
    filters: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<aube_lockfile::LockfileGraph> {
    let selected = aube_workspace::selector::select_workspace_packages(
        workspace_root,
        workspace_packages,
        filters,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "aube install: filter {filters:?} did not match any workspace package"
        ));
    }
    let mut keep_importers = std::collections::BTreeSet::new();
    if graph.importers.contains_key(".") {
        keep_importers.insert(".".to_string());
    }
    for pkg in selected {
        keep_importers.insert(super::workspace_importer_path(workspace_root, &pkg.dir)?);
    }
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(*importer))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    Ok(filtered.filter_deps(|_| true))
}

fn importer_project_dir(
    workspace_root: &std::path::Path,
    importer_path: &str,
) -> std::path::PathBuf {
    if importer_path == "." {
        workspace_root.to_path_buf()
    } else {
        // Lexically collapse `..` from the join so a parent-relative
        // importer key (`../sibling`, written by `find_workspace_packages`
        // when `pnpm-workspace.yaml#packages` uses `../**`) lands at
        // the actual sibling directory rather than `<root>/../sibling`.
        // Downstream consumers — `pathdiff` for symlink targets and
        // `strip_prefix` for ancestor checks — give wrong results
        // against an unnormalized path with embedded `..` segments.
        aube_util::path::normalize_lexical(&workspace_root.join(importer_path))
    }
}

fn order_lifecycle_manifests(
    manifests: Vec<(String, aube_manifest::PackageJson)>,
) -> Vec<(String, aube_manifest::PackageJson)> {
    if manifests.len() < 2 {
        return manifests;
    }

    let importer_index: std::collections::HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(idx, (importer, _))| (importer.as_str(), idx))
        .collect();
    let workspace_name_to_importer: std::collections::HashMap<&str, &str> = manifests
        .iter()
        .filter_map(|(importer, manifest)| {
            manifest
                .name
                .as_deref()
                .map(|name| (name, importer.as_str()))
        })
        .collect();

    let mut edges = vec![Vec::<usize>::new(); manifests.len()];
    let mut indegree = vec![0usize; manifests.len()];
    for (dependent_idx, (dependent_importer, manifest)) in manifests.iter().enumerate() {
        for dep_name in manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .chain(manifest.optional_dependencies.keys())
        {
            let Some(dependency_importer) = workspace_name_to_importer.get(dep_name.as_str())
            else {
                continue;
            };
            if *dependency_importer == dependent_importer {
                continue;
            }
            let Some(&dependency_idx) = importer_index.get(dependency_importer) else {
                continue;
            };
            if !edges[dependency_idx].contains(&dependent_idx) {
                edges[dependency_idx].push(dependent_idx);
                indegree[dependent_idx] += 1;
            }
        }
    }

    let mut ready: std::collections::VecDeque<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, degree)| (*degree == 0).then_some(idx))
        .collect();
    let mut ordered = Vec::with_capacity(manifests.len());
    let mut emitted = vec![false; manifests.len()];
    while let Some(idx) = ready.pop_front() {
        if emitted[idx] {
            continue;
        }
        emitted[idx] = true;
        ordered.push(idx);
        for &dependent_idx in &edges[idx] {
            indegree[dependent_idx] -= 1;
            if indegree[dependent_idx] == 0 {
                ready.push_back(dependent_idx);
            }
        }
    }
    for (idx, is_emitted) in emitted.iter().enumerate() {
        if !is_emitted {
            ordered.push(idx);
        }
    }

    let mut manifests = manifests
        .into_iter()
        .map(Some)
        .collect::<Vec<Option<(String, aube_manifest::PackageJson)>>>();
    ordered
        .into_iter()
        .filter_map(|idx| manifests[idx].take())
        .collect()
}

/// Write one lockfile per non-root workspace importer when
/// `sharedWorkspaceLockfile=false` is set. Each lockfile contains
/// only the importer's own deps (remapped to `.`) plus the transitive
/// closure reachable from them. The workspace-root lockfile is not
/// written under this layout.
///
/// Importers without a corresponding manifest entry are skipped — the
/// resolver should never produce one, but defensive skipping keeps a
/// stale graph entry from triggering a write into a directory that
/// doesn't exist on disk.
fn write_per_project_lockfiles(
    workspace_root: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    workspace_manifests: &[(String, aube_manifest::PackageJson)],
    write_kind: aube_lockfile::LockfileKind,
) -> miette::Result<()> {
    use miette::IntoDiagnostic;
    for (importer_path, pkg_manifest) in workspace_manifests {
        if importer_path == "." {
            // The root manifest gets no per-project lockfile under
            // sharedWorkspaceLockfile=false; it's the workspace anchor,
            // not an installable importer.
            continue;
        }
        let Some(subset) = graph.subset_to_importer(importer_path, |_| true) else {
            tracing::debug!(
                "sharedWorkspaceLockfile=false: skipping {importer_path} (no graph importer entry)"
            );
            continue;
        };
        let pkg_dir = workspace_root.join(importer_path);
        let written = aube_lockfile::write_lockfile_as(&pkg_dir, &subset, pkg_manifest, write_kind)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write per-project lockfile at {importer_path}"))?;
        tracing::debug!(
            "sharedWorkspaceLockfile=false: wrote {} for importer {importer_path}",
            written
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| written.display().to_string())
        );
    }
    Ok(())
}

fn filter_graph_to_importers<const N: usize>(
    graph: &aube_lockfile::LockfileGraph,
    keep_importers: [&str; N],
) -> aube_lockfile::LockfileGraph {
    let keep_importers: std::collections::BTreeSet<&str> = keep_importers.into_iter().collect();
    let importers: std::collections::BTreeMap<String, Vec<aube_lockfile::DirectDep>> = graph
        .importers
        .iter()
        .filter(|(importer, _)| keep_importers.contains(importer.as_str()))
        .map(|(importer, deps)| (importer.clone(), deps.clone()))
        .collect();
    let filtered = aube_lockfile::LockfileGraph {
        importers,
        ..graph.clone()
    };
    filtered.filter_deps(|_| true)
}

#[cfg(test)]
mod allow_build_review_tests {
    use super::{order_lifecycle_manifests, package_name_from_spec_key};

    #[test]
    fn package_name_from_spec_key_handles_scoped_names() {
        assert_eq!(package_name_from_spec_key("@scope/pkg@1.2.3"), "@scope/pkg");
        assert_eq!(package_name_from_spec_key("@scope/pkg"), "@scope/pkg");
    }

    #[test]
    fn package_name_from_spec_key_handles_unscoped_names() {
        assert_eq!(package_name_from_spec_key("esbuild@1.2.3"), "esbuild");
        assert_eq!(package_name_from_spec_key("esbuild"), "esbuild");
    }

    #[test]
    fn lifecycle_manifests_follow_workspace_dependency_order() {
        let ordered = order_lifecycle_manifests(vec![
            (".".to_string(), named_manifest("root")),
            (
                "packages/app".to_string(),
                manifest_with_dep("app", "@scope/lib"),
            ),
            ("packages/lib".to_string(), named_manifest("@scope/lib")),
        ]);
        let importers = ordered
            .iter()
            .map(|(importer, _)| importer.as_str())
            .collect::<Vec<_>>();

        assert_eq!(importers, [".", "packages/lib", "packages/app"]);
    }

    fn named_manifest(name: &str) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn manifest_with_dep(name: &str, dep: &str) -> aube_manifest::PackageJson {
        let mut manifest = named_manifest(name);
        manifest
            .dependencies
            .insert(dep.to_string(), "workspace:*".to_string());
        manifest
    }
}

#[cfg(test)]
mod critical_path_tests {
    use super::is_likely_native_build;

    #[test]
    fn flags_exact_native_packages() {
        for name in [
            "sharp",
            "esbuild",
            "fsevents",
            "node-gyp",
            "better-sqlite3",
            "sodium-native",
        ] {
            assert!(is_likely_native_build(name), "{name} should match");
        }
    }

    #[test]
    fn flags_scoped_native_prefixes() {
        for name in [
            "@swc/core",
            "@swc/cli",
            "@parcel/source-map",
            "@napi-rs/cli",
            "@next/swc-linux-x64-gnu",
            "@rollup/rollup-linux-x64-gnu",
            "@esbuild/linux-x64",
            "@playwright/test",
            "@biomejs/biome",
        ] {
            assert!(is_likely_native_build(name), "{name} should match");
        }
    }

    #[test]
    fn does_not_flag_pure_js_packages() {
        for name in [
            "react",
            "lodash",
            "@types/node",
            "swc",  // unscoped, not native
            "ws",   // pure JS
            "sass", // dart-sass, pure JS
            "@scope/random",
        ] {
            assert!(!is_likely_native_build(name), "{name} should NOT match");
        }
    }

    #[test]
    fn sort_is_stable_within_groups() {
        // Mirror the sort applied at install/mod.rs. Stable sort
        // must keep relative order within natives and non-natives.
        let mut items = [
            ("react", 1),
            ("sharp", 2),
            ("lodash", 3),
            ("esbuild", 4),
            ("@types/node", 5),
            ("@swc/core", 6),
        ];
        items.sort_by_key(|(name, _)| !is_likely_native_build(name));
        let order: Vec<&str> = items.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            order,
            [
                "sharp",
                "esbuild",
                "@swc/core",
                "react",
                "lodash",
                "@types/node"
            ]
        );
    }
}

#[cfg(test)]
mod combine_pipeline_errors_tests {
    use super::combine_install_pipeline_errors;
    use miette::miette;

    fn fmt_chain(report: &miette::Report) -> String {
        let mut out = report.to_string();
        let mut src = report.source();
        while let Some(e) = src {
            out.push_str(" :: ");
            out.push_str(&e.to_string());
            src = e.source();
        }
        out
    }

    #[tokio::test]
    async fn returns_fetch_err_when_materializer_succeeded() {
        let handle = tokio::spawn(async {
            Ok((
                aube_linker::LinkStats::default(),
                None::<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
            ))
        });
        let fetch_err = miette!("network down: timed out fetching foo@1.0");
        let combined = combine_install_pipeline_errors(handle, fetch_err).await;
        assert!(
            combined.to_string().contains("network down"),
            "got: {}",
            combined
        );
    }

    #[tokio::test]
    async fn nests_both_errors_when_materializer_failed() {
        let handle = tokio::spawn(async {
            Err::<
                (
                    aube_linker::LinkStats,
                    Option<std::sync::Arc<aube_lockfile::graph_hash::GraphHashes>>,
                ),
                _,
            >(miette!("materialize foo@1.0: permission denied"))
        });
        // The fetch task surfaces the channel-closed symptom.
        let fetch_err = miette!("materializer task exited before fetch finished");
        let combined = combine_install_pipeline_errors(handle, fetch_err).await;
        let chain = fmt_chain(&combined);
        // Both errors visible: fetch's symptom on top, materializer's
        // root cause in the chain below.
        assert!(
            chain.contains("materializer task exited before fetch finished"),
            "fetch err missing from chain: {chain}"
        );
        assert!(
            chain.contains("permission denied"),
            "materializer err missing from chain: {chain}"
        );
    }
}
