use miette::{Context, IntoDiagnostic, miette};

use super::bin_linking::{dep_modules_dir_for, materialized_pkg_dir};
use super::node_gyp_bootstrap;
use super::side_effects_cache::{
    SideEffectsCacheConfig, SideEffectsCacheEntry, SideEffectsCacheRestore,
};

/// Run a root-package lifecycle hook, announcing it to the user if defined
/// and turning aube_scripts::Error into a miette::Report with context.
/// Silent when the hook isn't defined in package.json.
pub(super) async fn run_root_lifecycle(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    manifest: &aube_manifest::PackageJson,
    hook: aube_scripts::LifecycleHook,
) -> miette::Result<()> {
    // Only announce when the hook is actually defined, so projects without
    // lifecycle scripts don't get noise in their install output.
    if !manifest.scripts.contains_key(hook.script_name()) {
        return Ok(());
    }
    tracing::debug!("Running {} script...", hook.script_name());
    aube_scripts::run_root_hook(project_dir, modules_dir_name, manifest, hook)
        .await
        .map_err(|e| {
            // Old message was just the bare error string. User got
            // a cryptic "exit status 1" with no hook name, no script
            // path, nothing. Tag with which hook fired so the log
            // line is self-documenting. This is the common case
            // (failed preinstall on `aube install`) so the regression
            // really hurt triage.
            miette!("root {} script failed: {e}", hook.script_name())
        })?;
    Ok(())
}

/// Build the dependency lifecycle-script `BuildPolicy` by merging
/// every supported source on the root manifest + workspace file:
///
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.allowBuilds` map
///   (aube's superset format — patterns with bool values)
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.onlyBuiltDependencies`
///   flat list (pnpm's canonical allowlist, used by nearly every
///   real-world pnpm project)
/// - `package.json` / `pnpm-workspace.yaml` `pnpm.neverBuiltDependencies`
///   flat list (pnpm's canonical denylist)
/// - the `--dangerously-allow-all-builds` escape hatch
///
/// Workspace-level entries in the `allowBuilds` map take precedence
/// over the manifest map for the same pattern, matching pnpm. The
/// flat lists are pure append — deny always wins at `decide()` time.
pub(crate) fn build_policy_from_sources(
    manifest: &aube_manifest::PackageJson,
    workspace: &aube_manifest::WorkspaceConfig,
    dangerously_allow_all_builds: bool,
) -> (
    aube_scripts::BuildPolicy,
    Vec<aube_scripts::BuildPolicyError>,
) {
    let mut merged = manifest.pnpm_allow_builds();
    for (k, v) in workspace.allow_builds_raw() {
        merged.insert(k, v);
    }
    let mut only_built = manifest.pnpm_only_built_dependencies();
    only_built.extend(workspace.only_built_dependencies.iter().cloned());
    // Bun's top-level `trustedDependencies` feeds the same allowlist so
    // bun projects migrating to aube keep running their install scripts
    // without moving the list under `pnpm.onlyBuiltDependencies` first.
    only_built.extend(manifest.trusted_dependencies());
    let mut never_built = manifest.pnpm_never_built_dependencies();
    never_built.extend(workspace.never_built_dependencies.iter().cloned());
    aube_scripts::BuildPolicy::from_config(
        &merged,
        &only_built,
        &never_built,
        dangerously_allow_all_builds,
    )
}

