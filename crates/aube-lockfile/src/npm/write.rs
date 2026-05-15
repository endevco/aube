use crate::{DepType, DirectDep, Error, LocalSource, LockedPackage, LockfileGraph};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

#[derive(Debug, Serialize)]
struct WriteNpmLockfile<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    requires: bool,
    packages: BTreeMap<String, WriteNpmPackage<'a>>,
}

// Field order mirrors npm's own package-lock.json output, so a
// parse → write round-trip diffs cleanly against what `npm install`
// would produce: `name`, `version`, `resolved`, `integrity`,
// `license`, then the dep sections, then `bin`, `engines`, platform
// fields, `funding`, then the dev/optional flags. Don't reorder — the JSON is
// serialized as a `BTreeMap`-like structure but serde preserves
// struct field order for us, which is what npm readers (and git
// diffs) expect.
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct WriteNpmPackage<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<&'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dev_dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    optional_dependencies: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    peer_dependencies: BTreeMap<&'a str, &'a str>,
    /// Paired with `peer_dependencies` above. Required for round-trip
    /// parity: the `optional: true` bit gates
    /// `hoist_auto_installed_peers` and `detect_unmet_peers` — dropping
    /// it on write-back would silently re-flag every optional peer as
    /// required on the next install. Only the `optional` key is
    /// meaningful; other fields npm may add elsewhere aren't modeled.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    peer_dependencies_meta: BTreeMap<&'a str, WriteNpmPeerDepMeta>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    bin: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    engines: BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    funding: Option<WriteNpmFunding<'a>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    link: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dev: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
    /// npm v3 collapses the "reachable via dev *and* via optional,
    /// but never via production" case into a single `devOptional`
    /// flag. Emitting both `dev: true` and `optional: true` instead
    /// would trip `npm install --omit=dev` into dropping a package
    /// that should have stayed because it's still reachable via
    /// the optional chain (or vice versa with `--omit=optional`).
    #[serde(rename = "devOptional", skip_serializing_if = "std::ops::Not::not")]
    dev_optional: bool,
}

/// npm emits `funding: {"url": "…"}` verbatim, one key, on every
/// package entry that declared funding. We only carry the URL on
/// `LockedPackage`, so this wrapper slots it back into the expected
/// shape on write.
#[derive(Debug, Serialize, Default)]
struct WriteNpmFunding<'a> {
    url: &'a str,
}

