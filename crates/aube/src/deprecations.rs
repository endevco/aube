//! Shared deprecation-warning plumbing for install and `aube deprecations`.
//!
//! The resolver stashes a deprecation message on each [`ResolvedPackage`] it
//! emits; the install command accumulates those into [`DeprecationRecord`]s,
//! classifies them as direct vs. transitive via the [`LockfileGraph`]'s
//! `importers` map, and renders the result according to the user's
//! `deprecationWarnings` setting. The same renderer backs the stand-alone
//! `aube deprecations` command.
//!
//! [`ResolvedPackage`]: aube_resolver::ResolvedPackage
//! [`LockfileGraph`]: aube_lockfile::LockfileGraph

use aube_lockfile::LockfileGraph;
use aube_settings::resolved::DeprecationWarnings;
use clx::style;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DeprecationRecord {
    pub name: String,
    pub version: String,
    pub dep_path: String,
    pub message: Arc<str>,
}

/// Partition records into direct (listed by any importer) and transitive.
/// Preserves input order within each bucket.
pub fn classify<'a>(
    records: &'a [DeprecationRecord],
    graph: &LockfileGraph,
) -> (Vec<&'a DeprecationRecord>, Vec<&'a DeprecationRecord>) {
    let direct_names: BTreeSet<&str> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().map(|d| d.name.as_str()))
        .collect();
    let mut direct = Vec::new();
    let mut transitive = Vec::new();
    for r in records {
        if direct_names.contains(r.name.as_str()) {
            direct.push(r);
        } else {
            transitive.push(r);
        }
    }
    (direct, transitive)
}

/// Drop records whose `dep_path` is no longer in the finalized graph
/// (pruned by `filter_graph`'s platform/optional trim).
pub fn retain_in_graph(records: &mut Vec<DeprecationRecord>, graph: &LockfileGraph) {
    records.retain(|r| graph.packages.contains_key(&r.dep_path));
}

/// Deduplicate by `(name, version)`. The stream can emit the same canonical
/// package multiple times under different peer-context dep_paths; the user
/// only wants to see each deprecated version once.
pub fn dedupe(records: Vec<DeprecationRecord>) -> Vec<DeprecationRecord> {
    let mut seen: BTreeMap<(String, String), DeprecationRecord> = BTreeMap::new();
    for r in records {
        seen.entry((r.name.clone(), r.version.clone())).or_insert(r);
    }
    seen.into_values().collect()
}

/// Render install-time warnings according to the user's `deprecationWarnings`
/// setting. Writes to stderr. Must be called after the progress UI has been
/// finished (see `InstallProgress::finish`).
pub fn render_install_warnings(
    records: &[DeprecationRecord],
    graph: &LockfileGraph,
    mode: DeprecationWarnings,
) {
    if records.is_empty() {
        return;
    }
    let (direct, transitive) = classify(records, graph);
    match mode {
        DeprecationWarnings::None => {}
        DeprecationWarnings::Summary => write_count_line(records.len()),
        DeprecationWarnings::Direct => {
            for r in &direct {
                write_warn_line(r);
            }
            if !transitive.is_empty() {
                write_transitive_count_line(transitive.len());
            }
        }
        DeprecationWarnings::All => {
            for r in direct.iter().chain(transitive.iter()) {
                write_warn_line(r);
            }
        }
    }
}

fn write_warn_line(r: &DeprecationRecord) {
    let line = format!(
        "{} {}@{}: {}",
        style::eyellow("WARN deprecated").bold(),
        r.name,
        r.version,
        r.message
    );
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn write_transitive_count_line(count: usize) {
    let pkgs = pluralizer::pluralize("transitive package", count as isize, true);
    let msg = format!("{pkgs} have deprecation warnings. Run `aube deprecations` to see them.");
    let _ = writeln!(std::io::stderr(), "{}", style::edim(msg));
}

fn write_count_line(count: usize) {
    let pkgs = pluralizer::pluralize("package", count as isize, true);
    let msg = format!("{pkgs} have deprecation warnings. Run `aube deprecations` to see them.");
    let _ = writeln!(std::io::stderr(), "{}", style::edim(msg));
}