/// Resolve the link strategy (reflink / hardlink / copy) from CLI
/// override, `.npmrc` / `pnpm-workspace.yaml`, or filesystem detection.
/// Shared by the prewarm-GVS materializer (which needs the strategy
/// before the full linker is built) and the link phase proper.
pub(super) fn resolve_link_strategy(
    cwd: &std::path::Path,
    ctx: &aube_settings::ResolveCtx<'_>,
) -> miette::Result<aube_linker::LinkStrategy> {
    let package_import_method_cli =
        aube_settings::values::string_from_cli("packageImportMethod", ctx.cli);
    // Shared probe used by both the CLI and resolved-setting paths
    // below. Probe across store dir and project dir. Single-dir probe
    // reports reflink but real link ops across a mount boundary hit
    // EXDEV and silently fall back to per-file copy. Catches the
    // cross-FS case at probe time without requiring node_modules to
    // exist yet.
    let auto_probe = || {
        let store_dir = super::super::open_store(cwd)
            .map(|s| s.root().to_path_buf())
            .ok();
        match store_dir.as_deref() {
            Some(sd) => aube_linker::Linker::detect_strategy_cross(sd, cwd),
            None => aube_linker::Linker::detect_strategy(cwd),
        }
    };
    let strategy = if let Some(cli) = package_import_method_cli.as_deref() {
        match cli.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => auto_probe(),
            "hardlink" => aube_linker::LinkStrategy::Hardlink,
            "copy" => aube_linker::LinkStrategy::Copy,
            "clone-or-copy" => aube_linker::LinkStrategy::Reflink,
            "clone" => {
                tracing::warn!(
                    "package-import-method=clone: reflink will silently fall back to copy \
                     if the filesystem does not support it (strict enforcement is a known TODO)"
                );
                aube_linker::LinkStrategy::Reflink
            }
            other => {
                return Err(miette!(
                    "unknown --package-import-method value `{other}`; expected `auto`, `hardlink`, `copy`, `clone`, or `clone-or-copy`"
                ));
            }
        }
    } else {
        match aube_settings::resolved::package_import_method(ctx) {
            aube_settings::resolved::PackageImportMethod::Auto => auto_probe(),
            aube_settings::resolved::PackageImportMethod::Hardlink => {
                aube_linker::LinkStrategy::Hardlink
            }
            aube_settings::resolved::PackageImportMethod::Copy => aube_linker::LinkStrategy::Copy,
            aube_settings::resolved::PackageImportMethod::CloneOrCopy => {
                aube_linker::LinkStrategy::Reflink
            }
            aube_settings::resolved::PackageImportMethod::Clone => {
                tracing::warn!(
                    "package-import-method=clone: reflink will silently fall back to copy \
                     if the filesystem does not support it (strict enforcement is a known TODO)"
                );
                aube_linker::LinkStrategy::Reflink
            }
        }
    };
    Ok(strategy)
}