/// Serialized form of a `peerDependenciesMeta` entry. Mirrors the
/// reader's `RawNpmPeerDepMeta` so writer → reader → writer round
/// trips byte-identically for every meta variant we model today.
#[derive(Debug, Serialize, Default)]
struct WriteNpmPeerDepMeta {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

/// Serialize a [`LockfileGraph`] as a `package-lock.json` v3 file.
///
/// The graph is flat (one entry per `name@version`, peer contexts
/// collapsed to a single `(name, version)` identity) and npm wants a
/// hoist + nest layout, so we rebuild it here. Algorithm:
///
/// 1. Place each root direct dep at `node_modules/<name>` — these are
///    the "hoisted" versions.
/// 2. BFS from each placed node: for every child dep, walk up the
///    ancestor chain looking for a matching entry. If an ancestor
///    already carries the right version, the child resolves through
///    nested-resolution and needs no entry of its own. Otherwise,
///    hoist to root if the root slot is free or already matches; if
///    the root is occupied by a different version, nest directly
///    under the current node.
/// 3. Continue until the queue drains. Cycles terminate because each
///    install_path is placed at most once.
///
/// Lossy areas (documented so callers know what to expect):
///  - Peer-contextualized variants of the same `name@version` collapse
///    to one entry. npm's layout can't represent per-context peers.
///  - Registry `resolved` tarball URLs are emitted when they were
///    present in the parsed graph. Graphs synthesized without
///    `tarball_url` fall back to npm's tolerated no-`resolved` form.
///  - Non-git local source entries (`file:`, URL tarballs) aren't
///    emitted yet. Git sources emit their pinned `resolved:` URL.
///    Workspace `link:` packages are emitted as importer entries plus
///    a root `node_modules/<name>` link record.
pub fn write(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Key packages by `name@version` (ignore peer-context suffix) so
    // lookups from parent deps resolve to one canonical entry even if
    // the graph has several contextualized variants.
    let mut canonical = crate::build_canonical_map(graph);
    for pkg in graph
        .packages
        .values()
        .filter(|pkg| matches!(pkg.local_source, Some(LocalSource::Git(_))))
    {
        canonical
            .entry(super::canonical_key_from_dep_path(&pkg.dep_path))
            .or_insert(pkg);
    }

    // Compute reachability for dev/optional flags. A package is
    // `dev: true` iff it's only reachable from dev roots; `optional:
    // true` iff it's only reachable from optional roots. Production
    // wins the tie: if a package is reachable from any prod root, it
    // gets neither flag.
    let roots = graph.importers.get(".").cloned().unwrap_or_default();
    let all_roots: Vec<DirectDep> = graph
        .importers
        .values()
        .flat_map(|deps| deps.iter().cloned())
        .collect();
    let prod_reach = reachable_from(&canonical, &all_roots, DepType::Production);
    let dev_reach = reachable_from(&canonical, &all_roots, DepType::Dev);
    let opt_reach = reachable_from(&canonical, &all_roots, DepType::Optional);

    // Build a hoist/nest tree keyed by a sequence of "node_modules"
    // path segments — e.g. `["foo"]` for `node_modules/foo`,
    // `["foo", "bar"]` for `node_modules/foo/node_modules/bar`. Shared
    // with bun (which renders the same segment list as `foo/bar`).
    let root_tree_roots = non_link_roots(graph, &roots);
    let tree = super::build_hoist_tree(&canonical, &root_tree_roots);
    // For the npm writer, re-key the tree by install_path strings.
    let mut placed: BTreeMap<String, String> = tree
        .into_iter()
        .map(|(segs, key)| (super::segments_to_install_path(&segs), key))
        .collect();

    // Build the JSON structure.
    let root_key = ""; // npm's root importer install path.

    let mut packages: BTreeMap<String, WriteNpmPackage> = BTreeMap::new();

    // Root importer entry — mirrors the manifest's dep fields.
    packages.insert(
        root_key.to_string(),
        WriteNpmPackage {
            name: manifest.name.as_deref(),
            version: manifest.version.as_deref(),
            dependencies: borrow_map(&manifest.dependencies),
            dev_dependencies: borrow_map(&manifest.dev_dependencies),
            optional_dependencies: borrow_map(&manifest.optional_dependencies),
            peer_dependencies: borrow_map(&manifest.peer_dependencies),
            ..Default::default()
        },
    );

    for (importer_path, importer_roots) in graph.importers.iter().filter(|(path, _)| *path != ".") {
        let Some(workspace_pkg) = workspace_package_for_importer(graph, importer_path) else {
            continue;
        };
        let (dependencies, dev_dependencies, optional_dependencies) =
            dep_sections_from_direct_deps(importer_roots);
        packages.insert(
            importer_path.clone(),
            WriteNpmPackage {
                name: Some(workspace_pkg.name.as_str()),
                version: Some(workspace_pkg.version.as_str()),
                dependencies,
                dev_dependencies,
                optional_dependencies,
                peer_dependencies: workspace_pkg
                    .peer_dependencies
                    .iter()
                    .map(|(n, v)| (n.as_str(), v.as_str()))
                    .collect(),
                ..Default::default()
            },
        );
        packages.insert(
            format!("node_modules/{}", workspace_pkg.name),
            WriteNpmPackage {
                resolved: Some(importer_path.clone()),
                link: true,
                ..Default::default()
            },
        );

        let workspace_tree_roots = non_link_roots(graph, importer_roots);
        let workspace_tree = super::build_hoist_tree(&canonical, &workspace_tree_roots);
        // Skip subtrees whose top-level segment is already hoisted to
        // `node_modules/<name>` at the same canonical version: Node's
        // upward `node_modules` walk from `<importer>/...` resolves to
        // the root copy, so the workspace-nested entries are dead
        // weight. npm's writer omits them, and emitting them produces
        // round-trip diffs vs npm-generated lockfiles.
        let redundant_tops: BTreeSet<String> = workspace_tree
            .iter()
            .filter(|(segs, key)| {
                segs.len() == 1
                    && placed
                        .get(&format!("node_modules/{}", segs[0]))
                        .is_some_and(|root_key| root_key == *key)
            })
            .map(|(segs, _)| segs[0].clone())
            .collect();
        for (segs, canonical_key) in workspace_tree {
            if redundant_tops.contains(&segs[0]) {
                continue;
            }
            let install_path =
                format!("{importer_path}/{}", super::segments_to_install_path(&segs));
            placed.entry(install_path).or_insert(canonical_key);
        }
    }

    for (install_path, canonical_key) in &placed {
        let Some(pkg) = canonical.get(canonical_key).copied() else {
            continue;
        };
        // Re-serialize pkg.dependencies as `name → version` (strip
        // peer suffixes so npm's parser sees plain version ranges).
        // npm's format wants semver ranges here in theory, but since
        // we only have exact resolved versions, emit those — real
        // npm does the same thing for nested packages.
        //
        // Filter out deps whose canonical key isn't in the map.
        // These are typically platform-filtered optional deps or
        // ignoredOptionalDependencies — the resolver has already
        // dropped them from `canonical`, so emitting them here
        // would produce a `dependencies` entry referencing a
        // package with no matching `packages` record. `npm ci`
        // treats that as a corrupt lockfile, and `npm install`
        // would refetch the dropped package. Matches the bun and
        // yarn writers, which filter the same way.
        let optional_deps: BTreeMap<&str, &str> = pkg
            .optional_dependencies
            .iter()
            .filter(|(n, value)| canonical.contains_key(&super::child_canonical_key(n, value)))
            .map(|(n, value)| {
                // Prefer the declared range from the package's own
                // manifest (what npm itself writes) over the resolved
                // pin. Falls back to the pin for entries where the
                // source lockfile didn't carry declared ranges (e.g.
                // pnpm → npm conversion).
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| super::dep_value_as_version(n, value));
                (n.as_str(), rendered)
            })
            .collect();
        let deps: BTreeMap<&str, &str> = pkg
            .dependencies
            .iter()
            .filter(|(n, value)| {
                !pkg.optional_dependencies.contains_key(*n)
                    && canonical.contains_key(&super::child_canonical_key(n, value))
            })
            .map(|(n, value)| {
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| super::dep_value_as_version(n, value));
                (n.as_str(), rendered)
            })
            .collect();

