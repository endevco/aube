//! `aube outdated` — compare installed versions against the registry.
//!
//! Reads the root importer's direct deps from the lockfile, fetches each
//! package's packument (via the disk-backed cache), and prints the ones
//! whose current resolved version lags behind the `latest` dist-tag or
//! behind the highest version that still satisfies the range in
//! `package.json`. Mirrors `pnpm outdated`'s default table layout.
//!
//! Pure read: no state changes, no `node_modules/` writes, no project lock.

use super::{DepFilter, make_client, packument_cache_dir};
use aube_lockfile::{DepType, DirectDep};
use aube_registry::Packument;
use clap::Args;
use miette::{Context, IntoDiagnostic};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube outdated
  Package     Current  Wanted   Latest
  lodash      4.17.20  4.17.21  4.17.21
  typescript  5.3.3    5.3.3    5.4.5
  zod         3.22.4   3.22.4   3.23.8

  # Also print the package.json specifier and dep type
  $ aube outdated --long
  Package     Current  Wanted   Latest
  lodash      4.17.20  4.17.21  4.17.21
  typescript  5.3.3    5.3.3    5.4.5

    lodash (dependencies): ^4.17.20
    typescript (devDependencies): ^5.3.0

  # Filter by prefix
  $ aube outdated '@babel/*'

  # Machine-readable (pnpm-compatible shape)
  $ aube outdated --json
  {
    \"lodash\": {
      \"current\": \"4.17.20\",
      \"wanted\": \"4.17.21\",
      \"latest\": \"4.17.21\"
    }
  }

  # Nothing to report exits 0
  $ aube outdated
  All dependencies up to date.
";

#[derive(Debug, Args)]
pub struct OutdatedArgs {
    /// Optional package name (prefix match) to filter the report
    pub pattern: Option<String>,

    /// Show only devDependencies
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Emit a JSON object keyed by package name instead of the default table
    #[arg(long)]
    pub json: bool,

    /// Also show deps whose `wanted` version matches the installed version
    #[arg(long)]
    pub long: bool,

    /// Show only production dependencies (skip devDependencies)
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
}

#[derive(Debug, Serialize)]
struct Row {
    // Skipped on serialize — the outer `render_json` map is keyed by
    // name, so duplicating it inside each entry would diverge from
    // pnpm's `{ "<name>": { ... } }` shape.
    #[serde(skip)]
    name: String,
    current: String,
    wanted: String,
    latest: String,
    #[serde(rename = "dependencyType", serialize_with = "serialize_dep_type")]
    dep_type: DepType,
    // Whether the packument carried a `latest` dist-tag. When false,
    // `latest` is the human-facing "(unknown)" sentinel and the drift
    // check ignores it so a missing tag doesn't flip exit code 1.
    #[serde(skip)]
    latest_known: bool,
    #[serde(skip)]
    specifier: Option<String>,
    #[serde(skip)]
    importer: Option<String>,
}

/// Serialize `DepType` using pnpm's `package.json` field names so
/// `outdated --json` is a drop-in match for `pnpm outdated --json`.
fn serialize_dep_type<S: serde::Serializer>(dt: &DepType, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match dt {
        DepType::Production => "dependencies",
        DepType::Dev => "devDependencies",
        DepType::Optional => "optionalDependencies",
    })
}

pub async fn run(
    args: OutdatedArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;
    if !filter.is_empty() {
        return run_filtered(&cwd, args, &filter).await;
    }
    run_one(&cwd, args, None).await?;
    Ok(())
}