/// Walk every linked dependency, check its `package.json` for
/// lifecycle scripts, and run the ones the policy allows. Runs
/// `preinstall` → `install` → `postinstall` per package in that order;
/// `prepare` is skipped for deps (pnpm does the same).
///
/// `package_indices` gives us the stored `package.json` for each dep
/// without a second disk read, and the actual execution cwd is
/// `node_modules/.aube/<dep_path>/node_modules/<name>` — i.e. the
/// linked dir inside the virtual store.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_dep_lifecycle_scripts(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: &aube_scripts::BuildPolicy,
    virtual_store_dir_max_length: usize,
    child_concurrency: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    side_effects_cache: SideEffectsCacheConfig<'_>,
) -> miette::Result<usize> {
    // Pass 1 (serial, cheap): walk the graph, keep only the packages
    // the policy allows AND that actually define at least one dep
    // lifecycle hook in their on-disk `package.json`. Filtering up front
    // means the fan-out below only spawns real work — no tokio task per
    // every 200-package graph for a graph that has 3 allowlisted deps.
    #[derive(Clone)]
    struct BuildJob {
        name: String,
        version: String,
        package_dir: std::path::PathBuf,
        /// Directory containing the dep package and its sibling
        /// symlinks — i.e. `package_dir`'s enclosing `node_modules/`.
        /// `<dep_modules_dir>/.bin` is prepended to PATH so the
        /// postinstall can call binaries declared in the dep's own
        /// `dependencies`. See `link_dep_bins` for the write side.
        dep_modules_dir: std::path::PathBuf,
        manifest: aube_manifest::PackageJson,
        cache_entry: Option<SideEffectsCacheEntry>,
    }

    let mut jobs: Vec<BuildJob> = Vec::new();
    for (dep_path, pkg) in &graph.packages {
        // Use registry_name(), not pkg.name. pkg.name is the in-tree
        // alias (`h3-safe`). Real package is `h3`. Allowlist entry for
        // `h3` would miss if we checked against the alias. Attacker
        // writes `"h3-safe": "npm:h3@0.19.0"` to sneak a denied pkg
        // through the allowlist. registry_name() strips alias back to
        // real name.
        match policy.decide(pkg.registry_name(), &pkg.version) {
            aube_scripts::AllowDecision::Allow => {}
            aube_scripts::AllowDecision::Deny | aube_scripts::AllowDecision::Unspecified => {
                continue;
            }
        }
        let package_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !package_dir.exists() {
            tracing::debug!(
                "allowBuilds: skipping {} — {} not on disk",
                pkg.name,
                package_dir.display()
            );
            continue;
        }
        // Read the dep's `package.json` directly from its materialized
        // location. Previously we looked it up via `package_indices`,
        // but the fetch phase now skips `load_index` for packages
        // whose virtual-store entry already exists (which is every
        // package on a no-op re-install), so the map is sparse and
        // many dep_paths legitimately won't have an entry. The
        // on-disk file is hardlinked to the same bytes the store
        // would have pointed us at.
        //
        // `NotFound` is the only error we swallow here: some packages
        // legitimately ship without a top-level `package.json` (or
        // the field gets stripped by linkers that treat the virtual
        // store as opaque), and we shouldn't fail the install over
        // that. Every other I/O error — permission denied, disk
        // corruption, short reads — surfaces as a hard failure so
        // the user sees the real problem instead of a silently
        // skipped `node-gyp rebuild` or similar.
        let pkg_json_path = package_dir.join("package.json");
        let pkg_json_content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for {} at {}: {}",
                    pkg.name,
                    pkg_json_path.display(),
                    e
                ));
            }
        };
        let dep_manifest = aube_manifest::PackageJson::parse(&pkg_json_path, pkg_json_content)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to parse package.json for {}", pkg.name))?;
        // `has_dep_lifecycle_work` also accounts for the implicit
        // `node-gyp rebuild` fallback: a package with a top-level
        // `binding.gyp` and no `install`/`preinstall` script still has
        // work to run, and pre-filtering on `scripts` alone would drop
        // it before the fan-out even saw it.
        if !aube_scripts::has_dep_lifecycle_work(&package_dir, &dep_manifest) {
            continue;
        }
        let cache_entry = side_effects_cache
            .root()
            .map(|root| SideEffectsCacheEntry::new(root, &pkg.name, &pkg.version, &package_dir))
            .transpose()?;
        let dep_modules_dir = dep_modules_dir_for(&package_dir, &pkg.name);
        jobs.push(BuildJob {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            package_dir,
            dep_modules_dir,
            manifest: dep_manifest,
            cache_entry,
        });
    }

    if jobs.is_empty() {
        return Ok(0);
    }

    // Bootstrap node-gyp once before the fan-out when the ambient
    // `PATH` doesn't already provide one. At least one job is about
    // to run a lifecycle script, and we can't cheaply predict which
    // ones will end up shelling out to `node-gyp` (explicit,
    // implicit via binding.gyp, or transitive via node-gyp-build).
    // If the user already has node-gyp (system install, nvm, a test
    // shim), `ensure` returns `None` and we leave their copy alone.
    let node_gyp_bin_dir = std::sync::Arc::new(node_gyp_bootstrap::ensure(project_dir).await?);

    // Pass 2 (parallel, bounded): fan out across `child_concurrency`
    // concurrent workers. Inside one job the three hooks
    // (preinstall → install → postinstall) still run sequentially —
    // pnpm's execution model is "at most N packages building in
    // parallel," not "at most N scripts running," so hook ordering
    // within a single package is preserved.
    //
    // Cancellation on first failure uses `JoinSet`, which aborts every
    // outstanding task when it's dropped. A plain `Vec<JoinHandle>`
    // would NOT be safe here — dropping a `tokio::spawn` handle lets
    // the task keep running detached, so a failing script would
    // silently leave N siblings still executing `postinstall` against
    // the user's machine after the install returned an error.
    // `join_next` also surfaces whichever task fails first rather than
    // waiting for the longest-running one to finish.
    let concurrency = child_concurrency.max(1);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let project_dir = project_dir.to_path_buf();
    let modules_dir_name = modules_dir_name.to_string();
    let should_restore_side_effects_cache = side_effects_cache.should_restore();
    let should_save_side_effects_cache = side_effects_cache.should_save();
    let overwrite_side_effects_cache = side_effects_cache.overwrite_existing();
    let mut set: tokio::task::JoinSet<miette::Result<usize>> = tokio::task::JoinSet::new();
    for job in jobs {
        let sem = semaphore.clone();
        let project_dir = project_dir.clone();
        let modules_dir_name = modules_dir_name.clone();
        let node_gyp_bin_dir = node_gyp_bin_dir.clone();
        set.spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            if should_restore_side_effects_cache && let Some(cache_entry) = job.cache_entry.clone()
            {
                let package_dir = job.package_dir.clone();
                let restore_result = tokio::task::spawn_blocking(move || {
                    cache_entry.restore_if_available(&package_dir)
                })
                .await
                .map_err(|e| {
                    miette!(
                        "side-effects-cache restore task panicked for {}@{}: {e}",
                        job.name,
                        job.version
                    )
                })?;
                match restore_result? {
                    SideEffectsCacheRestore::Restored | SideEffectsCacheRestore::AlreadyApplied => {
                        return Ok(0);
                    }
                    SideEffectsCacheRestore::Miss => {}
                }
            }
            let tool_dirs: Vec<&std::path::Path> = node_gyp_bin_dir
                .as_ref()
                .as_deref()
                .map(|p| vec![p])
                .unwrap_or_default();
            let mut ran_here = 0usize;
            for hook in aube_scripts::DEP_LIFECYCLE_HOOKS {
                let did_run = aube_scripts::run_dep_hook(
                    &job.package_dir,
                    &job.dep_modules_dir,
                    &project_dir,
                    &modules_dir_name,
                    &job.manifest,
                    hook,
                    &tool_dirs,
                )
                .await
                .map_err(|e| {
                    miette!(
                        "lifecycle script {} failed for {}@{}: {}",
                        hook.script_name(),
                        job.name,
                        job.version,
                        e
                    )
                })?;
                if did_run {
                    tracing::debug!(
                        "ran {} for {}@{}",
                        hook.script_name(),
                        job.name,
                        job.version
                    );
                    ran_here += 1;
                }
            }
            if should_save_side_effects_cache
                && ran_here > 0
                && let Some(cache_entry) = job.cache_entry.clone()
            {
                let package_dir = job.package_dir.clone();
                let save_result = tokio::task::spawn_blocking(move || {
                    cache_entry.save(&package_dir, overwrite_side_effects_cache)
                })
                .await
                .map_err(|e| {
                    miette!(
                        "side-effects-cache save task panicked for {}@{}: {e}",
                        job.name,
                        job.version
                    )
                })
                .and_then(|r| r);
                if let Err(e) = save_result {
                    tracing::debug!(
                        "side-effects-cache: ignoring cache save error for {}@{}: {e}",
                        job.name,
                        job.version
                    );
                }
            }
            Ok(ran_here)
        });
    }

    let mut ran = 0usize;
    while let Some(res) = set.join_next().await {
        // `?` on the outer `Result` propagates a real task-level panic
        // (tokio's `JoinError`); `?` on the inner `miette::Result`
        // propagates a script failure. Either way, the function
        // returns, `set` is dropped, and the remaining in-flight
        // scripts are aborted before they can scribble on disk.
        ran += res.into_diagnostic()??;
    }
    Ok(ran)
}

