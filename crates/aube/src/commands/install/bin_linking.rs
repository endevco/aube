use aube_lockfile::dep_path_filename::dep_path_to_filename;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) type PkgJsonCache = BTreeMap<String, Option<serde_json::Value>>;

/// Per-install cache of workspace-package `package.json` reads. Keyed
/// by the workspace dir on disk so a popular tooling package consumed
/// by many importers gets read and parsed once, not once per consumer.
pub(crate) type WsPkgJsonCache = BTreeMap<PathBuf, Option<serde_json::Value>>;

/// Link bin entries from packages to node_modules/.bin/
/// Compute the on-disk directory a dep's materialized package lives
/// in. Matches the path `aube-linker` writes under
/// `node_modules/.aube/<escaped dep_path>/node_modules/<name>`.
///
/// `virtual_store_dir_max_length` must match the value the linker
/// was built with (see `install::run` for the single source of
/// truth) — otherwise long `dep_path`s that trigger the
/// truncate-and-hash fallback inside `dep_path_to_filename` will
/// encode to a different filename than the one the linker wrote,
/// and this function will return a path that doesn't exist.
pub(crate) fn materialized_pkg_dir(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> std::path::PathBuf {
    // In hoisted mode the package was materialized directly into
    // `node_modules/<...>/<name>/` and its path is recorded in
    // `placements`. Fall back to the isolated `.aube/<dep_path>`
    // convention when either the mode is isolated (`placements` is
    // `None`) or the hoisted planner didn't place this specific
    // dep_path (e.g. filtered by `--prod` / `--no-optional`).
    // `aube_dir` is the resolved `virtualStoreDir` — the install
    // driver threads it in via `commands::resolve_virtual_store_dir`
    // so a custom override lands on the same path the linker wrote
    // to.
    if let Some(placements) = placements
        && let Some(p) = placements.package_dir(dep_path)
    {
        return p.to_path_buf();
    }
    aube_dir
        .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
        .join("node_modules")
        .join(name)
}

/// Directory holding the dep's own `node_modules/` — i.e. the dir
/// that contains both `<name>` and its sibling symlinks. For scoped
/// packages (`@scope/name`) `package_dir` is two levels below that
/// `node_modules/`, so we strip the extra `@scope` hop. Used to
/// locate the per-dep `.bin/` for transitive lifecycle-script bins.
pub(super) fn dep_modules_dir_for(package_dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    if name.starts_with('@') {
        package_dir
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    } else {
        package_dir
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| package_dir.to_path_buf())
    }
}

/// Read a dep's `package.json` from its materialized directory.
///
/// Earlier revisions of this file went through
/// `package_indices[dep_path]` and read
/// `stored.store_path.join("package.json")` from the CAS. That
/// stopped working once `fetch_packages_with_root` learned to skip
/// `load_index` for packages whose `.aube/<dep_path>` already exists
/// (the `AlreadyLinked` fast path) — the indices map is sparse on
/// warm installs, and every caller that reached for
/// `package_indices.get(..)?.get("package.json")` silently dropped
/// those deps via the `continue` or `?` on the missing key.
///
/// Read the hardlinked file at the materialized location instead:
/// same bytes, zero dependency on the sparse indices map, and
/// doesn't require a cache miss to surface when the virtual store is
/// intact.
///
/// Error policy: `Ok(None)` only when the file is legitimately
/// missing (e.g. a package that ships without a top-level
/// `package.json`, or hasn't been materialized yet). Every other
/// `std::io::Error` — permission denied, short reads, disk errors —
/// bubbles up as `Err` so the user sees a real failure instead of a
/// silently dropped bin link. Parse errors likewise propagate.
fn read_materialized_pkg_json(
    aube_dir: &std::path::Path,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    let pkg_json_path = pkg_dir.join("package.json");
    let content = match std::fs::read_to_string(&pkg_json_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(miette!(
                "failed to read package.json for {name} at {}: {e}",
                pkg_json_path.display()
            ));
        }
    };
    let value = aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
        .map_err(miette::Report::new)
        .wrap_err_with(|| format!("failed to parse package.json for {name}"))?;
    Ok(Some(value))
}

