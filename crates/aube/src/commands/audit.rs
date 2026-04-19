//! `aube audit` — check installed packages against the registry advisory DB.
//!
//! Walks the lockfile's resolved package set (filtered by `--prod`/`--dev`),
//! posts `{name: [versions]}` to the registry's
//! `/-/npm/v1/security/advisories/bulk` endpoint, and prints the matching
//! advisories. Mirrors `pnpm audit`'s default table layout and `--json` shape.
//!
//! Pure read: no lockfile writes, no `node_modules/` touches, no project lock.

use super::DepFilter;
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use aube_registry::config::normalize_registry_url_pub;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube audit
  Severity  Package    Vulnerable  Title
  moderate  minimatch  <3.0.5      Regular Expression Denial of Service
                                   https://github.com/advisories/GHSA-f8q6-p94x

  1 vulnerability found

  # Only fail on high and above
  $ aube audit --audit-level high

  # Skip optional deps and dev deps
  $ aube audit --prod --no-optional

  # Pipe into jq
  $ aube audit --json | jq '.advisories | length'

  # Clean
  $ aube audit
  No known vulnerabilities found
";

#[derive(Debug, Args)]
pub struct AuditArgs {
    /// Only print advisories at or above this severity.
    ///
    /// One of: `low`, `moderate`, `high`, `critical`. Default: `low`.
    #[arg(long, value_enum, default_value_t = Severity::Low)]
    pub audit_level: Severity,

    /// Only audit `devDependencies`.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,

    /// Write package.json overrides that force vulnerable packages to patched versions.
    #[arg(long)]
    pub fix: bool,

    /// Drop advisories whose ID matches one of these values.
    ///
    /// Matches against the numeric npm advisory `id`,
    /// `github_advisory_id` (`GHSA-…`), and any entry in `cves[]`
    /// (case-insensitive). Repeatable; comma-separated values are also
    /// accepted.
    #[arg(long, value_name = "ID", value_delimiter = ',')]
    pub ignore: Vec<String>,

    /// Use exit code 0 if the registry responds with an error.
    ///
    /// Useful when audit checks run in CI and the registry has a hiccup.
    #[arg(long)]
    pub ignore_registry_errors: bool,

    /// Drop advisories for which no non-vulnerable version is available
    /// in the package's packument.
    ///
    /// Same "best non-vulnerable" logic as `--fix`: an advisory is kept
    /// only when an upgrade path exists.
    #[arg(long)]
    pub ignore_unfixable: bool,

    /// Emit the report as JSON (pnpm-compatible shape) instead of a table.
    #[arg(long)]
    pub json: bool,

    /// Skip `optionalDependencies`.
    #[arg(long)]
    pub no_optional: bool,

    /// Only audit `dependencies` and `optionalDependencies`.
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    clap::ValueEnum,
    strum::Display,
    strum::EnumString,
)]
#[value(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Severity {
    Low,
    Moderate,
    High,
    Critical,
}