/// Verify + import + validate + save-index for a freshly fetched
/// tarball. Shared between the lockfile-driven fetch path and the
/// no-lockfile streaming fetch path so both honor the same integrity
/// and content-check settings. Runs inside `spawn_blocking` — no
/// async in this function.
#[allow(clippy::too_many_arguments)]
pub(super) fn import_verified_tarball(
    store: &aube_store::Store,
    bytes: &[u8],
    display_name: &str,
    registry_name: &str,
    version: &str,
    integrity: Option<&str>,
    verify_integrity: bool,
    strict_integrity: bool,
    strict_pkg_content_check: bool,
) -> miette::Result<aube_store::PackageIndex> {
    if verify_integrity {
        if let Some(expected) = integrity {
            aube_store::verify_integrity(bytes, expected)
                .map_err(|e| miette!("{display_name}@{version}: {e}"))?;
        } else if strict_integrity {
            // strict-store-integrity=true opts the user into
            // fail-closed. Default is off so ecosystem parity with
            // pnpm stays intact. A registry proxy that strips
            // dist.integrity will no longer slip past silently when
            // strict is on.
            return Err(miette!(
                "{display_name}@{version}: registry response has no `dist.integrity` and `strict-store-integrity` is on. Refusing to import unverified bytes."
            ));
        } else {
            tracing::warn!(
                "{display_name}@{version}: registry response has no `dist.integrity`, importing without content verification. Set `strict-store-integrity=true` to refuse instead."
            );
        }
    }
    let index = store
        .import_tarball(bytes)
        .map_err(|e| miette!("failed to import {display_name}@{version}: {e}"))?;
    // strictStorePkgContentCheck: cross-check the freshly stored
    // package.json against the resolver-asserted (name, version)
    // before the index is cached or returned to the linker. Validate
    // against `registry_name` — the real package name that appears
    // in the tarball's own `package.json` — not the alias, or this
    // would fail every npm-aliased entry.
    if strict_pkg_content_check {
        aube_store::validate_pkg_content(&index, registry_name, version)
            .map_err(|e| miette!("{display_name}@{version}: {e}"))?;
    }
    // Cache under `registry_name` so two aliases of the same real
    // package hit the same on-disk index file and avoid redundant
    // fetches. When `integrity` is `Some` the filename carries a
    // `+<hex>` suffix that discriminates same-(name, version)
    // tarballs from different sources; when `None` falls back to the
    // plain name@version key so warm installs still find the cache
    // on integrity-stripping proxies.
    if let Err(e) = store.save_index(registry_name, version, integrity, &index) {
        tracing::warn!("Failed to cache index for {display_name}@{version}: {e}");
    }
    Ok(index)
}

