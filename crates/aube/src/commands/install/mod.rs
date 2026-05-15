use super::make_client;
use crate::progress::InstallProgress;
use crate::state;
use aube_lockfile::DriftStatus;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::io::Write;

mod advisory;
mod args;
mod bin_linking;
mod critical_path;
mod delta;
mod dep_selection;
mod fetch;
mod frozen;
mod git_prepare;
mod lifecycle;
mod lockfile_dir;
mod materialize;
pub(crate) mod node_gyp_bootstrap;
mod settings;
mod side_effects_cache;
mod summary;
mod sweep;
mod unreviewed_builds;
mod workspace;

use advisory::resolve_osv_routing_settings;
pub use args::{InstallArgs, InstallOptions};
pub(crate) use bin_linking::{PkgJsonCache, link_dep_bins, materialized_pkg_dir};
use bin_linking::{link_bin_entries, link_bins, link_bins_for_dep};
pub use dep_selection::DepSelection;
pub(super) use fetch::fetch_packages;
use fetch::{
    fetch_packages_with_root, import_local_source, remap_indices_to_contextualized,
    version_from_dep_path,
};
pub use frozen::{FrozenMode, FrozenOverride, GlobalVirtualStoreFlags};
pub(crate) use lifecycle::{
    JailBuildPolicy, build_policy_from_manifest_sources, build_policy_from_sources,
    run_dep_lifecycle_scripts,
};
use lifecycle::{
    resolve_link_strategy, run_import_on_blocking, run_root_lifecycle, unreviewed_dep_builds,
    validate_required_scripts,
};
use lockfile_dir::{
    guard_against_foreign_importers, parse_lockfile_dir_remapped,
    parse_lockfile_dir_remapped_with_kind, write_lockfile_dir_remapped,
};
use materialize::{
    GvsPrewarmInputs, combine_install_pipeline_errors, materialize_channel, spawn_gvs_prewarm,
};
pub(crate) use settings::PeerDependencyRules;
pub(crate) use settings::{ResolverConfigInputs, configure_resolver};
pub(crate) use side_effects_cache::{SideEffectsCacheConfig, side_effects_cache_root};

