//! Per-direct-dep packument facts the install summary printer surfaces
//! inline with the `+ name@version` listing — currently deprecation
//! status and the registry `latest` dist-tag when it differs from the
//! resolved version. The data has to be snapshotted before the resolver
//! (which owns the packument cache) is dropped at the end of resolution.

use crate::Resolver;
use aube_lockfile::LockfileGraph;
use std::collections::HashMap;

/// Subset of packument facts the install summary printer wants to
/// render next to a direct-dependency line. Returned only for direct
/// deps where at least one signal is set — the printer skips the badge
/// column when [`Resolver::direct_dep_info`]'s map has no entry.
#[derive(Debug, Clone, Default)]
pub struct DirectDepInfo {
    /// Deprecation message published for the *resolved* version of this
    /// direct dep (i.e. the packument's per-version `deprecated` field).
    /// `None` for healthy versions.
    pub deprecated: Option<String>,
    /// The registry's `dist-tags.latest` for this package, but only
    /// when it differs from the resolved version. `None` when latest
    /// matches the resolved version, when the registry omits `latest`
    /// (common on private registries), or when the dep wasn't resolved
    /// from a packument (git / file / link / remote tarball).
    pub latest: Option<String>,
}

impl Resolver {
    /// Snapshot per-direct-dep packument facts so the install summary
    /// printer can render them inline after the resolver — and its
    /// packument cache — is dropped. Keys are `DirectDep::dep_path`;
    /// importer direct deps don't carry peer-context suffixes, so the
    /// key matches the `LockfileGraph.packages` entry 1:1.
    ///
    /// Skips deps whose packument wasn't fetched (frozen-lockfile reuse,
    /// non-registry sources) and deps whose registry didn't publish a
    /// `latest` dist-tag. Returns only entries where at least one signal
    /// is set so the caller's printer can use `get(dep_path)` as the
    /// "should I render badges?" check.
    pub fn direct_dep_info(&self, graph: &LockfileGraph) -> HashMap<String, DirectDepInfo> {
        let mut out: HashMap<String, DirectDepInfo> = HashMap::new();
        for deps in graph.importers.values() {
            for dep in deps {
                let Some(pkg) = graph.packages.get(&dep.dep_path) else {
                    continue;
                };
                if pkg.local_source.is_some() {
                    continue;
                }
                let Some(packument) = self.cache.get(pkg.registry_name()) else {
                    continue;
                };
                let deprecated = packument
                    .versions
                    .get(&pkg.version)
                    .and_then(|v| v.deprecated.clone());
                let latest = packument
                    .dist_tags
                    .get("latest")
                    .filter(|l| l.as_str() != pkg.version.as_str())
                    .cloned();
                if deprecated.is_some() || latest.is_some() {
                    out.insert(dep.dep_path.clone(), DirectDepInfo { deprecated, latest });
                }
            }
        }
        out
    }
}
