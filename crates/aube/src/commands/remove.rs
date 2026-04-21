use super::{install, make_client, packument_cache_dir};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    /// Package(s) to remove
    pub packages: Vec<String>,
    /// Remove only from devDependencies
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Remove from the global install directory instead of the project
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Skip root lifecycle scripts during the chained reinstall
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Remove the dependency from the workspace root's `package.json`,
    /// regardless of the current working directory.
    ///
    /// Walks up from cwd looking for `aube-workspace.yaml`,
    /// `pnpm-workspace.yaml`, or a `package.json` with a `workspaces`
    /// field and runs the remove against that directory. Takes
    /// precedence over `--filter` when both are supplied (same as
    /// `add --workspace`).
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
}

pub async fn run(
    args: RemoveArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let packages = &args.packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }

    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    if args.global {
        return run_global(packages);
    }

    // `--workspace` / `-w`: redirect the remove at the workspace root
    // before anything reads `dirs::cwd()`.
    if args.workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;

    for name in packages {
        let removed = if args.save_dev {
            manifest.dev_dependencies.remove(name).is_some()
        } else {
            // Clear every section that could hold this name — `--save-peer`
            // may have written to both `peerDependencies` and
            // `devDependencies` simultaneously, so we need to strip both to
            // fully uninstall.
            let from_deps = manifest.dependencies.remove(name).is_some();
            let from_dev = manifest.dev_dependencies.remove(name).is_some();
            let from_optional = manifest.optional_dependencies.remove(name).is_some();
            let from_peer = manifest.peer_dependencies.remove(name).is_some();
            from_deps || from_dev || from_optional || from_peer
        };

        if !removed {
            let section = if args.save_dev {
                "a devDependency"
            } else {
                "a dependency"
            };
            return Err(miette!("package '{name}' is not {section}"));
        }

        eprintln!("  - {name}");
    }

    // Write updated package.json
    let json = serde_json::to_string_pretty(&manifest)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    std::fs::write(&manifest_path, format!("{json}\n"))
        .into_diagnostic()
        .wrap_err("failed to write package.json")?;
    eprintln!("Updated package.json");

    // Re-resolve dependency tree without the removed packages
    let client = std::sync::Arc::new(make_client(&cwd));
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();
    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    let mut resolver = aube_resolver::Resolver::new(client)
        .with_packument_cache(packument_cache_dir())
        .with_catalogs(workspace_catalogs);
    let graph = resolver
        .resolve(&manifest, existing.as_ref())
        .await
        .into_diagnostic()
        .wrap_err("failed to resolve dependencies")?;
    eprintln!("Resolved {} packages", graph.packages.len());

    let written_path = aube_lockfile::write_lockfile_preserving_existing(&cwd, &graph, &manifest)
        .into_diagnostic()
        .wrap_err("failed to write lockfile")?;
    eprintln!(
        "Wrote {}",
        written_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| written_path.display().to_string())
    );

    // Reinstall to clean up node_modules
    let mut opts =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer));
    opts.ignore_scripts = args.ignore_scripts;
    install::run(opts).await?;

    Ok(())
}

async fn run_filtered(
    args: RemoveArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "remove")?;
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

/// `aube remove -g <pkg>...` — delete globally-installed packages and
/// unlink their bins. Each named package is looked up in the global pkg
/// dir; if found, the whole install (hash symlink + physical dir + bins)
/// is removed atomically.
fn run_global(packages: &[String]) -> miette::Result<()> {
    let layout = super::global::GlobalLayout::resolve()?;

    let mut any_removed = false;
    for name in packages {
        match super::global::find_package(&layout.pkg_dir, name) {
            Some(info) => {
                super::global::remove_package(&info, &layout)?;
                eprintln!("Removed global {name}");
                any_removed = true;
            }
            None => {
                eprintln!("Not globally installed: {name}");
            }
        }
    }
    if !any_removed {
        return Err(miette!("no matching global packages were removed"));
    }
    Ok(())
}
