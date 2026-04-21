//! `aube list` / `ls` — print the installed dependency tree.
//!
//! Reads `package.json` and the lockfile, walks the resolved graph, and
//! prints the root importer's direct deps (and optionally their transitive
//! subtrees) grouped by dependency type. Mirrors `pnpm list` / `npm ls`
//! closely enough that existing tooling regexes keep working.
//!
//! No state changes — this is a pure read. Doesn't touch `node_modules/`,
//! doesn't hit the network, doesn't take the project lock.

use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube list
  my-app@1.0.0 /home/user/project

  dependencies:
  ├── express 4.19.2
  ├── lodash 4.17.21
  └── zod 3.23.8

  devDependencies:
  ├── typescript 5.4.5
  └── vitest 1.6.0

  # Show the full transitive tree
  $ aube list --depth Infinity
  my-app@1.0.0 /home/user/project

  dependencies:
  ├─┬ express 4.19.2
  │ ├── accepts 1.3.8
  │ ├── body-parser 1.20.2
  │ └── ...

  # Only direct production deps
  $ aube list --prod

  # Machine-readable: one path per line (real store locations)
  $ aube list --parseable
  /home/user/project
  /home/user/project/node_modules/.aube/express@4.19.2/node_modules/express
  /home/user/project/node_modules/.aube/lodash@4.17.21/node_modules/lodash

  # Filter to a single package
  $ aube list express
";

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Optional package name (or glob-like prefix match) to filter the output
    pub pattern: Option<String>,

    /// Show only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// List globally-installed packages instead of the project's dependency tree
    #[arg(short = 'g', long)]
    pub global: bool,

    /// Show only production dependencies (skip devDependencies)
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,

    /// How deep to render the transitive tree.
    ///
    /// `0` (default) shows only the top-level direct deps. Pass
    /// `9999` (or any large number) for the full graph;
    /// `--depth=Infinity` is accepted for pnpm/npm compat.
    #[arg(long, default_value = "0", value_parser = parse_depth)]
    pub depth: usize,

    /// Output format: one of `default`, `json`, or `parseable`
    #[arg(long, value_enum, default_value_t = ListFormat::Default)]
    pub format: ListFormat,

    /// Shortcut for `--format json`.
    ///
    /// Emit a JSON array of package entries.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Show version and path for each entry (default output is already
    /// name + version; `--long` adds the store path for debugging).
    #[arg(long)]
    pub long: bool,

    /// Shortcut for `--format parseable`.
    ///
    /// Emit one tab-separated line per package.
    #[arg(long, conflicts_with_all = ["format", "json"])]
    pub parseable: bool,
}