async fn run_filtered(
    cwd: &Path,
    args: OutdatedArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let matched = super::select_workspace_packages(cwd, filter, "outdated")?;
    let manifest = aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let graph = match aube_lockfile::parse_lockfile(cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!("No lockfile found. Run `aube install` first.");
            return Ok(());
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };
    let mut any_drift = false;
    for pkg in matched {
        let importer = pkg
            .name
            .clone()
            .unwrap_or_else(|| pkg.dir.display().to_string());
        let importer_path = super::workspace_importer_path(cwd, &pkg.dir)?;
        let roots = graph
            .importers
            .get(&importer_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if run_graph(cwd, args.clone_for_fanout(), &graph, roots, Some(importer)).await? {
            any_drift = true;
        }
    }
    if any_drift {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_one(cwd: &Path, args: OutdatedArgs, importer: Option<String>) -> miette::Result<bool> {
    let manifest = aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;

    let graph = match aube_lockfile::parse_lockfile(cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            eprintln!("No lockfile found. Run `aube install` first.");
            return Ok(false);
        }
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    run_graph(cwd, args, &graph, graph.root_deps(), importer).await
}

async fn run_graph(
    cwd: &Path,
    args: OutdatedArgs,
    graph: &aube_lockfile::LockfileGraph,
    roots: &[DirectDep],
    importer: Option<String>,
) -> miette::Result<bool> {
    let filter = DepFilter::from_flags(args.prod, args.dev);
    let roots: Vec<&DirectDep> = roots
        .iter()
        .filter(|d| filter.keeps(d.dep_type))
        .filter(|d| match args.pattern.as_deref() {
            None => true,
            Some(p) => d.name.starts_with(p),
        })
        .collect();

    if roots.is_empty() {
        if !args.json {
            println!("(no matching dependencies)");
        } else {
            println!("{{}}");
        }
        return Ok(false);
    }

    let client = std::sync::Arc::new(make_client(cwd));
    let cache_dir = packument_cache_dir();

    // Fetch every packument in parallel via a JoinSet. Failures are surfaced
    // per-row so a single missing package doesn't sink the whole report.
    let mut set = tokio::task::JoinSet::new();
    for dep in &roots {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        let name = dep.name.clone();
        set.spawn(async move {
            let result = client.fetch_packument_cached(&name, &cache_dir).await;
            (name, result)
        });
    }
    let mut packuments: HashMap<String, Result<Packument, aube_registry::Error>> =
        HashMap::with_capacity(roots.len());
    while let Some(res) = set.join_next().await {
        let (name, result) = res.into_diagnostic().wrap_err("packument fetch panicked")?;
        packuments.insert(name, result);
    }

    let mut rows: Vec<Row> = Vec::new();
    for dep in &roots {
        let packument = packuments.remove(&dep.name);
        let current = match graph.get_package(&dep.dep_path) {
            Some(p) => p.version.clone(),
            None => "(missing)".to_string(),
        };
        let packument = match packument {
            Some(Ok(p)) => p,
            Some(Err(e)) => {
                eprintln!("warn: failed to fetch packument for {}: {e}", dep.name);
                continue;
            }
            None => continue,
        };
        // `latest` is optional so a registry that never publishes a
        // `latest` dist-tag (common on private registries) doesn't get
        // silently flagged as outdated. Drift detection treats an
        // unknown latest the same as "matches current".
        let latest: Option<String> = packument.dist_tags.get("latest").cloned();

        // Wanted = highest version in the packument that still satisfies the
        // manifest range. Fall back to `current` when the range is unparseable
        // (workspace:/file: specifiers, git URLs, etc.) so we don't lie.
        let wanted = dep
            .specifier
            .as_deref()
            .and_then(|spec| max_satisfying(&packument, spec))
            .unwrap_or_else(|| current.clone());

        let latest_known = latest.is_some();
        let latest_drift = latest.as_deref().is_some_and(|l| l != current);
        let wanted_drift = current != wanted;
        let changed = latest_drift || wanted_drift;
        if changed || args.long {
            rows.push(Row {
                name: dep.name.clone(),
                current,
                wanted,
                latest: latest.unwrap_or_else(|| "(unknown)".to_string()),
                dep_type: dep.dep_type,
                latest_known,
                specifier: dep.specifier.clone(),
                importer: importer.clone(),
            });
        }
    }

    rows.sort_by(|a, b| a.name.cmp(&b.name));

    // Hide "up-to-date but only because --long" rows from the non-empty check
    // so `--long` alone doesn't cause a pnpm CI pipeline to flip to exit 1.
    // A row only counts as drift when its latest is known AND differs from
    // current, or its wanted version diverges from current — a missing
    // `latest` dist-tag must never flip the exit code.
    let has_drift = rows
        .iter()
        .any(|r| (r.latest_known && r.current != r.latest) || r.current != r.wanted);

    if args.json {
        render_json(&rows)?;
    } else {
        render_table(&rows, args.long);
    }

    // Match pnpm: exit 1 when any dependency is outdated so CI patterns like
    // `aube outdated || exit 1` and bare `aube outdated && echo ok` behave
    // the same as with pnpm. `std::process::exit` is fine here because the
    // command has no resources to clean up beyond what the OS handles.
    if has_drift && importer.is_none() {
        std::process::exit(1);
    }

    Ok(has_drift)
}

impl OutdatedArgs {
    fn clone_for_fanout(&self) -> Self {
        Self {
            pattern: self.pattern.clone(),
            dev: self.dev,
            json: self.json,
            long: self.long,
            prod: self.prod,
        }
    }
}

/// Pick the highest version in the packument that satisfies `range_str`.
/// Returns the *original packument key* (not a round-tripped `Version`
/// display string) so string comparisons against `current` — which
/// comes from the lockfile's packument key — stay stable for versions
/// whose `Display` differs from their original form (e.g., leading
/// zeros in prerelease identifiers, build metadata that `Version` drops).
fn max_satisfying(packument: &Packument, range_str: &str) -> Option<String> {
    let range = node_semver::Range::parse(range_str).ok()?;
    let mut best: Option<(&str, node_semver::Version)> = None;
    for ver_str in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, b)| v > *b) {
            best = Some((ver_str.as_str(), v));
        }
    }
    best.map(|(key, _)| key.to_string())
}

