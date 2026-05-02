use super::run::load_manifest;
use aube_scripts::LifecycleHook;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeSet;

/// `aube rebuild` — re-run the root package's preinstall hook, then
/// install / postinstall work for dependency packages allowed by the active
/// `allowBuilds` / `onlyBuiltDependencies` policy, then the root package's
/// install / postinstall / prepare lifecycle hooks.
///
/// When one or more `<package>` positional args are given, only those
/// dependencies' lifecycle scripts run — root hooks are skipped, matching
/// `pnpm rebuild <pkg>`.
///
/// Unlike the other lifecycle shortcuts, `rebuild` intentionally does not
/// auto-install: `aube install` already runs these same four hooks after
/// linking, so triggering an install here would double-run every script
/// on a stale tree. Users who actually want a fresh install should run
/// `aube install`.
#[derive(Debug, Clone, Args)]
pub struct RebuildArgs {
    /// Package(s) to rebuild (all allowlisted deps + root hooks if empty)
    pub packages: Vec<String>,
}

pub async fn run(
    args: RebuildArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() {
        return run_filtered(args, &filter).await;
    }

    let selective = !args.packages.is_empty();
    let name_filter: Option<BTreeSet<String>> =
        selective.then(|| args.packages.iter().cloned().collect());

    let cwd = crate::dirs::project_root()?;
    let manifest = load_manifest(&cwd)?;
    let npmrc_entries = aube_registry::config::load_npmrc_entries(&cwd);
    let (workspace, raw_workspace) = aube_manifest::workspace::load_both(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let settings_ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env: &env_snapshot,
        cli: &[],
    };
    super::configure_script_settings(&settings_ctx);

    // Read the lockfile from the project, or fall back to the shared
    // workspace-root one when the project doesn't have its own (the
    // common shape inside a workspace fanout — `run_filtered` retargets
    // CWD to each sub-project, but the lockfile lives at the root).
    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(graph) => Some(graph),
        Err(aube_lockfile::Error::NotFound(_)) => super::find_workspace_root(&cwd)
            .ok()
            .filter(|ws| ws.as_path() != cwd.as_path())
            .and_then(|ws| aube_lockfile::parse_lockfile(&ws, &manifest).ok()),
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let (policy, policy_warnings) =
        super::install::build_policy_from_sources(&manifest, &workspace, false);
    if let Some(names) = name_filter.as_ref() {
        let Some(graph) = graph.as_ref() else {
            return Err(miette!(
                "no lockfile found for package(s) {}; run `aube install` first",
                names
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let known: BTreeSet<&str> = graph
            .packages
            .values()
            .flat_map(|p| [p.name.as_str(), p.registry_name()])
            .collect();
        let unknown: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|n| !known.contains(n))
            .collect();
        if !unknown.is_empty() {
            return Err(miette!(
                "package(s) {} not a dependency in the current install",
                unknown
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        // The named package is in the graph; check it's also allowlisted
        // by the active `allowBuilds` / `onlyBuiltDependencies` policy.
        // Without this check, `aube rebuild blocked-pkg` silently
        // succeeds because `run_dep_lifecycle_scripts` filters by both
        // `name_filter` and `policy.decide()`.
        let blocked: Vec<String> = names
            .iter()
            .filter(|n| {
                graph
                    .packages
                    .values()
                    .filter(|p| p.name == **n || p.registry_name() == n.as_str())
                    .all(|p| {
                        !matches!(
                            policy.decide(&p.name, &p.version),
                            aube_scripts::AllowDecision::Allow
                        )
                    })
            })
            .map(String::clone)
            .collect();
        if !blocked.is_empty() {
            return Err(miette!(
                "package(s) {} are not allowlisted to run build scripts; \
                 run `aube approve-builds` to add them",
                blocked
                    .iter()
                    .map(|n| format!("'{n}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);

    if !selective {
        aube_scripts::run_root_hook(
            &cwd,
            &modules_dir_name,
            &manifest,
            LifecycleHook::PreInstall,
        )
        .await
        .map_err(|e| miette!("{}", e))?;
    }

    if let Some(graph) = graph {
        for warning in policy_warnings {
            eprintln!("warn: {warning}");
        }

        if policy.has_any_allow_rule() {
            let child_concurrency =
                aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
            let (jail_policy, jail_policy_warnings) =
                super::install::JailBuildPolicy::from_settings(&settings_ctx, &workspace);
            for warning in jail_policy_warnings {
                eprintln!("warn: {warning}");
            }
            // The generated accessor already reads `nodeLinker` from
            // `raw_workspace`, which is the same map `workspace.node_linker`
            // is parsed out of — no need for a separate fallback on the
            // typed struct field.
            let node_linker_setting = aube_settings::resolved::node_linker(&settings_ctx);
            let hoisted_placements = match node_linker_setting {
                aube_settings::resolved::NodeLinker::Pnp => {
                    return Err(miette!(
                        "node-linker=pnp is not supported by aube; use `isolated` (default) or `hoisted`"
                    ));
                }
                aube_settings::resolved::NodeLinker::Hoisted => Some(
                    aube_linker::HoistedPlacements::from_graph(&cwd, &graph, &modules_dir_name),
                ),
                aube_settings::resolved::NodeLinker::Isolated => None,
            };
            let side_effects_cache_root =
                if aube_settings::resolved::side_effects_cache(&settings_ctx) {
                    let store = super::open_store(&cwd)?;
                    Some(super::install::side_effects_cache_root(&store))
                } else {
                    None
                };
            // Re-emit per-dep `.bin/` shims so a rebuild on a tree
            // that pre-dates the transitive-bin fix still lands them
            // on PATH for the lifecycle scripts. `link_bins_for_dep`
            // is idempotent, so re-running on an already-wired tree
            // is a no-op.
            let shim_opts = aube_linker::BinShimOptions {
                extend_node_path: aube_settings::resolved::extend_node_path(&settings_ctx),
                prefer_symlinked_executables: aube_settings::resolved::prefer_symlinked_executables(
                    &settings_ctx,
                ),
            };
            let mut pkg_json_cache = super::install::PkgJsonCache::new();
            super::install::link_dep_bins(
                &aube_dir,
                &graph,
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
                hoisted_placements.as_ref(),
                shim_opts,
                &mut pkg_json_cache,
            )?;
            super::install::run_dep_lifecycle_scripts(
                &cwd,
                &modules_dir_name,
                &aube_dir,
                &graph,
                &policy,
                super::resolve_virtual_store_dir_max_length(&settings_ctx),
                child_concurrency,
                hoisted_placements.as_ref(),
                side_effects_cache_root
                    .as_deref()
                    .map(|root| {
                        // `rebuild` means "run scripts again"; readonly
                        // cache may not write, but it must not restore and
                        // skip the script work either.
                        if aube_settings::resolved::side_effects_cache_readonly(&settings_ctx) {
                            super::install::SideEffectsCacheConfig::Disabled
                        } else {
                            super::install::SideEffectsCacheConfig::SaveOnlyOverwrite(root)
                        }
                    })
                    .unwrap_or(super::install::SideEffectsCacheConfig::Disabled),
                &jail_policy,
                name_filter.as_ref(),
            )
            .await?;
        }
    }

    if !selective {
        for hook in [
            LifecycleHook::Install,
            LifecycleHook::PostInstall,
            LifecycleHook::Prepare,
        ] {
            aube_scripts::run_root_hook(&cwd, &modules_dir_name, &manifest, hook)
                .await
                .map_err(|e| miette!("{}", e))?;
        }
    }

    Ok(())
}

async fn run_filtered(
    args: RebuildArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "rebuild")?;
    let result = async {
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            Box::pin(run(
                args.clone(),
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}