/// `--depth=Infinity` is what pnpm/npm accept for "all the way down".
/// Map it to a very large number so we never actually stop walking.
fn parse_depth(s: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("infinity") || s == "inf" {
        return Ok(usize::MAX);
    }
    s.parse::<usize>()
        .map_err(|e| format!("invalid depth {s:?}: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ListFormat {
    Default,
    Json,
    Parseable,
}

pub async fn run(
    args: ListArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if args.global {
        return run_global(&args);
    }

    let cwd = crate::dirs::project_root()?;
    // In yarn / npm / bun monorepos the lockfile lives only at the
    // workspace root, not in the subpackage. When the caller asks for
    // `--filter` we read manifest + lockfile from the root so
    // `run_filtered` sees the real graph — otherwise `parse_lockfile`
    // returns `NotFound` from the child and we exit before ever
    // iterating the workspace.
    let read_from = if !filter.is_empty() {
        crate::dirs::find_workspace_root(&cwd).unwrap_or_else(|| cwd.clone())
    } else {
        cwd.clone()
    };

    // Read manifest (needed even for `list` — we print the project name/version
    // at the top, and the lockfile parser needs it for non-pnpm formats).
    let manifest = aube_manifest::PackageJson::from_path(&read_from.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;

    // Lockfile may be absent in a brand-new project — treat that as "nothing
    // installed yet" rather than a hard error, and print an empty tree.
    let graph = match aube_lockfile::parse_lockfile(&read_from, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!("No lockfile found. Run `aube install` to populate node_modules.");
            return Ok(());
        }
        Err(e) => {
            return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile");
        }
    };

    // Resolve format from the flag combination.
    let format = if args.json {
        ListFormat::Json
    } else if args.parseable {
        ListFormat::Parseable
    } else {
        args.format
    };

    let dep_filter = DepFilter::from_flags(args.prod, args.dev);

    // Resolve `virtualStoreDirMaxLength` once so `--long` mode prints
    // the same `.aube/<name>` filename the linker actually wrote.
    // Passing the default would mis-report long dep_paths on projects
    // that customize the cap via `.npmrc`.
    let vstore_max_len = super::resolve_virtual_store_dir_max_length_for_cwd(&cwd);
    // Resolve `virtualStoreDir` too — without this, `--long` would
    // always print `./node_modules/.aube/...` even when the user has
    // relocated the virtual store (e.g. `virtualStoreDir=node_modules/vstore`
    // or an out-of-tree absolute path), pointing at a path that
    // doesn't exist.
    let vstore_prefix = super::format_virtual_store_display_prefix(
        &super::resolve_virtual_store_dir_for_cwd(&cwd),
        &cwd,
    );

    if !filter.is_empty() {
        return run_filtered(
            &read_from,
            &manifest,
            &graph,
            &args,
            dep_filter,
            &filter,
            vstore_max_len,
            &vstore_prefix,
        );
    }

    match format {
        ListFormat::Default => render_default(
            &cwd,
            &manifest,
            &graph,
            &args,
            dep_filter,
            vstore_max_len,
            &vstore_prefix,
        )?,
        ListFormat::Json => render_json(&cwd, &manifest, &graph, &args, dep_filter)?,
        ListFormat::Parseable => render_parseable(&graph, &args, dep_filter)?,
    }

    Ok(())
}

use super::DepFilter;

#[allow(clippy::too_many_arguments)]
fn run_filtered(
    root: &std::path::Path,
    root_manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    args: &ListArgs,
    dep_filter: DepFilter,
    workspace_filter: &aube_workspace::selector::EffectiveFilter,
    vstore_max_len: usize,
    vstore_prefix: &str,
) -> miette::Result<()> {
    let workspace_pkgs = aube_workspace::find_workspace_packages(root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "aube list: --filter requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at or above {}",
            root.display()
        ));
    }
    let selected = aube_workspace::selector::select_workspace_packages(
        root,
        &workspace_pkgs,
        workspace_filter,
    )
    .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    if selected.is_empty() {
        return Err(miette!(
            "aube list: filter {workspace_filter:?} did not match any workspace package"
        ));
    }

    let format = if args.json {
        ListFormat::Json
    } else if args.parseable {
        ListFormat::Parseable
    } else {
        args.format
    };

    match format {
        ListFormat::Json => {
            let mut values = Vec::new();
            for pkg in &selected {
                let importer = super::workspace_importer_path(root, &pkg.dir)?;
                values.push(json_importer_value(
                    &pkg.dir,
                    &pkg.manifest,
                    graph,
                    args,
                    dep_filter,
                    &importer,
                ));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::Value::Array(values))
                    .into_diagnostic()
                    .wrap_err("failed to serialize list output")?
            );
        }
        ListFormat::Default => {
            for (idx, pkg) in selected.iter().enumerate() {
                if idx > 0 {
                    println!();
                }
                let importer = super::workspace_importer_path(root, &pkg.dir)?;
                render_default_for_importer(
                    &pkg.dir,
                    &pkg.manifest,
                    graph,
                    args,
                    dep_filter,
                    &importer,
                    vstore_max_len,
                    vstore_prefix,
                )?;
            }
        }
        ListFormat::Parseable => {
            for pkg in &selected {
                let importer = super::workspace_importer_path(root, &pkg.dir)?;
                render_parseable_for_importer(graph, args, dep_filter, &importer)?;
            }
        }
    }

    // Keep root_manifest intentionally referenced so future refactors do not
    // accidentally move the root lockfile parse below selection.
    let _ = root_manifest;
    Ok(())
}

