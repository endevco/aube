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
            // Strip from every section. `--save-peer` previously
            // wrote to both peerDependencies and devDependencies so
            // both need clearing on a full uninstall.
            let from_deps = manifest.dependencies.remove(name).is_some();
            let from_dev = manifest.dev_dependencies.remove(name).is_some();
            let from_optional = manifest.optional_dependencies.remove(name).is_some();
            let from_peer = manifest.peer_dependencies.remove(name).is_some();
            from_deps || from_dev || from_optional || from_peer
        };

        // Also prune sidecar metadata so a later `aube add <name>`
        // does not silently inherit the old entries. Main concern is
        // pnpm.allowBuilds. If user removes a build-script package
        // then later adds a malicious package with the same name
        // (typo-squat, name reclaim), the old allowBuilds entry
        // would auto-approve its postinstall. Same hazard, lower
        // risk, for overrides and resolutions which just leave dead
        // rewrite rules around. Matches pnpm remove behavior.
        prune_sidecar_entries(&mut manifest, name);

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

    // Write updated package.json atomically. Crash mid-write would
    // otherwise truncate the user manifest, worst-case aube failure
    // mode. Tempfile + persist keeps the swap atomic.
    let json = serde_json::to_string_pretty(&manifest)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    write_manifest_atomic(&manifest_path, format!("{json}\n").as_bytes())?;
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
        .map_err(miette::Report::new)
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

/// Prune aube/pnpm sidecar metadata entries that reference `name`.
/// Covers pnpm.allowBuilds, pnpm.onlyBuiltDependencies,
/// pnpm.neverBuiltDependencies, pnpm.overrides, aube.* mirrors,
/// top-level overrides, yarn resolutions. Also removes the whole
/// namespace block if its last entry was the one we just dropped.
/// Safe no-op if the manifest has none of these fields.
fn prune_sidecar_entries(manifest: &mut aube_manifest::PackageJson, name: &str) {
    // Namespaced (pnpm.* / aube.*) allowlists, overrides, denylists.
    for ns_key in ["pnpm", "aube"] {
        let Some(ns) = manifest.extra.get_mut(ns_key) else {
            continue;
        };
        let Some(obj) = ns.as_object_mut() else {
            continue;
        };
        // Map-shape fields: key is package name.
        for map_key in ["allowBuilds", "overrides", "peerDependencyRules"] {
            if let Some(inner) = obj.get_mut(map_key).and_then(|v| v.as_object_mut()) {
                inner.remove(name);
                // peerDependencyRules has nested allowedVersions,
                // ignoreMissing. Only clean the outer pkg-keyed
                // entries, deeper structures are author-controlled.
                if inner.is_empty() {
                    obj.remove(map_key);
                }
            }
        }
        // Array-shape fields: whole entries match name or name@ver.
        for arr_key in [
            "onlyBuiltDependencies",
            "neverBuiltDependencies",
            "trustedDependencies",
        ] {
            if let Some(arr) = obj.get_mut(arr_key).and_then(|v| v.as_array_mut()) {
                arr.retain(|entry| match entry.as_str() {
                    Some(s) => {
                        // "pkg" stays only if it is not our name.
                        // "pkg@range" stays only if pkg is not ours.
                        let base = s.rsplit_once('@').map(|(a, _)| a).unwrap_or(s);
                        base != name
                    }
                    None => true,
                });
                if arr.is_empty() {
                    obj.remove(arr_key);
                }
            }
        }
        // Drop the whole pnpm/aube block if we emptied it completely.
        if obj.is_empty() {
            manifest.extra.remove(ns_key);
        }
    }
    // Top-level `overrides` (npm + pnpm both accept it here).
    if let Some(top) = manifest
        .extra
        .get_mut("overrides")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("overrides");
        }
    }
    // yarn `resolutions` at top level.
    if let Some(top) = manifest
        .extra
        .get_mut("resolutions")
        .and_then(|v| v.as_object_mut())
    {
        top.remove(name);
        if top.is_empty() {
            manifest.extra.remove("resolutions");
        }
    }
}

fn write_manifest_atomic(path: &std::path::Path, body: &[u8]) -> miette::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".aube-remove-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to open tempfile for {}", path.display()))?;
    {
        use std::io::Write as _;
        let mut f = tmp.as_file();
        f.write_all(body)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to write tempfile for {}", path.display()))?;
        f.sync_all()
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to sync tempfile for {}", path.display()))?;
    }
    tmp.persist(path)
        .map_err(|e| miette!("failed to persist {}: {e}", path.display()))?;
    Ok(())
}