pub(super) fn validate_required_scripts(
    project_dir: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    required: &[String],
) -> miette::Result<()> {
    if required.is_empty() {
        return Ok(());
    }
    let mut missing = Vec::new();
    collect_missing_required_scripts(".", manifest, required, &mut missing);
    for pkg_dir in aube_workspace::find_workspace_packages(project_dir)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?
    {
        let manifest_path = pkg_dir.join("package.json");
        let pkg_manifest = aube_manifest::PackageJson::from_path(&manifest_path)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
        let label = pkg_manifest
            .name
            .as_deref()
            .map(str::to_string)
            .unwrap_or_else(|| {
                pkg_dir
                    .strip_prefix(project_dir)
                    .unwrap_or(&pkg_dir)
                    .display()
                    .to_string()
            });
        collect_missing_required_scripts(&label, &pkg_manifest, required, &mut missing);
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(miette!(
            "requiredScripts check failed:\n{}",
            missing
                .into_iter()
                .map(|(pkg, script)| format!("  - {pkg} is missing `{script}`"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn collect_missing_required_scripts(
    label: &str,
    manifest: &aube_manifest::PackageJson,
    required: &[String],
    missing: &mut Vec<(String, String)>,
) {
    for script in required {
        if !manifest.scripts.contains_key(script) {
            missing.push((label.to_string(), script.clone()));
        }
    }
}

pub(super) fn unreviewed_dep_builds(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: &aube_scripts::BuildPolicy,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Vec<String>> {
    let mut unreviewed = Vec::new();
    for (dep_path, pkg) in &graph.packages {
        if !matches!(
            policy.decide(pkg.registry_name(), &pkg.version),
            aube_scripts::AllowDecision::Unspecified
        ) {
            continue;
        }
        let package_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !package_dir.exists() {
            continue;
        }
        let pkg_json_path = package_dir.join("package.json");
        let pkg_json_content = match std::fs::read_to_string(&pkg_json_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for {} at {}: {}",
                    pkg.name,
                    pkg_json_path.display(),
                    e
                ));
            }
        };
        let dep_manifest = aube_manifest::PackageJson::parse(&pkg_json_path, pkg_json_content)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to parse package.json for {}", pkg.name))?;
        if aube_scripts::has_dep_lifecycle_work(&package_dir, &dep_manifest) {
            unreviewed.push(pkg.spec_key());
        }
    }
    unreviewed.sort();
    unreviewed.dedup();
    Ok(unreviewed)
}