/// `aube list -g` — enumerate globally-installed packages. Works entirely
/// off the global directory; no project, no lockfile, no network.
fn run_global(args: &ListArgs) -> miette::Result<()> {
    let layout = super::global::GlobalLayout::resolve()?;
    let mut packages = super::global::scan_packages(&layout.pkg_dir);
    packages.sort_by(|a, b| a.aliases.first().cmp(&b.aliases.first()));

    // For each alias, read its installed version from the install dir's
    // node_modules/<alias>/package.json. Honor the positional name filter
    // the same way the local path does — prefix match, via
    // `matches_pattern` — so `aube list -g semver` shows only that entry.
    let pattern = args.pattern.as_deref();
    let mut rows: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    for pkg in &packages {
        for alias in &pkg.aliases {
            if !matches_pattern(pattern, alias) {
                continue;
            }
            // Match what `global::link_bins` wrote: each per-package
            // install dir honors the user's `modulesDir` setting, so
            // reading back from `node_modules/` hardcoded would miss
            // everything `aube add -g` put in a custom outer dir.
            let manifest = super::project_modules_dir(&pkg.install_dir)
                .join(alias)
                .join("package.json");
            let version = std::fs::read_to_string(&manifest)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| {
                    v.get("version")
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "?".to_string());
            rows.push((alias.clone(), version, pkg.install_dir.clone()));
        }
    }

    // Resolve format the same way the local path does so `--format json`
    // / `--format parseable` work in addition to the `--json`/`--parseable`
    // shortcut flags.
    let format = if args.json {
        ListFormat::Json
    } else if args.parseable {
        ListFormat::Parseable
    } else {
        args.format
    };

    match format {
        ListFormat::Json => {
            let entries: Vec<serde_json::Value> = rows
                .iter()
                .map(|(name, version, path)| {
                    serde_json::json!({
                        "name": name,
                        "version": version,
                        "path": path.to_string_lossy(),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&entries)
                    .into_diagnostic()
                    .wrap_err("failed to serialize JSON output")?
            );
        }
        ListFormat::Parseable => {
            for (name, version, path) in &rows {
                println!("{}\t{name}\t{version}", path.display());
            }
        }
        ListFormat::Default => {
            if rows.is_empty() {
                eprintln!("(no global packages installed)");
                return Ok(());
            }
            println!("{}", layout.pkg_dir.display());
            let last_idx = rows.len().saturating_sub(1);
            for (i, (name, version, path)) in rows.iter().enumerate() {
                let connector = if i == last_idx {
                    "└── "
                } else {
                    "├── "
                };
                if args.long {
                    println!("{connector}{name} {version} ({})", path.display());
                } else {
                    println!("{connector}{name} {version}");
                }
            }
        }
    }
    Ok(())
}

/// Match the user-supplied name filter (prefix match — mirrors pnpm's
/// loose matching without pulling in a glob dependency).
fn matches_pattern(pat: Option<&str>, name: &str) -> bool {
    match pat {
        None => true,
        // starts_with covers the exact-match case; an empty pattern matches
        // everything, which is the same as passing no pattern at all.
        Some(p) => name.starts_with(p),
    }
}

/// Default tree output, pnpm-style.
fn render_default(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
    vstore_max_len: usize,
    vstore_prefix: &str,
) -> miette::Result<()> {
    render_default_for_importer(
        cwd,
        manifest,
        graph,
        args,
        filter,
        ".",
        vstore_max_len,
        vstore_prefix,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_default_for_importer(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
    importer: &str,
    vstore_max_len: usize,
    vstore_prefix: &str,
) -> miette::Result<()> {
    let project_name = manifest.name.as_deref().unwrap_or("(unnamed)");
    let project_version = manifest.version.as_deref().unwrap_or("");
    let project_path = cwd.display();
    println!("{project_name}@{project_version} {project_path}");
    println!();

    let grouped = group_roots(graph, importer, filter, args.pattern.as_deref());
    if grouped.prod.is_empty() && grouped.dev.is_empty() && grouped.optional.is_empty() {
        println!("(no dependencies)");
        return Ok(());
    }

    if !grouped.prod.is_empty() {
        println!("dependencies:");
        render_section(graph, &grouped.prod, args, vstore_max_len, vstore_prefix)?;
    }
    if !grouped.optional.is_empty() {
        if !grouped.prod.is_empty() {
            println!();
        }
        println!("optionalDependencies:");
        render_section(
            graph,
            &grouped.optional,
            args,
            vstore_max_len,
            vstore_prefix,
        )?;
    }
    if !grouped.dev.is_empty() {
        if !grouped.prod.is_empty() || !grouped.optional.is_empty() {
            println!();
        }
        println!("devDependencies:");
        render_section(graph, &grouped.dev, args, vstore_max_len, vstore_prefix)?;
    }

    Ok(())
}

struct GroupedRoots<'g> {
    prod: Vec<&'g DirectDep>,
    dev: Vec<&'g DirectDep>,
    optional: Vec<&'g DirectDep>,
}

fn group_roots<'g>(
    graph: &'g LockfileGraph,
    importer: &str,
    filter: DepFilter,
    pattern: Option<&str>,
) -> GroupedRoots<'g> {
    let mut prod = Vec::new();
    let mut dev = Vec::new();
    let mut optional = Vec::new();
    for dep in graph
        .importers
        .get(importer)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
    {
        if !filter.keeps(dep.dep_type) {
            continue;
        }
        if !matches_pattern(pattern, &dep.name) {
            continue;
        }
        match dep.dep_type {
            DepType::Production => prod.push(dep),
            DepType::Dev => dev.push(dep),
            DepType::Optional => optional.push(dep),
        }
    }
    prod.sort_by(|a, b| a.name.cmp(&b.name));
    dev.sort_by(|a, b| a.name.cmp(&b.name));
    optional.sort_by(|a, b| a.name.cmp(&b.name));
    GroupedRoots {
        prod,
        dev,
        optional,
    }
}

/// Print one section (dependencies / devDependencies / optionalDependencies)
/// as a tree, honoring `--depth` for transitive expansion.
fn render_section(
    graph: &LockfileGraph,
    roots: &[&DirectDep],
    args: &ListArgs,
    vstore_max_len: usize,
    vstore_prefix: &str,
) -> miette::Result<()> {
    let last_idx = roots.len().saturating_sub(1);
    for (i, dep) in roots.iter().enumerate() {
        let is_last = i == last_idx;
        let connector = if is_last { "└── " } else { "├── " };
        let pkg = graph.get_package(&dep.dep_path);
        let version = pkg.map(|p| p.version.as_str()).unwrap_or("?");
        let extra = if args.long {
            format!(
                "  ({vstore_prefix}{})",
                aube_lockfile::dep_path_filename::dep_path_to_filename(
                    &dep.dep_path,
                    vstore_max_len,
                )
            )
        } else {
            String::new()
        };
        println!("{connector}{} {version}{extra}", dep.name);

        if args.depth >= 1
            && let Some(pkg) = pkg
        {
            let child_prefix = if is_last { "    " } else { "│   " };
            let mut visited: BTreeSet<String> = BTreeSet::new();
            // Pre-seed with the root's own dep_path so a root→root cycle is
            // caught on the first re-encounter, not one level too late.
            visited.insert(dep.dep_path.clone());
            render_subtree(
                graph,
                pkg,
                child_prefix,
                args,
                1,
                &mut visited,
                vstore_max_len,
                vstore_prefix,
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_subtree(
    graph: &LockfileGraph,
    pkg: &LockedPackage,
    prefix: &str,
    args: &ListArgs,
    current_depth: usize,
    visited: &mut BTreeSet<String>,
    vstore_max_len: usize,
    vstore_prefix: &str,
) {
    if current_depth > args.depth {
        return;
    }
    let children: Vec<(&String, &String)> = pkg.dependencies.iter().collect();
    let last = children.len().saturating_sub(1);
    for (i, (name, version)) in children.iter().enumerate() {
        let is_last = i == last;
        let connector = if is_last { "└── " } else { "├── " };
        let dep_path = format!("{name}@{version}");
        let extra = if args.long {
            format!(
                "  ({vstore_prefix}{})",
                aube_lockfile::dep_path_filename::dep_path_to_filename(&dep_path, vstore_max_len,)
            )
        } else {
            String::new()
        };
        // Cycle guard: if we've already printed this dep_path in the current
        // walk, mark it as deduped instead of recursing forever.
        let cycle_marker = if visited.contains(&dep_path) {
            " (cycle)"
        } else {
            ""
        };
        println!("{prefix}{connector}{name} {version}{extra}{cycle_marker}");

        if cycle_marker.is_empty() {
            visited.insert(dep_path.clone());
            if let Some(child) = graph.get_package(&dep_path) {
                let nested_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
                render_subtree(
                    graph,
                    child,
                    &nested_prefix,
                    args,
                    current_depth + 1,
                    visited,
                    vstore_max_len,
                    vstore_prefix,
                );
            }
            visited.remove(&dep_path);
        }
    }
}

/// JSON output: a single array with one entry per root importer (matching
/// `pnpm list --json`). Each entry contains `name`, `version`, `path`,
/// and nested `dependencies` / `devDependencies` / `optionalDependencies`
/// maps keyed by name.
fn render_json(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
) -> miette::Result<()> {
    let output = serde_json::Value::Array(vec![json_importer_value(
        cwd, manifest, graph, args, filter, ".",
    )]);
    println!(
        "{}",
        serde_json::to_string_pretty(&output)
            .into_diagnostic()
            .wrap_err("failed to serialize list output")?
    );
    Ok(())
}

fn json_importer_value(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
    importer_path: &str,
) -> serde_json::Value {
    let grouped = group_roots(graph, importer_path, filter, args.pattern.as_deref());

    let mut importer = serde_json::Map::new();
    importer.insert(
        "name".to_string(),
        serde_json::Value::String(
            manifest
                .name
                .clone()
                .unwrap_or_else(|| "(unnamed)".to_string()),
        ),
    );
    if let Some(v) = manifest.version.clone() {
        importer.insert("version".to_string(), serde_json::Value::String(v));
    }
    importer.insert(
        "path".to_string(),
        serde_json::Value::String(cwd.display().to_string()),
    );

    if !grouped.prod.is_empty() {
        importer.insert(
            "dependencies".to_string(),
            serde_json::Value::Object(build_json_deps(graph, &grouped.prod, args)),
        );
    }
    if !grouped.dev.is_empty() {
        importer.insert(
            "devDependencies".to_string(),
            serde_json::Value::Object(build_json_deps(graph, &grouped.dev, args)),
        );
    }
    if !grouped.optional.is_empty() {
        importer.insert(
            "optionalDependencies".to_string(),
            serde_json::Value::Object(build_json_deps(graph, &grouped.optional, args)),
        );
    }

    serde_json::Value::Object(importer)
}

fn build_json_deps(
    graph: &LockfileGraph,
    roots: &[&DirectDep],
    args: &ListArgs,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for dep in roots {
        let pkg = graph.get_package(&dep.dep_path);
        let mut entry = serde_json::Map::new();
        if let Some(pkg) = pkg {
            entry.insert(
                "version".to_string(),
                serde_json::Value::String(pkg.version.clone()),
            );
        }
        if args.depth >= 1
            && let Some(pkg) = pkg
        {
            let mut visited: BTreeSet<String> = BTreeSet::new();
            visited.insert(dep.dep_path.clone());
            let children = build_json_subtree(graph, pkg, args, 1, &mut visited);
            if !children.is_empty() {
                entry.insert(
                    "dependencies".to_string(),
                    serde_json::Value::Object(children),
                );
            }
        }
        out.insert(dep.name.clone(), serde_json::Value::Object(entry));
    }
    out
}

fn build_json_subtree(
    graph: &LockfileGraph,
    pkg: &LockedPackage,
    args: &ListArgs,
    current_depth: usize,
    visited: &mut BTreeSet<String>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    if current_depth > args.depth {
        return out;
    }
    for (name, version) in &pkg.dependencies {
        let dep_path = format!("{name}@{version}");
        let mut entry = serde_json::Map::new();
        entry.insert(
            "version".to_string(),
            serde_json::Value::String(version.clone()),
        );

        if visited.contains(&dep_path) {
            entry.insert("cycle".to_string(), serde_json::Value::Bool(true));
        } else {
            visited.insert(dep_path.clone());
            if let Some(child) = graph.get_package(&dep_path) {
                let children = build_json_subtree(graph, child, args, current_depth + 1, visited);
                if !children.is_empty() {
                    entry.insert(
                        "dependencies".to_string(),
                        serde_json::Value::Object(children),
                    );
                }
            }
            visited.remove(&dep_path);
        }

        out.insert(name.clone(), serde_json::Value::Object(entry));
    }
    out
}

/// Parseable output: one line per package, tab-separated
/// `<dep_path>\t<name>\t<version>`. Matches `pnpm list --parseable` closely
/// enough for shell pipelines; depth-limited walk, no tree characters.
fn render_parseable(
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
) -> miette::Result<()> {
    render_parseable_for_importer(graph, args, filter, ".")
}

fn render_parseable_for_importer(
    graph: &LockfileGraph,
    args: &ListArgs,
    filter: DepFilter,
    importer: &str,
) -> miette::Result<()> {
    let grouped = group_roots(graph, importer, filter, args.pattern.as_deref());

    // Collect roots + transitives (respecting --depth) into a BTreeMap so
    // output is stable and deduplicated.
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();
    for dep in grouped
        .prod
        .iter()
        .chain(grouped.optional.iter())
        .chain(grouped.dev.iter())
    {
        if let Some(pkg) = graph.get_package(&dep.dep_path) {
            out.insert(
                dep.dep_path.clone(),
                (pkg.name.clone(), pkg.version.clone()),
            );
            if args.depth >= 1 {
                collect_transitive(graph, pkg, args.depth, 1, &mut out);
            }
        }
    }

    for (dep_path, (name, version)) in &out {
        println!("{dep_path}\t{name}\t{version}");
    }
    Ok(())
}

fn collect_transitive(
    graph: &LockfileGraph,
    pkg: &LockedPackage,
    max_depth: usize,
    current_depth: usize,
    out: &mut BTreeMap<String, (String, String)>,
) {
    if current_depth > max_depth {
        return;
    }
    for (name, version) in &pkg.dependencies {
        let dep_path = format!("{name}@{version}");
        if out.contains_key(&dep_path) {
            continue;
        }
        if let Some(child) = graph.get_package(&dep_path) {
            out.insert(
                dep_path.clone(),
                (child.name.clone(), child.version.clone()),
            );
            collect_transitive(graph, child, max_depth, current_depth + 1, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{LockedPackage, LockfileGraph};

    fn mk_pkg(name: &str, version: &str, deps: &[(&str, &str)]) -> LockedPackage {
        let mut dependencies = BTreeMap::new();
        for (n, v) in deps {
            dependencies.insert((*n).to_string(), (*v).to_string());
        }
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            integrity: None,
            dependencies,
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    // Regression guard for greptile feedback on PR #36: a root→root cycle
    // (a@1.0.0 → b@1.0.0 → a@1.0.0) must be tagged on the first re-encounter
    // of the root. That requires pre-seeding `visited` with the root's own
    // dep_path before descending.
    #[test]
    fn json_subtree_tags_root_cycle_on_first_reencounter() {
        let a = mk_pkg("a", "1.0.0", &[("b", "1.0.0")]);
        let b = mk_pkg("b", "1.0.0", &[("a", "1.0.0")]);
        let mut packages = BTreeMap::new();
        packages.insert("a@1.0.0".to_string(), a.clone());
        packages.insert("b@1.0.0".to_string(), b);
        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages,
            ..Default::default()
        };

        let args = ListArgs {
            pattern: None,
            dev: false,
            prod: false,
            global: false,
            depth: usize::MAX,
            format: ListFormat::Json,
            json: true,
            long: false,
            parseable: false,
        };

        let mut visited: BTreeSet<String> = BTreeSet::new();
        visited.insert("a@1.0.0".to_string());
        let children = build_json_subtree(&graph, &a, &args, 1, &mut visited);

        let b_entry = children.get("b").expect("b missing");
        let b_obj = b_entry.as_object().unwrap();
        assert!(b_obj.get("cycle").is_none(), "b should not be a cycle");
        let b_deps = b_obj
            .get("dependencies")
            .and_then(|v| v.as_object())
            .expect("b.dependencies missing");
        let a_entry = b_deps.get("a").expect("a missing under b");
        let a_obj = a_entry.as_object().unwrap();
        assert_eq!(a_obj.get("cycle"), Some(&serde_json::Value::Bool(true)));
        assert!(
            a_obj.get("dependencies").is_none(),
            "cycled node should not recurse further"
        );
    }
}