#[allow(clippy::too_many_arguments)]
fn read_materialized_pkg_json_cached(
    cache: &mut PkgJsonCache,
    store: Option<&aube_store::Store>,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
) -> miette::Result<Option<serde_json::Value>> {
    if let Some(value) = cache.get(dep_path) {
        return Ok(value.clone());
    }
    if let Some(store) = store
        && let Some(pkg) = graph.packages.get(dep_path)
        && let Some(value) =
            store.load_index_manifest(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
    {
        cache.insert(dep_path.to_string(), Some(value.clone()));
        return Ok(Some(value));
    }
    let value = read_materialized_pkg_json(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )?;
    cache.insert(dep_path.to_string(), value.clone());
    Ok(value)
}

/// Create top-level + bundled bin symlinks for one dep. Extracted so
/// both the root-importer pass (`link_bins`) and the per-workspace
/// loop use the same code path.
#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins_for_dep(
    cache: &mut PkgJsonCache,
    store: Option<&aube_store::Store>,
    aube_dir: &std::path::Path,
    bin_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    name: &str,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    let pkg_dir = materialized_pkg_dir(
        aube_dir,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    );
    if let Some(pkg_json) = read_materialized_pkg_json_cached(
        cache,
        store,
        aube_dir,
        graph,
        dep_path,
        name,
        virtual_store_dir_max_length,
        placements,
    )? && let Some(bin) = pkg_json.get("bin")
    {
        link_bin_entries(bin_dir, &pkg_dir, Some(name), bin, shim_opts)?;
    }
    link_bundled_bins(bin_dir, &pkg_dir, graph, dep_path, shim_opts)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn link_bins(
    project_dir: &std::path::Path,
    modules_dir_name: &str,
    store: Option<&aube_store::Store>,
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
    ws_dirs: Option<&BTreeMap<String, PathBuf>>,
    ws_cache: &mut WsPkgJsonCache,
) -> miette::Result<()> {
    let bin_dir = project_dir.join(modules_dir_name).join(".bin");
    std::fs::create_dir_all(&bin_dir).into_diagnostic()?;

    for dep in graph.root_deps() {
        if let Some(ws_dir) = ws_dirs.and_then(|m| m.get(&dep.name)) {
            link_bins_for_workspace_dep(ws_cache, &bin_dir, ws_dir, &dep.name, shim_opts)?;
        } else {
            link_bins_for_dep(
                cache,
                store,
                aube_dir,
                &bin_dir,
                graph,
                &dep.dep_path,
                &dep.name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
            )?;
        }
    }

    Ok(())
}

/// Link bins declared by a `workspace:` dep into the importer's
/// `.bin/`. Workspace deps don't get a `.aube/<dep_path>/` materialization
/// (the linker symlinks them straight into the importer's `node_modules/`),
/// so `link_bins_for_dep` finds nothing on disk and silently skips. Read
/// the workspace package's own `package.json` and shim each bin entry,
/// matching pnpm's behavior of exposing workspace bins to dependent
/// packages' npm scripts.
///
/// `cache` deduplicates the read+parse across importers — without it,
/// a popular tooling package consumed by N workspace members gets its
/// `package.json` read N times during a single install.
pub(super) fn link_bins_for_workspace_dep(
    cache: &mut WsPkgJsonCache,
    bin_dir: &Path,
    ws_dir: &Path,
    name: &str,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    let pkg_json = if let Some(cached) = cache.get(ws_dir) {
        cached.clone()
    } else {
        let pkg_json_path = ws_dir.join("package.json");
        let parsed = match std::fs::read_to_string(&pkg_json_path) {
            Ok(content) => Some(
                aube_manifest::parse_json::<serde_json::Value>(&pkg_json_path, content)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| {
                        format!("failed to parse package.json for workspace dep {name}")
                    })?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(miette!(
                    "failed to read package.json for workspace dep {name} at {}: {e}",
                    pkg_json_path.display()
                ));
            }
        };
        cache.insert(ws_dir.to_path_buf(), parsed.clone());
        parsed
    };
    if let Some(pkg_json) = pkg_json
        && let Some(bin) = pkg_json.get("bin")
    {
        link_bin_entries(bin_dir, ws_dir, Some(name), bin, shim_opts)?;
    }
    Ok(())
}

