//! `aube dedupe` — collapse redundant lockfile versions by re-resolving fresh.
//!
//! The resolver's `resolve(&manifest, existing)` reuses versions from `existing`
//! when they satisfy a range. Passing `existing = None` forces a fresh resolve
//! that always picks the highest version satisfying each range, which
//! naturally collapses duplicates left over from past adds/removes/updates.

use super::{install, make_client, packument_cache_dir};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Args)]
pub struct DedupeArgs {
    /// Check whether dedupe would change the lockfile; don't write anything.
    ///
    /// Exits non-zero when dedupe would make changes — useful in CI.
    #[arg(long)]
    pub check: bool,
}

pub async fn run(args: DedupeArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;

    let manifest = aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;

    // Read the existing lockfile purely for the diff. We do NOT pass it to
    // the resolver — passing `None` is what makes this "dedupe" instead of
    // "install": the resolver won't reuse stale pinned versions.
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();

    // Discover workspace packages so we resolve every importer, not just
    // the root package.json. Without this, dedupe would produce a graph
    // missing workspace importers, diff would be wrong, and the subsequent
    // `install::run` would see drift and silently re-resolve on top.
    let workspace_packages = aube_workspace::find_workspace_packages(&cwd)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;
    let is_workspace = !workspace_packages.is_empty();

    let mut manifests: Vec<(String, aube_manifest::PackageJson)> =
        vec![(".".to_string(), manifest.clone())];
    let mut ws_package_versions: HashMap<String, String> = HashMap::new();

    if is_workspace {
        for pkg_dir in &workspace_packages {
            let pkg_manifest = aube_manifest::PackageJson::from_path(&pkg_dir.join("package.json"))
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to read {}/package.json", pkg_dir.display()))?;

            let rel_path = pkg_dir
                .strip_prefix(&cwd)
                .unwrap_or(pkg_dir)
                .to_string_lossy()
                .to_string();

            if let Some(name) = &pkg_manifest.name {
                let version = pkg_manifest.version.as_deref().unwrap_or("0.0.0");
                ws_package_versions.insert(name.clone(), version.to_string());
            }

            manifests.push((rel_path, pkg_manifest));
        }
    }

    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    let client = std::sync::Arc::new(make_client(&cwd));
    let mut resolver = aube_resolver::Resolver::new(client)
        .with_packument_cache(packument_cache_dir())
        .with_catalogs(workspace_catalogs);
    let graph = resolver
        .resolve_workspace(&manifests, None, &ws_package_versions)
        .await
        .into_diagnostic()
        .wrap_err("failed to resolve dependencies")?;

    let (removed, added) = diff_graphs(existing.as_ref(), &graph);

    // No changes: report and exit cleanly.
    if removed.is_empty() && added.is_empty() {
        eprintln!(
            "Lockfile is already deduped ({} packages)",
            graph.packages.len()
        );
        return Ok(());
    }

    // Changes would happen. Report the diff.
    for dep_path in &removed {
        eprintln!("  - {dep_path}");
    }
    for dep_path in &added {
        eprintln!("  + {dep_path}");
    }
    eprintln!(
        "Dedupe: {} removed, {} added (net {} packages)",
        removed.len(),
        added.len(),
        graph.packages.len() as i64 - existing.as_ref().map_or(0, |g| g.packages.len()) as i64,
    );

    if args.check {
        return Err(miette!("dedupe --check: lockfile is not deduped"));
    }

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

    // Resync node_modules against the new lockfile.
    install::run(install::InstallOptions::with_mode(
        super::chained_frozen_mode(install::FrozenMode::Prefer),
    ))
    .await?;

    Ok(())
}

/// Diff two lockfile graphs by dep_path. Returns `(removed, added)` — packages
/// present in `old` but not `new`, and vice versa. Versions that are in both
/// are omitted (they're untouched).
fn diff_graphs(
    existing: Option<&aube_lockfile::LockfileGraph>,
    new: &aube_lockfile::LockfileGraph,
) -> (Vec<String>, Vec<String>) {
    let empty: BTreeMap<String, aube_lockfile::LockedPackage> = BTreeMap::new();
    let old_pkgs = existing.map(|g| &g.packages).unwrap_or(&empty);
    let old_keys: BTreeSet<&String> = old_pkgs.keys().collect();
    let new_keys: BTreeSet<&String> = new.packages.keys().collect();

    let removed: Vec<String> = old_keys
        .difference(&new_keys)
        .map(|s| s.to_string())
        .collect();
    let added: Vec<String> = new_keys
        .difference(&old_keys)
        .map(|s| s.to_string())
        .collect();
    (removed, added)
}