pub async fn run(args: AuditArgs, registry_override: Option<&str>) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;

    let manifest = aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;

    let graph = match aube_lockfile::parse_lockfile(&cwd, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => {
            return Err(miette!(
                "no lockfile found — run `aube install` before `aube audit`"
            ));
        }
        Err(e) => return Err(miette!(e)).wrap_err("failed to parse lockfile"),
    };

    let filter = DepFilter::from_flags(args.prod, args.dev);
    let closure = collect_closure(&graph, filter, args.no_optional);

    // Build the bulk request body: { name: [version, ...] } with versions
    // deduped so the registry doesn't do extra work on a diamond dep.
    let mut pkg_versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pkg in closure.values() {
        let entry = pkg_versions.entry(pkg.name.clone()).or_default();
        if !entry.contains(&pkg.version) {
            entry.push(pkg.version.clone());
        }
    }

    if pkg_versions.is_empty() {
        if args.json {
            println!("{{}}");
        } else {
            println!("No dependencies to audit.");
        }
        return Ok(());
    }

    let client = build_client(&cwd, registry_override);
    let raw = match client.fetch_advisories_bulk(&pkg_versions).await {
        Ok(v) => v,
        Err(e) => {
            if args.ignore_registry_errors {
                eprintln!("warn: advisory fetch failed: {e}");
                if args.json {
                    println!("{{}}");
                } else {
                    println!("No known vulnerabilities found");
                }
                return Ok(());
            }
            return Err(miette!("advisory fetch failed: {e}"));
        }
    };

    // `--ignore` is a cheap JSON filter. Run it first so we don't
    // bother fetching packuments for advisories the user already said
    // to drop.
    let raw = if args.ignore.is_empty() {
        raw
    } else {
        filter_ignored_ids(&raw, &args.ignore)
    };

    // `--ignore-unfixable` is expensive (one packument fetch per
    // vulnerable package) but the request set is already scoped to
    // packages with at least one advisory at or above the threshold,
    // and packuments are cached on disk.
    let raw = if args.ignore_unfixable {
        filter_unfixable(&raw, &client, args.audit_level).await?
    } else {
        raw
    };

    let rows = flatten_advisories(&raw, args.audit_level);

    if args.fix && !rows.is_empty() {
        write_fix_overrides(&cwd, &rows, &client).await?;
    }

    if args.json {
        // pnpm/npm audit --json shape is `{ "<pkg>": [ {advisory...}, ... ] }`.
        // Filter the raw response to the packages/levels we kept.
        let filtered = filter_json_by_level(&raw, args.audit_level);
        let out = serde_json::to_string_pretty(&filtered).into_diagnostic()?;
        println!("{out}");
    } else {
        render_table(&rows);
    }

    if rows.is_empty() {
        Ok(())
    } else {
        // pnpm-compat: exit 1 when any advisory matches the threshold.
        std::process::exit(1);
    }
}

/// Construct the registry client, optionally pointing the default
/// registry (used for the bulk advisory POST and non-scoped packument
/// fetches) at `registry_override`. Scoped registries from `.npmrc`
/// / `aube-workspace.yaml` still win for their own packument lookups
/// — they exist precisely because advisory tooling can't know which
/// mirror a scope is pinned to.
fn build_client(cwd: &std::path::Path, registry_override: Option<&str>) -> RegistryClient {
    let mut config = super::load_npm_config(cwd);
    if let Some(url) = registry_override {
        config.registry = normalize_registry_url_pub(url);
    }
    tracing::debug!("registry: {}", config.registry);
    for (scope, url) in &config.scoped_registries {
        tracing::debug!("scoped registry: {scope} -> {url}");
    }
    let policy = super::resolve_fetch_policy(cwd);
    RegistryClient::from_config_with_policy(config, policy)
}

/// Drop advisories whose ID matches any `ignore` value. Matches
/// against the npm numeric `id`, the `github_advisory_id`, and each
/// entry in `cves[]`. IDs are compared case-insensitively as strings
/// so users can pass either `GHSA-abcd-...` or the same in uppercase
/// / lowercase, or the CVE form. Packages whose advisories all get
/// filtered out drop from the response entirely.
fn filter_ignored_ids(v: &serde_json::Value, ignore: &[String]) -> serde_json::Value {
    use serde_json::{Map, Value};
    let Some(obj) = v.as_object() else {
        return v.clone();
    };
    let needles: BTreeSet<String> = ignore.iter().map(|s| s.to_ascii_lowercase()).collect();
    let mut out: Map<String, Value> = Map::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        let kept: Vec<Value> = arr
            .iter()
            .filter(|adv| !advisory_matches_ignore(adv, &needles))
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.clone(), Value::Array(kept));
        }
    }
    Value::Object(out)
}