/// Write per-dep `.bin/` directories holding shims for each package's
/// *own* declared dependencies. Mirrors pnpm's post-link pass that
/// populates `node_modules/.pnpm/<dep_path>/node_modules/.bin/`.
///
/// Without this, a dep's lifecycle script (e.g. `unrs-resolver`'s
/// postinstall that calls `prebuild-install`) can't find transitive
/// binaries on PATH — the project-level `node_modules/.bin` only holds
/// shims for the root's *direct* deps. `run_dep_hook` prepends the
/// dep-local `.bin` (via `dep_modules_dir_for`) before the
/// project-level one so the dep's own transitive bins always win.
///
/// Isolated mode only. Hoisted mode materializes deps at the project
/// root's `node_modules/` and generally relies on the single top-level
/// `.bin`; nested transitive bins under hoisted are a known rough edge
/// and out of scope here.
pub(crate) fn link_dep_bins(
    aube_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    virtual_store_dir_max_length: usize,
    placements: Option<&aube_linker::HoistedPlacements>,
    shim_opts: aube_linker::BinShimOptions,
    cache: &mut PkgJsonCache,
) -> miette::Result<()> {
    if placements.is_some() {
        // Hoisted — skip. See function doc.
        return Ok(());
    }
    for (dep_path, pkg) in &graph.packages {
        if pkg.dependencies.is_empty() {
            continue;
        }
        let pkg_dir = materialized_pkg_dir(
            aube_dir,
            dep_path,
            &pkg.name,
            virtual_store_dir_max_length,
            placements,
        );
        if !pkg_dir.exists() {
            // Filtered by optional / platform guards, or a staging
            // hiccup. Skipping avoids blowing up the whole install on
            // a dep that was never materialized.
            continue;
        }
        let dep_modules_dir = dep_modules_dir_for(&pkg_dir, &pkg.name);
        let bin_dir = dep_modules_dir.join(".bin");
        // Don't `create_dir_all(&bin_dir)` here — most deps have
        // no child that ships a `bin`, and an eager mkdir would leave
        // empty `.bin/` directories everywhere. `create_bin_link`
        // materializes the parent the first time a shim actually
        // lands, so deps whose children contribute zero shims stay
        // empty on disk.

        for (child_name, child_version) in &pkg.dependencies {
            // Mirror the linker's self-ref guard from
            // `materialize_into`: a package that depends on its own
            // dep_path is a graph artefact, not a real edge.
            let child_dep_path = format!("{child_name}@{child_version}");
            if child_dep_path == *dep_path && child_name == &pkg.name {
                continue;
            }
            // The sibling may have been filtered (optional on another
            // platform); `link_bins_for_dep` already returns Ok when
            // the target pkg_json is absent, so just call through.
            link_bins_for_dep(
                cache,
                None,
                aube_dir,
                &bin_dir,
                graph,
                &child_dep_path,
                child_name,
                virtual_store_dir_max_length,
                placements,
                shim_opts,
            )?;
        }
    }
    Ok(())
}

/// Hoist bins declared by a package's `bundledDependencies` into
/// `bin_dir`. The bundled children live under
/// `<pkg_dir>/node_modules/<bundled>/` straight from the tarball — the
/// resolver never walks them, so they don't show up in the regular
/// packument-driven bin-linking pass and need this companion hoist.
/// Matches pnpm's post-bin-linking pass for `hasBundledDependencies`.
/// Used by both the root importer (`link_bins`) and the per-workspace
/// loop so a workspace package depending on a parent with bundled deps
/// sees the children's bins in its own `node_modules/.bin`.
fn link_bundled_bins(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    dep_path: &str,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    let Some(locked) = graph.get_package(dep_path) else {
        return Ok(());
    };
    for bundled in &locked.bundled_dependencies {
        let bundled_dir = pkg_dir.join("node_modules").join(bundled);
        let bundled_pkg_json_path = bundled_dir.join("package.json");
        let Ok(content) = std::fs::read_to_string(&bundled_pkg_json_path) else {
            continue;
        };
        let Ok(bundled_pkg_json) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(bin) = bundled_pkg_json.get("bin") else {
            continue;
        };
        link_bin_entries(bin_dir, &bundled_dir, Some(bundled), bin, shim_opts)?;
    }
    Ok(())
}