        // npm v3 flag semantics:
        //   prod-reachable     → neither flag
        //   dev only           → `dev: true`
        //   optional only      → `optional: true`
        //   dev + optional     → `devOptional: true` (single flag)
        // Emitting both `dev` and `optional` for the both-reachable
        // case is *wrong*: `npm install --omit=dev` drops anything
        // with `dev: true` and `--omit=optional` drops anything with
        // `optional: true`, so a package reachable through both
        // chains would get removed under either omit even though the
        // other chain still needs it.
        let is_prod = prod_reach.contains(canonical_key);
        let is_dev = !is_prod && dev_reach.contains(canonical_key);
        let is_opt = !is_prod && opt_reach.contains(canonical_key);
        let dev_optional = is_dev && is_opt;
        let dev = is_dev && !dev_optional;
        let optional = is_opt && !dev_optional;

        // Aliased deps (`"h3-v2": "npm:h3@..."` in package.json)
        // round-trip as `node_modules/h3-v2` with an explicit
        // `name: "h3"`, and every registry package gets a
        // `resolved:` line — what npm itself writes. JSR packages
        // are just the degenerate case where the URL can't be
        // reconstructed from name+version alone. The URL is
        // populated on the LockedPackage by the resolver (from the
        // packument's `dist.tarball`) or carried through from a
        // prior parse of the same npm lockfile.
        let alias_name = pkg.alias_of.as_deref();
        let resolved = super::source::npm_resolved_field(pkg);

        // Round-trip `peerDependencies` so a subsequent read of the
        // rewritten lockfile still feeds the peer-context pass. Values
        // are the declared peer ranges; they never carry the peer
        // suffix the snapshot side uses, so no re-encoding is needed.
        let peer_deps: BTreeMap<&str, &str> = pkg
            .peer_dependencies
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect();
        // Paired `peerDependenciesMeta` round-trip. The `optional: true`
        // bit is what `hoist_auto_installed_peers` and
        // `detect_unmet_peers` key off to distinguish "user opted
        // out" from "peer missing and required" — dropping this
        // on write-back silently re-flags every optional peer as
        // required on the next install.
        let peer_deps_meta: BTreeMap<&str, WriteNpmPeerDepMeta> = pkg
            .peer_dependencies_meta
            .iter()
            .map(|(n, m)| {
                (
                    n.as_str(),
                    WriteNpmPeerDepMeta {
                        optional: m.optional,
                    },
                )
            })
            .collect();

