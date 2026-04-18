use super::run::load_manifest;
use aube_scripts::LifecycleHook;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

/// `aube rebuild` — re-run the root package's preinstall hook, then
/// install / postinstall work for dependency packages allowed by the active
/// `allowBuilds` / `onlyBuiltDependencies` policy, then the root package's
/// install / postinstall / prepare lifecycle hooks.
///
/// Unlike the other lifecycle shortcuts, `rebuild` intentionally does not
/// auto-install: `aube install` already runs these same four hooks after
/// linking, so triggering an install here would double-run every script
/// on a stale tree. Users who actually want a fresh install should run
/// `aube install`.
#[derive(Debug, Args)]
pub struct RebuildArgs {}

pub async fn run(
    _args: RebuildArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() {
        return run_filtered(&filter).await;
    }

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

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(graph) => Some(graph),
        Err(aube_lockfile::Error::NotFound(_)) => None,
        Err(e) => return Err(miette!(e)).wrap_err("failed to parse lockfile"),
    };

    let modules_dir_name = aube_settings::resolved::modules_dir(&settings_ctx);
    let aube_dir = super::resolve_virtual_store_dir(&settings_ctx, &cwd);
    aube_scripts::run_root_hook(
        &cwd,
        &modules_dir_name,
        &manifest,
        LifecycleHook::PreInstall,
    )
    .await
    .map_err(|e| miette!("{}", e))?;

    if let Some(graph) = graph {
        let (policy, warnings) =
            super::install::build_policy_from_sources(&manifest, &workspace, false);
        for warning in warnings {
            eprintln!("warn: {warning}");
        }

        if policy.has_any_allow_rule() {
            let child_concurrency =
                aube_settings::resolved::child_concurrency(&settings_ctx) as usize;
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
            )
            .await?;
        }
    }

    for hook in [
        LifecycleHook::Install,
        LifecycleHook::PostInstall,
        LifecycleHook::Prepare,
    ] {
        aube_scripts::run_root_hook(&cwd, &modules_dir_name, &manifest, hook)
            .await
            .map_err(|e| miette!("{}", e))?;
    }

    Ok(())
}

async fn run_filtered(filter: &aube_workspace::selector::EffectiveFilter) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let matched = super::select_workspace_packages(&cwd, filter, "rebuild")?;
    let result = async {
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            Box::pin(run(
                RebuildArgs {},
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    let restore_result = super::retarget_cwd(&cwd)
        .wrap_err_with(|| format!("failed to restore cwd to {}", cwd.display()));
    match result {
        Ok(()) => restore_result,
        Err(err) => {
            let _ = restore_result;
            Err(err)
        }
    }
}