fn render_table(rows: &[Row], long: bool) {
    if rows.is_empty() {
        println!("All dependencies up to date.");
        return;
    }

    // Compute column widths.
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(7).max(7);
    let cur_w = rows
        .iter()
        .map(|r| r.current.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let want_w = rows
        .iter()
        .map(|r| r.wanted.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let latest_w = rows
        .iter()
        .map(|r| r.latest.len())
        .max()
        .unwrap_or(6)
        .max(6);

    if rows.iter().any(|r| r.importer.is_some()) {
        let importer_w = rows
            .iter()
            .filter_map(|r| r.importer.as_ref())
            .map(|s| s.len())
            .max()
            .unwrap_or(8)
            .max(8);
        println!(
            "{:<importer_w$}  {:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
            "Importer", "Package", "Current", "Wanted", "Latest",
        );
        for row in rows {
            println!(
                "{:<importer_w$}  {:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
                row.importer.as_deref().unwrap_or(""),
                row.name,
                row.current,
                row.wanted,
                row.latest,
            );
        }
    } else {
        println!(
            "{:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
            "Package", "Current", "Wanted", "Latest",
        );
        for row in rows {
            println!(
                "{:<name_w$}  {:<cur_w$}  {:<want_w$}  {:<latest_w$}",
                row.name, row.current, row.wanted, row.latest,
            );
        }
    }

    if long {
        println!();
        for row in rows {
            if let Some(spec) = &row.specifier {
                let dep_label = match row.dep_type {
                    DepType::Production => "dependencies",
                    DepType::Dev => "devDependencies",
                    DepType::Optional => "optionalDependencies",
                };
                println!("  {} ({dep_label}): {spec}", row.name);
            }
        }
    }
}

fn render_json(rows: &[Row]) -> miette::Result<()> {
    // Emit a pnpm-compatible shape: `{ "<name>": { current, wanted, latest } }`.
    use serde_json::{Map, Value};
    let mut map: Map<String, Value> = Map::new();
    for row in rows {
        let v = serde_json::to_value(row).into_diagnostic()?;
        map.insert(row.name.clone(), v);
    }
    let out = serde_json::to_string_pretty(&Value::Object(map)).into_diagnostic()?;
    println!("{out}");
    Ok(())
}