        packages.insert(
            install_path.clone(),
            WriteNpmPackage {
                name: alias_name,
                version: Some(pkg.version.as_str()),
                resolved,
                integrity: pkg.integrity.as_deref(),
                license: pkg.license.as_deref(),
                dependencies: deps,
                optional_dependencies: optional_deps,
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_deps_meta,
                bin: pkg
                    .bin
                    .iter()
                    .filter(|(k, _)| !k.is_empty())
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect(),
                engines: pkg
                    .engines
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect(),
                os: pkg.os.to_vec(),
                cpu: pkg.cpu.to_vec(),
                libc: pkg.libc.to_vec(),
                funding: pkg
                    .funding_url
                    .as_deref()
                    .map(|url| WriteNpmFunding { url }),
                dev,
                optional,
                dev_optional,
                ..Default::default()
            },
        );
    }

    let doc = WriteNpmLockfile {
        name: manifest.name.as_deref(),
        version: manifest.version.as_deref(),
        lockfile_version: 3,
        requires: true,
        packages,
    };

    let mut body =
        serde_json::to_string_pretty(&doc).map_err(|e| Error::parse(path, e.to_string()))?;
    // npm writes a trailing newline; match it so diffs stay clean.
    body.push('\n');
    crate::atomic_write_lockfile(path, body.as_bytes())?;
    Ok(())
}

fn workspace_package_for_importer<'a>(
    graph: &'a LockfileGraph,
    importer_path: &str,
) -> Option<&'a LockedPackage> {
    graph.packages.values().find(|pkg| {
        matches!(
            &pkg.local_source,
            Some(LocalSource::Link(path)) if path == Path::new(importer_path)
        )
    })
}

fn non_link_roots(graph: &LockfileGraph, roots: &[DirectDep]) -> Vec<DirectDep> {
    roots
        .iter()
        .filter(|dep| {
            !graph
                .packages
                .get(&dep.dep_path)
                .is_some_and(|pkg| matches!(pkg.local_source, Some(LocalSource::Link(_))))
        })
        .cloned()
        .collect()
}

type DepSections<'a> = (
    BTreeMap<&'a str, &'a str>,
    BTreeMap<&'a str, &'a str>,
    BTreeMap<&'a str, &'a str>,
);

fn dep_sections_from_direct_deps(deps: &[DirectDep]) -> DepSections<'_> {
    let mut dependencies = BTreeMap::new();
    let mut dev_dependencies = BTreeMap::new();
    let mut optional_dependencies = BTreeMap::new();

    for dep in deps {
        let rendered = dep.specifier.as_deref().unwrap_or_else(|| {
            super::dep_value_as_version(&dep.name, super::dep_path_tail(&dep.name, &dep.dep_path))
        });
        match dep.dep_type {
            DepType::Production => {
                dependencies.insert(dep.name.as_str(), rendered);
            }
            DepType::Dev => {
                dev_dependencies.insert(dep.name.as_str(), rendered);
            }
            DepType::Optional => {
                optional_dependencies.insert(dep.name.as_str(), rendered);
            }
        }
    }

    (dependencies, dev_dependencies, optional_dependencies)
}
/// Compute the set of canonical keys (`name@version`) reachable from
/// the root importer's direct deps of a given type. Traversal follows
/// `LockedPackage.dependencies`, dropping peer suffixes so the visited
/// keys match the canonical map built at the top of [`write`].
fn reachable_from(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: &[DirectDep],
    dep_type: DepType,
) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for dep in roots {
        if dep.dep_type != dep_type {
            continue;
        }
        let key = super::canonical_key_from_dep_path(&dep.dep_path);
        if canonical.contains_key(&key) && out.insert(key.clone()) {
            queue.push_back(key);
        }
    }
    while let Some(key) = queue.pop_front() {
        let Some(pkg) = canonical.get(&key).copied() else {
            continue;
        };
        for (child_name, child_value) in &pkg.dependencies {
            let child_key = super::child_canonical_key(child_name, child_value);
            if canonical.contains_key(&child_key) && out.insert(child_key.clone()) {
                queue.push_back(child_key);
            }
        }
    }
    out
}
fn borrow_map(m: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}