fn advisory_matches_ignore(adv: &serde_json::Value, needles: &BTreeSet<String>) -> bool {
    if let Some(id) = adv.get("id") {
        let id_str = match id {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        if !id_str.is_empty() && needles.contains(&id_str.to_ascii_lowercase()) {
            return true;
        }
    }
    if let Some(ghsa) = adv.get("github_advisory_id").and_then(|v| v.as_str())
        && needles.contains(&ghsa.to_ascii_lowercase())
    {
        return true;
    }
    if let Some(cves) = adv.get("cves").and_then(|v| v.as_array()) {
        for cve in cves {
            if let Some(s) = cve.as_str()
                && needles.contains(&s.to_ascii_lowercase())
            {
                return true;
            }
        }
    }
    false
}

/// Drop advisories whose `vulnerable_versions` range cannot be
/// escaped: we ask `best_non_vulnerable` whether the packument has a
/// clean version outside the range, and when the answer is "no" the
/// advisory is considered unfixable and filtered out. Any packument
/// fetch error leaves the advisories untouched — being wrong about
/// "unfixable" is worse than a harmless overreport, matching the
/// spirit of `--ignore-registry-errors`.
async fn filter_unfixable(
    v: &serde_json::Value,
    client: &RegistryClient,
    threshold: Severity,
) -> miette::Result<serde_json::Value> {
    use serde_json::{Map, Value};
    let Some(obj) = v.as_object() else {
        return Ok(v.clone());
    };
    let cache_dir = super::packument_cache_dir();
    let mut out: Map<String, Value> = Map::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        // Skip the packument fetch if every advisory on this package
        // is below the user's severity threshold — they'd all be
        // dropped by `flatten_advisories` / `filter_json_by_level`
        // anyway.
        let has_in_threshold = arr.iter().any(|adv| {
            adv.get("severity")
                .and_then(|s| s.as_str())
                .and_then(|s| s.parse::<Severity>().ok())
                .is_some_and(|s| s >= threshold)
        });
        if !has_in_threshold {
            out.insert(name.clone(), Value::Array(arr.clone()));
            continue;
        }
        let packument = match client.fetch_packument_cached(name, &cache_dir).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "audit --ignore-unfixable: keeping advisories for {name}: packument fetch failed: {e}"
                );
                out.insert(name.clone(), Value::Array(arr.clone()));
                continue;
            }
        };
        let kept: Vec<Value> = arr
            .iter()
            .filter(|adv| {
                let Some(range) = adv.get("vulnerable_versions").and_then(|s| s.as_str()) else {
                    return true;
                };
                best_non_vulnerable(&packument, &[range.to_string()]).is_some()
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.clone(), Value::Array(kept));
        }
    }
    Ok(Value::Object(out))
}

async fn write_fix_overrides(
    cwd: &std::path::Path,
    rows: &[Row],
    client: &RegistryClient,
) -> miette::Result<()> {
    let manifest_path = cwd.join("package.json");
    let content = std::fs::read_to_string(&manifest_path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;
    let mut root: serde_json::Value = serde_json::from_str(&content)
        .into_diagnostic()
        .wrap_err("failed to parse package.json")?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| miette!("package.json root must be an object"))?;

    let cache_dir = super::packument_cache_dir();
    let mut vulnerable_ranges: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        vulnerable_ranges
            .entry(row.name.clone())
            .or_default()
            .push(row.vulnerable_versions.clone());
    }

    let mut fixes = BTreeMap::new();
    for (name, ranges) in vulnerable_ranges {
        let packument = client
            .fetch_packument_cached(&name, &cache_dir)
            .await
            .map_err(|e| miette!("failed to fetch packument for {name}: {e}"))?;
        if let Some(version) = best_non_vulnerable(&packument, &ranges) {
            fixes.insert(name.clone(), version);
        } else {
            eprintln!("warn: no patched version found for {name}");
        }
    }

    if fixes.is_empty() {
        eprintln!("No audit fixes available.");
        return Ok(());
    }

    let overrides = obj
        .entry("overrides".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let overrides = overrides
        .as_object_mut()
        .ok_or_else(|| miette!("package.json `overrides` must be an object for audit --fix"))?;
    for (name, version) in &fixes {
        overrides.insert(name.clone(), serde_json::Value::String(version.clone()));
    }

    let json = serde_json::to_string_pretty(&root).into_diagnostic()?;
    std::fs::write(&manifest_path, format!("{json}\n"))
        .into_diagnostic()
        .wrap_err("failed to write package.json")?;
    eprintln!(
        "Updated package.json overrides for {} package(s).",
        fixes.len()
    );
    Ok(())
}