use settings::{
    check_unmet_peers, default_streaming_network_concurrency, detect_aube_dir_gvs_mode,
    find_gvs_incompatible_trigger, maybe_cleanup_unused_catalogs, resolve_dedupe_peer_dependents,
    resolve_dedupe_peers, resolve_git_shallow_hosts, resolve_link_concurrency,
    resolve_network_concurrency, resolve_peers_from_workspace_root,
    resolve_peers_suffix_max_length, resolve_side_effects_cache,
    resolve_side_effects_cache_readonly, resolve_strict_peer_dependencies,
    resolve_strict_store_pkg_content_check, resolve_symlink, resolve_use_running_store_server,
    resolve_verify_store_integrity,
};
use summary::{
    print_already_up_to_date, print_direct_dependency_summary, should_print_human_install_summary,
};
use sweep::{invalidate_changed_aube_entries, sweep_orphaned_aube_entries};
use workspace::{
    filter_graph_to_importers, filter_graph_to_workspace_selection, importer_project_dir,
    order_lifecycle_manifests, write_per_project_lockfiles,
};

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
        unreviewed_builds::emit_warning(&unreviewed_builds::from_state(&cwd));
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
        unreviewed_builds::emit_warning(&unreviewed_builds::from_state(&cwd));
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
                minimum_release_age_override: opts.minimum_release_age_override,
                // `lockfile=false` collapses to `None` so the resolver
                // doesn't waste a fetch widening a lockfile that will
                // never be written. With lockfiles enabled, a missing
                // `source_kind_before` means "we'll create the default
                // aube-lock.yaml", so the aube-native wide default
                // applies.
                target_lockfile_kind: lockfile_enabled
                    .then(|| source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube)),
                cache_full_packuments: true,
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
            // Time the hoist on its own, then `filter_graph` runs untimed
            // (it's not part of the peer pass), then apply is timed below.
            // Snapshotting `pkgs_before` after `filter_graph` keeps the
            // logged delta a pure measure of `apply_peer_contexts`'s
            // additions, not filter_graph's prunes.
            let mut hoist_elapsed: Option<std::time::Duration> = None;
            if needs_peer_pass {
                let hoist_start = std::time::Instant::now();
                graph = aube_resolver::hoist_auto_installed_peers(graph);
                hoist_elapsed = Some(hoist_start.elapsed());
            }
            aube_resolver::platform::filter_graph(
                &mut graph,
                &supported_architectures,
                &ignored_optional_deps,
            );
            if let Some(hoist_elapsed) = hoist_elapsed {
                let peer_options = aube_resolver::PeerContextOptions {
                    dedupe_peer_dependents: resolve_dedupe_peer_dependents(&settings_ctx),
                    dedupe_peers: resolve_dedupe_peers(&settings_ctx),
                    resolve_from_workspace_root: resolve_peers_from_workspace_root(&settings_ctx),
                    peers_suffix_max_length: resolve_peers_suffix_max_length(&settings_ctx),
                };
                let pkgs_before = graph.packages.len();
                let apply_start = std::time::Instant::now();
                graph = aube_resolver::apply_peer_contexts(graph, &peer_options)
                    .map_err(|e| miette!("peer-context pass failed: {e}"))?;
                tracing::debug!(
                    "peer-context pass (lockfile={:?}) {} → {} packages in {:.1?}",
                    kind,
                    pkgs_before,
                    graph.packages.len(),
                    hoist_elapsed + apply_start.elapsed()
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

            // Post-resolve OSV `MAL-*` routing — lockfile-found
            // branch. `fresh_resolution = false` here because the
            // graph came from the lockfile and we never ran the
            // resolver, so the router falls through to the mirror
            // backend unless `osv_transitive_check` or
            // `advisoryCheckEveryInstall` forces the live API.
            // Same helper as the no-lockfile branch — kept here so
            // `aube ci`, `aube install --frozen-lockfile`, and
            // every frozen reinstall actually run the routing
            // (previously skipped, surfaced by review).
            let osv_settings = resolve_osv_routing_settings(&cwd);
            super::add_supply_chain::run_post_resolve_osv_routing(
                &cwd,
                &graph,
                /*fresh_resolution=*/ false,
                opts.osv_transitive_check,
                osv_settings.advisory_check,
                osv_settings.advisory_check_on_install,
                osv_settings.advisory_bloom_check,
                osv_settings.advisory_check_every_install,
            )
            .await?;

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
                    minimum_release_age_override: opts.minimum_release_age_override,
                    // Same disambiguation as the `--lockfile-only` path:
                    // `None` only when no lockfile will be written, so
                    // widening to every common platform doesn't happen
                    // just to be discarded.
                    target_lockfile_kind: lockfile_enabled
                        .then(|| source_kind_before.unwrap_or(aube_lockfile::LockfileKind::Aube)),
                    cache_full_packuments: true,
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
                    //
                    // `_verified`: see the matching call in
                    // `fetch_packages_with_root` for the full
                    // rationale — short version, a stat-per-file cache
                    // check is cheap, and dropping a stale index
                    // here re-fetches the tarball cleanly instead of
                    // letting the materializer die later with
                    // `ERR_AUBE_MISSING_STORE_FILE`.
                    let pkg_registry_name = pkg.registry_name().to_string();
                    if let Some(index) = fetch_store.load_index_verified(
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

            // Post-resolve OSV `MAL-*` routing — no-lockfile /
            // re-resolve branch. The lockfile-found branch has the
            // parallel call before its own fetch so both paths
            // run through the same router. See
            // `add_supply_chain::run_post_resolve_osv_routing` for
            // the decision table. Fires before the pluggable
            // scanner so a confirmed-malicious advisory aborts
            // without spawning the scanner.
            let prior_lockfile = lockfile_pre_parse.as_ref().map(|(g, _)| g);
            let fresh_resolution =
                super::add_supply_chain::lockfile_has_new_picks(&cwd, prior_lockfile, &graph);
            let osv_settings = resolve_osv_routing_settings(&cwd);
            super::add_supply_chain::run_post_resolve_osv_routing(
                &cwd,
                &graph,
                fresh_resolution,
                opts.osv_transitive_check,
                osv_settings.advisory_check,
                osv_settings.advisory_check_on_install,
                osv_settings.advisory_bloom_check,
                osv_settings.advisory_check_every_install,
            )
            .await?;

            // Bun-compatible security scanner runs against the
            // *resolved* graph — full transitive set with concrete
            // versions, matching Bun's contract. Fires before fetch
            // so a `fatal` advisory aborts without wasting bandwidth
            // on tarball downloads. Fail-closed on any subprocess
            // failure (see `commands::security_scanner`); empty
            // `securityScanner` (the default) short-circuits to a
            // no-op without spawning `node`.
            let scanner = super::with_settings_ctx(&cwd, aube_settings::resolved::security_scanner);
            if !scanner.is_empty() {
                let scanner_packages =
                    super::security_scanner::resolved_packages_for_scanner(&graph);
                super::security_scanner::run_scanner(&scanner, &cwd, &scanner_packages).await?;
            }

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
            return Err(miette!(
                "dependencies with build scripts must be reviewed before install:\n{}\nhelp: add the package(s) to `allowBuilds` with `true`/`false`, or set `strictDepBuilds=false`",
                unreviewed
                    .into_iter()
                    .map(|b| format!("  - {}", b.spec_key))
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
        // Computed once above and reused here. The state file
        // carries only spec_keys; per-package suspicion data is
        // re-derived from the live tree on each install rather
        // than persisted, since the regex set evolves with aube.
        let unreviewed_builds_for_state: Vec<String> = unreviewed_builds
            .iter()
            .map(|b| b.spec_key.clone())
            .collect();
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
        unreviewed_builds::emit_warning(&unreviewed_builds);
    }

    Ok(())
}