/// Shim each entry of a package.json `bin` field into `bin_dir`,
/// resolving relative targets against `pkg_dir`. Shared by the
/// dep-bin pass (`link_bins_for_dep`), bundled-deps pass
/// (`link_bundled_bins`), and importer self-bin pass (root + each
/// workspace member, discussion #228).
///
/// String-form `bin: "./x.js"` uses the basename of `pkg_name` as the
/// shim name (scope `@a/b` → `b`); the entry is silently skipped when
/// `pkg_name` is `None`. Object-form `bin: { foo: "./f" }` uses each
/// key as-is. Entries whose name or target fail
/// [`aube_linker::validate_bin_name`] / [`aube_linker::validate_bin_target`]
/// are dropped without error, matching the pnpm/npm "silently ignore
/// invalid bin" behavior.
pub(super) fn link_bin_entries(
    bin_dir: &std::path::Path,
    pkg_dir: &std::path::Path,
    pkg_name: Option<&str>,
    bin: &serde_json::Value,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    match bin {
        serde_json::Value::String(bin_path) => {
            let Some(name) = pkg_name else {
                return Ok(());
            };
            let bin_name = name.split('/').next_back().unwrap_or(name);
            if aube_linker::validate_bin_name(bin_name).is_ok()
                && aube_linker::validate_bin_target(bin_path).is_ok()
            {
                create_bin_link(bin_dir, bin_name, &pkg_dir.join(bin_path), shim_opts)?;
            }
        }
        serde_json::Value::Object(bins) => {
            for (bin_name, path) in bins {
                if let Some(path_str) = path.as_str()
                    && aube_linker::validate_bin_name(bin_name).is_ok()
                    && aube_linker::validate_bin_target(path_str).is_ok()
                {
                    create_bin_link(bin_dir, bin_name, &pkg_dir.join(path_str), shim_opts)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn create_bin_link(
    bin_dir: &std::path::Path,
    name: &str,
    target: &std::path::Path,
    shim_opts: aube_linker::BinShimOptions,
) -> miette::Result<()> {
    // `link_dep_bins` skips eager `create_dir_all` on per-dep `.bin/`.
    // Deps whose children ship no bins stay empty on disk. First shim
    // write materializes the dir on demand.
    //
    // Windows `CreateDirectoryW` returns `ERROR_ALREADY_EXISTS` (os 183)
    // when the leaf sits behind a junction in the path, even when the
    // leaf is absent. The isolated layout's `.aube/<dep_path>` is a
    // junction into the global virtual store, so every `.bin/` under it
    // hits the quirk. Fix: canonicalize the parent (`crate::dirs::canonicalize`
    // already strips the `\\?\` verbatim prefix, which would otherwise
    // trip CreateDirectoryW's own os-123 quirk, while keeping real
    // `\\?\UNC\…` share paths intact), then create the leaf on the
    // resulting plain drive path. No-op on Unix.
    #[cfg(windows)]
    let target_for_mkdir_owned = bin_dir.parent().and_then(|parent| {
        let leaf = bin_dir.file_name()?;
        let canon = crate::dirs::canonicalize(parent).ok()?;
        Some(canon.join(leaf))
    });
    #[cfg(windows)]
    let target_for_mkdir: &std::path::Path = target_for_mkdir_owned.as_deref().unwrap_or(bin_dir);
    #[cfg(not(windows))]
    let target_for_mkdir = bin_dir;
    if let Err(e) = std::fs::create_dir_all(target_for_mkdir) {
        let tolerated = e.kind() == std::io::ErrorKind::AlreadyExists && target_for_mkdir.is_dir();
        if !tolerated {
            return Err(e)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create bin directory {}", bin_dir.display()));
        }
    }
    aube_linker::create_bin_shim(target_for_mkdir, name, target, shim_opts)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to link bin `{name}` at {} -> {}",
                bin_dir.join(name).display(),
                target.display()
            )
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};

    fn locked(name: &str, version: &str, bin: BTreeMap<String, String>) -> LockedPackage {
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            dep_path: format!("{name}@{version}"),
            bin,
            ..Default::default()
        }
    }

    #[test]
    fn link_bins_reads_manifest_when_lockfile_metadata_is_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path();
        let aube_dir = project_dir.join("node_modules/.aube");
        let dep_path = "vitepress@1.6.4";
        let pkg_dir = materialized_pkg_dir(&aube_dir, dep_path, "vitepress", 120, None);
        std::fs::create_dir_all(pkg_dir.join("bin")).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"vitepress","bin":{"vitepress":"bin/vitepress.js"}}"#,
        )
        .unwrap();
        std::fs::write(pkg_dir.join("bin/vitepress.js"), "#!/usr/bin/env node\n").unwrap();

        let mut semver_bin = BTreeMap::new();
        semver_bin.insert("semver".to_string(), "bin/semver.js".to_string());

        let mut packages = BTreeMap::new();
        packages.insert(
            dep_path.to_string(),
            locked("vitepress", "1.6.4", BTreeMap::new()),
        );
        packages.insert(
            "semver@7.7.4".to_string(),
            locked("semver", "7.7.4", semver_bin),
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "vitepress".to_string(),
                dep_path: dep_path.to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^1.5.0".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        link_bins(
            project_dir,
            "node_modules",
            None,
            &aube_dir,
            &graph,
            120,
            None,
            aube_linker::BinShimOptions::default(),
            &mut PkgJsonCache::new(),
            None,
            &mut WsPkgJsonCache::new(),
        )
        .unwrap();

        assert!(project_dir.join("node_modules/.bin/vitepress").exists());
    }
}