fn best_non_vulnerable(packument: &Packument, vulnerable_versions: &[String]) -> Option<String> {
    let vulnerable: Vec<node_semver::Range> = vulnerable_versions
        .iter()
        .filter_map(|range| node_semver::Range::parse(range).ok())
        .collect();
    let mut best: Option<(&str, node_semver::Version)> = None;
    for ver_str in packument.versions.keys() {
        let Ok(version) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !version.pre_release.is_empty() {
            continue;
        }
        if vulnerable.iter().any(|range| version.satisfies(range)) {
            continue;
        }
        if best.as_ref().is_none_or(|(_, current)| version > *current) {
            best = Some((ver_str.as_str(), version));
        }
    }
    best.map(|(raw, _)| raw.to_string())
}

/// Reachable packages from the filtered roots, keyed by `dep_path`.
fn collect_closure(
    graph: &LockfileGraph,
    filter: DepFilter,
    no_optional: bool,
) -> BTreeMap<String, &LockedPackage> {
    let mut out: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    let roots: Vec<&DirectDep> = graph
        .root_deps()
        .iter()
        .filter(|d| filter.keeps(d.dep_type))
        .filter(|d| !(no_optional && matches!(d.dep_type, DepType::Optional)))
        .collect();

    let mut stack: Vec<String> = roots.iter().map(|d| d.dep_path.clone()).collect();
    while let Some(dep_path) = stack.pop() {
        if out.contains_key(&dep_path) {
            continue;
        }
        let Some(pkg) = graph.get_package(&dep_path) else {
            continue;
        };
        out.insert(dep_path.clone(), pkg);
        for (name, version) in &pkg.dependencies {
            stack.push(format!("{name}@{version}"));
        }
    }
    out
}

#[derive(Debug)]
struct Row {
    name: String,
    severity: Severity,
    title: String,
    vulnerable_versions: String,
    url: String,
}

fn flatten_advisories(v: &serde_json::Value, threshold: Severity) -> Vec<Row> {
    let Some(obj) = v.as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = Vec::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        for adv in arr {
            let sev_str = adv
                .get("severity")
                .and_then(|s| s.as_str())
                .unwrap_or("low");
            let Ok(sev) = sev_str.parse::<Severity>() else {
                continue;
            };
            if sev < threshold {
                continue;
            }
            rows.push(Row {
                name: name.clone(),
                severity: sev,
                title: adv
                    .get("title")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                vulnerable_versions: adv
                    .get("vulnerable_versions")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: adv
                    .get("url")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    rows.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.title.cmp(&b.title))
    });
    rows
}

fn filter_json_by_level(v: &serde_json::Value, threshold: Severity) -> serde_json::Value {
    use serde_json::{Map, Value};
    let Some(obj) = v.as_object() else {
        return Value::Object(Map::new());
    };
    let mut out: Map<String, Value> = Map::new();
    for (name, advisories) in obj {
        let Some(arr) = advisories.as_array() else {
            continue;
        };
        let kept: Vec<Value> = arr
            .iter()
            .filter(|adv| {
                adv.get("severity")
                    .and_then(|s| s.as_str())
                    .and_then(|s| s.parse::<Severity>().ok())
                    .is_some_and(|s| s >= threshold)
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.clone(), Value::Array(kept));
        }
    }
    Value::Object(out)
}

fn render_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("No known vulnerabilities found");
        return;
    }

    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(7).max(7);
    let sev_w = 8; // "critical"
    let vul_w = rows
        .iter()
        .map(|r| r.vulnerable_versions.len())
        .max()
        .unwrap_or(10)
        .max(10);

    println!(
        "{:<sev_w$}  {:<name_w$}  {:<vul_w$}  Title",
        "Severity", "Package", "Vulnerable",
    );
    for row in rows {
        println!(
            "{:<sev_w$}  {:<name_w$}  {:<vul_w$}  {}",
            row.severity, row.name, row.vulnerable_versions, row.title,
        );
        if !row.url.is_empty() {
            println!("{:<sev_w$}  {:<name_w$}  {}", "", "", row.url);
        }
    }
    println!();
    println!(
        "{} vulnerabilit{} found",
        rows.len(),
        if rows.len() == 1 { "y" } else { "ies" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packument_with_versions(versions: &[&str]) -> Packument {
        let mut packument = Packument {
            name: "demo".to_string(),
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        for version in versions {
            packument.versions.insert(
                (*version).to_string(),
                aube_registry::VersionMetadata {
                    name: "demo".to_string(),
                    version: (*version).to_string(),
                    dependencies: BTreeMap::new(),
                    dev_dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    peer_dependencies_meta: BTreeMap::new(),
                    optional_dependencies: BTreeMap::new(),
                    bundled_dependencies: None,
                    dist: None,
                    os: Vec::new(),
                    cpu: Vec::new(),
                    libc: Vec::new(),
                    has_install_script: false,
                    deprecated: None,
                },
            );
        }
        packument
    }

    #[test]
    fn audit_fix_does_not_select_prerelease_versions() {
        let packument = packument_with_versions(&["1.0.0", "1.5.0", "2.0.0-beta.1"]);

        assert_eq!(
            best_non_vulnerable(&packument, &["<1.5.0".to_string()]),
            Some("1.5.0".to_string())
        );
    }

    #[test]
    fn filter_ignored_drops_matching_ghsa_and_cve_and_numeric_id() {
        let raw = serde_json::json!({
            "pkg-a": [
                {
                    "id": 1404,
                    "severity": "high",
                    "github_advisory_id": "GHSA-xxxx-aaaa-bbbb",
                    "cves": ["CVE-2022-1111"],
                    "title": "a",
                    "vulnerable_versions": "<1.0.0",
                    "url": "https://example.test/a"
                },
                {
                    "id": 1405,
                    "severity": "low",
                    "github_advisory_id": "GHSA-yyyy-cccc-dddd",
                    "cves": [],
                    "title": "b",
                    "vulnerable_versions": "<2.0.0",
                    "url": "https://example.test/b"
                }
            ],
            "pkg-b": [{
                "id": 1406,
                "severity": "critical",
                "github_advisory_id": "GHSA-zzzz-eeee-ffff",
                "cves": [],
                "title": "c",
                "vulnerable_versions": "<3.0.0",
                "url": "https://example.test/c"
            }]
        });

        // Match by GHSA (case-insensitive) drops the high advisory on pkg-a
        // but leaves its low advisory and all of pkg-b intact.
        let out = filter_ignored_ids(&raw, &["ghsa-xxxx-aaaa-bbbb".to_string()]);
        let pkg_a = out.get("pkg-a").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pkg_a.len(), 1);
        assert_eq!(pkg_a[0].get("title").unwrap(), "b");
        assert!(out.get("pkg-b").is_some());

        // Match by CVE also drops just the high advisory.
        let out = filter_ignored_ids(&raw, &["CVE-2022-1111".to_string()]);
        let pkg_a = out.get("pkg-a").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pkg_a.len(), 1);
        assert_eq!(pkg_a[0].get("title").unwrap(), "b");

        // Match by numeric id as string works too.
        let out = filter_ignored_ids(&raw, &["1406".to_string()]);
        assert!(out.get("pkg-b").is_none());
        assert!(out.get("pkg-a").is_some());

        // Dropping every advisory on a package removes the package entry.
        let out = filter_ignored_ids(
            &raw,
            &["1404".to_string(), "GHSA-yyyy-cccc-dddd".to_string()],
        );
        assert!(out.get("pkg-a").is_none());
        assert!(out.get("pkg-b").is_some());
    }

    #[test]
    fn filter_ignored_with_empty_list_is_a_noop() {
        let raw = serde_json::json!({
            "pkg-a": [{
                "id": 1,
                "severity": "high",
                "github_advisory_id": "GHSA-a",
                "cves": [],
                "title": "t",
                "vulnerable_versions": "<1.0.0",
                "url": ""
            }]
        });
        let out = filter_ignored_ids(&raw, &[]);
        assert_eq!(out, raw);
    }
}
