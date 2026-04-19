//! Peer-dependency post-processing over an already-resolved graph.
//!
//! Two user-visible passes live here:
//!
//! * [`hoist_auto_installed_peers`] — promotes declared-but-unmet peers
//!   up to importer direct deps, matching pnpm's `auto-install-peers=true`
//!   behavior. Idempotent on graphs that already ship with those hoists
//!   (npm v7+ output, lockfile-driven installs).
//! * [`apply_peer_contexts`] — computes pnpm-style `(peer@ver)` suffixes
//!   on contextualized `dep_path`s. Drives the sibling-symlink wiring in
//!   `aube-linker` so each subtree that pins different peer versions gets
//!   its own virtual-store entry.
//!
//! [`detect_unmet_peers`] reports what the two passes above couldn't wire
//! up, so the CLI can surface warnings.
//!
//! Call order from `Resolver::resolve`: `hoist_auto_installed_peers`
//! (fresh resolves only) → `apply_peer_contexts` → `detect_unmet_peers`.

use crate::version_satisfies;
use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
use std::collections::{BTreeMap, BTreeSet};

/// A peer dependency whose declared range doesn't match the version the
/// tree actually ends up providing. Emitted as a warning by `aube install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmetPeer {
    /// dep_path of the package that declared the peer.
    pub from_dep_path: String,
    /// Human-friendly package name (pre-context) for display.
    pub from_name: String,
    /// Name of the peer being declared (e.g. `"react"`).
    pub peer_name: String,
    /// The declared peer range from the package's packument
    /// (e.g. `"^16.8.0 || ^17.0.0 || ^18.0.0"`).
    pub declared: String,
    /// What the tree actually provides, if anything. `None` means the
    /// peer is completely missing — rare in practice because the BFS
    /// auto-install path usually drags *some* version in, but it can
    /// happen for corner cases.
    pub found: Option<String>,
}

/// Scan the resolved graph and return every declared required peer whose
/// resolved version doesn't satisfy its declared range. Optional peers
/// (`peerDependenciesMeta.optional = true`) are skipped — pnpm treats
/// those as "warn suppressed" with `auto-install-peers=true`. The result
/// is purely informational; aube never fails an install on unmet peers,
/// matching pnpm.
///
/// The "found" version for each package comes from its own
/// `dependencies` map — the peer-context pass writes the resolved peer
/// tail there, so we don't have to re-walk ancestors. Any peer suffix on
/// the stored tail is stripped before the semver check so `18.2.0(foo@1)`
/// is treated as `18.2.0`.
pub fn detect_unmet_peers(graph: &LockfileGraph) -> Vec<UnmetPeer> {
    let mut unmet = Vec::new();
    for pkg in graph.packages.values() {
        for (peer_name, declared_range) in &pkg.peer_dependencies {
            let optional = pkg
                .peer_dependencies_meta
                .get(peer_name)
                .map(|m| m.optional)
                .unwrap_or(false);
            if optional {
                continue;
            }

            let found_tail = pkg.dependencies.get(peer_name);
            let found_version = found_tail.map(|t| t.split('(').next().unwrap_or(t).to_string());

            let satisfied = match &found_version {
                Some(v) => version_satisfies(v, declared_range),
                None => false,
            };
            if satisfied {
                continue;
            }

            unmet.push(UnmetPeer {
                from_dep_path: pkg.dep_path.clone(),
                from_name: pkg.name.clone(),
                peer_name: peer_name.clone(),
                declared: declared_range.clone(),
                found: found_version,
            });
        }
    }
    // Stable order for deterministic test output and readable warnings.
    unmet.sort_by(|a, b| {
        (a.from_dep_path.as_str(), a.peer_name.as_str())
            .cmp(&(b.from_dep_path.as_str(), b.peer_name.as_str()))
    });
    unmet
}

/// Promote unmet peers to importer direct deps.
///
/// Walks every resolved package's declared peer deps and hoists any
/// peer that isn't already a direct dep of the importer up to the
/// importer's `dependencies` list — what pnpm's
/// `auto-install-peers=true` produces in its v9 lockfile. If you
/// depend on a package whose `peerDependencies` declares `react` and
/// you don't list `react` yourself, pnpm (and now aube) adds it to
/// your importer's dependencies with the declared peer range as the
/// specifier, and the linker creates a top-level
/// `node_modules/react` symlink you can import from your own code.
///
/// Public so lockfile-driven installs that need to re-derive peer
/// wiring (npm/yarn/bun formats, which don't record peer contexts)
/// can run this before [`apply_peer_contexts`] to match fresh-resolve
/// behavior. Idempotent in the npm case: npm v7+ already hoists
/// auto-installed peers into root's `dependencies`, so they arrive
/// pre-`satisfied` and no additions are emitted.
///
/// Algorithm:
///   1. For each importer, collect the set of names already in its
///      direct deps. Those are "satisfied" and need no hoist.
///   2. DFS the reachable graph from the importer, visiting each package
///      and examining its `peer_dependencies` declarations. For each
///      declared peer not already satisfied by the importer, find a
///      resolved version somewhere in the graph and synthesize a
///      `DirectDep` entry. Mark it as satisfied so a second encounter
///      doesn't add a duplicate.
///   3. Stable: we walk in-order and take the first declared peer range
///      encountered per name as the specifier. Conflicting ranges across
///      the tree are not reconciled — first one wins. This matches pnpm
///      for the simple case; the complex case is deferred.
///
/// Leaves everything else about the graph untouched — no packages are
/// added or removed, only importer entries grow.
pub fn hoist_auto_installed_peers(mut graph: LockfileGraph) -> LockfileGraph {
    let importer_paths: Vec<String> = graph.importers.keys().cloned().collect();
    for importer_path in importer_paths {
        let Some(direct_deps) = graph.importers.get(&importer_path) else {
            continue;
        };
        let mut satisfied: std::collections::HashSet<String> =
            direct_deps.iter().map(|d| d.name.clone()).collect();

        let mut queue: std::collections::VecDeque<String> =
            direct_deps.iter().map(|d| d.dep_path.clone()).collect();
        let mut walked: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Additions are gathered into a separate vec so we don't mutate
        // the importer's direct-dep list while still borrowing from it.
        let mut additions: Vec<DirectDep> = Vec::new();

        while let Some(dep_path) = queue.pop_front() {
            if !walked.insert(dep_path.clone()) {
                continue;
            }
            let Some(pkg) = graph.packages.get(&dep_path) else {
                continue;
            };

            // Collect unmet peer declarations from this package.
            for (peer_name, peer_range) in &pkg.peer_dependencies {
                if satisfied.contains(peer_name) {
                    continue;
                }
                // Find any resolved version in the graph for this peer.
                // Prefer the one the package already wired via its own
                // dependencies map (the BFS auto-install result), and
                // fall back to scanning `graph.packages` for a name
                // match. If nothing matches, we quietly drop the peer —
                // that's the only path where aube stays stricter than
                // pnpm today; a future PR will emit an unmet warning.
                //
                // Fallback takes the semver-max version rather than
                // whatever `BTreeMap` iteration order surfaces first —
                // otherwise two resolved `react` entries like `18.0.0`
                // and `18.3.1` would pick the lexicographically-earlier
                // (older) one.
                let resolved_via_pkg_deps = pkg.dependencies.contains_key(peer_name);
                let resolved_version = pkg.dependencies.get(peer_name).cloned().or_else(|| {
                    // Filter to parseable semver versions *before* the
                    // max_by — returning `Equal` on parse failure makes
                    // the comparator non-transitive, so an unparseable
                    // entry sitting between two valid ones would cause
                    // `max_by` to pick an iteration-order-dependent
                    // result instead of the true maximum.
                    graph
                        .packages
                        .values()
                        .filter(|p| p.name == *peer_name)
                        .filter_map(|p| {
                            node_semver::Version::parse(&p.version)
                                .ok()
                                .map(|v| (v, p.version.clone()))
                        })
                        .max_by(|a, b| a.0.cmp(&b.0))
                        .map(|(_, s)| s)
                });
                let Some(version) = resolved_version else {
                    continue;
                };
                let canonical_version = version.split('(').next().unwrap_or(&version).to_string();
                let synth_dep_path = format!("{peer_name}@{canonical_version}");
                if !graph.packages.contains_key(&synth_dep_path) {
                    // The peer version the package wired didn't match an
                    // actual package entry — bail out for this peer
                    // rather than writing a dangling DirectDep.
                    continue;
                }
                satisfied.insert(peer_name.clone());
                // Peer reached via the fallback path isn't in
                // `pkg.dependencies`, so the normal "walk pkg's deps"
                // loop at the bottom of the while block would skip it.
                // Push it onto the queue directly so its own declared
                // peers get hoisted too.
                if !resolved_via_pkg_deps {
                    queue.push_back(synth_dep_path.clone());
                }
                additions.push(DirectDep {
                    name: peer_name.clone(),
                    dep_path: synth_dep_path,
                    // Peers auto-hoisted to the root are in the prod
                    // graph by convention — matches what pnpm writes.
                    dep_type: DepType::Production,
                    specifier: Some(peer_range.clone()),
                });
            }

            // Queue the package's own resolved deps for further walking.
            for (child_name, child_version_tail) in &pkg.dependencies {
                let canonical = child_version_tail
                    .split('(')
                    .next()
                    .unwrap_or(child_version_tail);
                queue.push_back(format!("{child_name}@{canonical}"));
            }
        }

        if !additions.is_empty() {
            tracing::debug!(
                "hoisted {} auto-installed peer(s) into importer {}",
                additions.len(),
                importer_path
            );
            if let Some(deps) = graph.importers.get_mut(&importer_path) {
                deps.extend(additions);
                deps.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }
    graph
}

/// Walk the resolved graph top-down from each importer and compute a
/// peer-dependency context for every package, producing a new graph whose
/// dep_paths carry pnpm-style `(peer@ver)` suffixes.
///
/// The goal is parity with pnpm's v9 lockfile output: the same
/// `name@version` can appear multiple times — once per distinct set of peer
/// resolutions — so different subtrees that pin incompatible peers get
/// isolated virtual-store entries and truly different sibling-symlink
/// neighborhoods.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue above auto-installed it as a transitive, which matches
///     pnpm's `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` (rewritten to point at contextualized
///     children) and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
///
/// Nested peer suffixes: pnpm writes `(react-dom@18.2.0(react@18.2.0))`
/// when a declared peer has its own resolved peers. A single top-down
/// DFS pass can't produce that form, because when a parent P records
/// a peer version in its children's scope, it only knows the canonical
/// tail — the peer's OWN suffix is computed later when the peer itself
/// gets visited. We solve this by running `apply_peer_contexts_once` in
/// a fixed-point loop: the second iteration's input has Pass 1's
/// contextualized tails in every `pkg.dependencies` map, so when a
/// descendant looks a peer up in ancestor scope it sees the full
/// nested tail and serializes it as such. Most peer chains converge in
/// 2–3 iterations; we cap at 16 as a safety belt.
///
/// Limitations (documented as follow-ups in the README):
///   - No per-peer range satisfaction — we take whatever the ancestor has,
///     even if it technically doesn't match P's declared peer range.
///
/// Knobs controlling the peer-context pass. Plumbed from four
/// pnpm-compatible settings (`dedupe-peer-dependents`, `dedupe-peers`,
/// `resolve-peers-from-workspace-root`, `peers-suffix-max-length`)
/// through the `Resolver`'s `with_*` setters.
#[derive(Debug, Clone, Copy)]
pub struct PeerContextOptions {
    /// When true, run the cross-subtree peer-variant collapse pass
    /// after every iteration of the fixed-point loop. Matches pnpm's
    /// default.
    pub dedupe_peer_dependents: bool,
    /// When true, emit suffixes as `(version)` instead of
    /// `(name@version)`. Affects both the package key, the reference
    /// tails stored in `dependencies`, and the cycle-break form of
    /// `contains_canonical_back_ref`.
    pub dedupe_peers: bool,
    /// When true, unresolved peers can be satisfied by a dep declared
    /// at the root importer (`"."`) even if no ancestor scope carries
    /// the peer. Runs between own-deps and graph-wide scan in the
    /// peer-context visitor — see `visit_peer_context` in this
    /// module for the owning implementation (intentionally crate-
    /// private; the public API here is the option flag itself).
    pub resolve_from_workspace_root: bool,
    /// Byte cap on the peer-ID suffix after which the entire suffix
    /// is hashed to `_<10-char-sha256-hex>`. pnpm's default is 1000.
    pub peers_suffix_max_length: usize,
}

impl Default for PeerContextOptions {
    fn default() -> Self {
        Self {
            dedupe_peer_dependents: true,
            dedupe_peers: false,
            resolve_from_workspace_root: true,
            peers_suffix_max_length: 1000,
        }
    }
}

/// Compute peer-context suffixes over an already-resolved graph.
///
/// Takes a *canonical* graph — one `LockedPackage` per `(name,
/// version)` with `peer_dependencies` populated — and produces a
/// *contextualized* graph whose keys and transitive references carry
/// `(peer@ver)` suffixes when packages resolve peers differently in
/// different subtrees. Drives the sibling-symlink wiring in
/// `aube-linker` for peers, so every fetch/materialize site sees a
/// per-context identity for any package whose peers disambiguate.
///
/// Public so lockfile-driven installs can run the pass over graphs
/// parsed from npm/yarn/bun lockfiles (which emit canonical form —
/// no peer suffixes — and would otherwise leave peer-dependent
/// packages without their peers as `.aube/<pkg>/node_modules/<peer>`
/// siblings). Fresh resolves call it internally from
/// `Resolver::resolve`.
pub fn apply_peer_contexts(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    const MAX_ITERATIONS: usize = 16;
    let mut current = canonical;
    let mut previous_keys: Option<std::collections::BTreeSet<String>> = None;
    let mut converged = false;
    for i in 0..MAX_ITERATIONS {
        let after_once = apply_peer_contexts_once(current, options);
        let next = if options.dedupe_peer_dependents {
            dedupe_peer_variants(after_once)
        } else {
            after_once
        };
        let next_keys: std::collections::BTreeSet<String> = next.packages.keys().cloned().collect();
        if previous_keys.as_ref() == Some(&next_keys) {
            tracing::debug!("peer-context pass converged after {i} iteration(s)");
            current = next;
            converged = true;
            break;
        }
        previous_keys = Some(next_keys);
        current = next;
    }
    if !converged {
        tracing::warn!(
            "peer-context pass hit MAX_ITERATIONS={MAX_ITERATIONS} without converging — \
             lockfile may not be byte-identical to pnpm's nested form"
        );
    }
    // `dedupe-peers=true` rewrites the parenthesized peer suffix to
    // drop the `name@` prefix. Done as a post-pass rather than inline
    // so cycle detection during the fixed-point loop keeps the full
    // `name@version` form (otherwise unrelated same-version packages
    // would false-positive as back-references).
    if options.dedupe_peers {
        dedupe_peer_suffixes(current)
    } else {
        current
    }
}

/// Cross-subtree peer-variant dedupe. When `dedupe-peer-dependents` is
/// on, packages that landed at different contextualized dep_paths but
/// resolved every declared peer to the *same* version (ignoring the
/// nested peer suffix on each peer tail) collapse into a single
/// canonical variant — chosen as the lexicographically smallest key in
/// the equivalence class. References in every surviving
/// `LockedPackage.dependencies` map and every `importers[*]` direct
/// dep get rewritten through the old→canonical map, and the
/// non-canonical entries are dropped from `packages`.
///
/// Packages whose `peer_dependencies` map is empty — i.e. the canonical
/// base already has only one variant — are skipped.
pub(crate) fn dedupe_peer_variants(graph: LockfileGraph) -> LockfileGraph {
    let canonical_base = |key: &str| -> String { key.split('(').next().unwrap_or(key).to_string() };
    // Only the peer-bearing part of the resolved peer tail is
    // comparable across subtrees — the nested suffix could differ even
    // for peer-equivalent variants on mid-iterations of the outer
    // fixed-point loop.
    let peer_base = |tail: &str| -> String { tail.split('(').next().unwrap_or(tail).to_string() };

    // Group dep_paths by their peer-free base name.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in graph.packages.keys() {
        groups
            .entry(canonical_base(key))
            .or_default()
            .push(key.clone());
    }

    let mut rewrite: BTreeMap<String, String> = BTreeMap::new();
    for (_base, mut keys) in groups {
        if keys.len() < 2 {
            continue;
        }
        // Deterministic order for canonical selection + stable hashing.
        keys.sort();
        // Union-find over equivalence classes. Two variants are
        // equivalent when each declared peer name resolves to the same
        // peer base in both (or is missing from both).
        let mut parent: Vec<usize> = (0..keys.len()).collect();
        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] == i {
                i
            } else {
                let r = find(parent, parent[i]);
                parent[i] = r;
                r
            }
        }
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                let pa = &graph.packages[&keys[i]];
                let pb = &graph.packages[&keys[j]];
                // Same canonical version is required — packages with
                // different versions but the same name would share no
                // canonical_base only if the name-without-version
                // collided, which doesn't happen (version is in the
                // base). Still, belt-and-suspenders.
                if pa.version != pb.version {
                    continue;
                }
                let peer_names: BTreeSet<&String> = pa
                    .peer_dependencies
                    .keys()
                    .chain(pb.peer_dependencies.keys())
                    .collect();
                let equivalent = peer_names.iter().all(|name| {
                    match (
                        pa.dependencies.get(name.as_str()),
                        pb.dependencies.get(name.as_str()),
                    ) {
                        (Some(va), Some(vb)) => peer_base(va) == peer_base(vb),
                        (None, None) => true,
                        _ => false,
                    }
                });
                if equivalent {
                    let ri = find(&mut parent, i);
                    let rj = find(&mut parent, j);
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }
        // Build class → canonical (smallest key) mapping. Using
        // index-based iteration here because `find` takes a mutable
        // reference into `parent`, so holding an immutable borrow
        // from `keys.iter()` at the same time would double-borrow.
        #[allow(clippy::needless_range_loop)]
        {
            let mut class_rep: BTreeMap<usize, String> = BTreeMap::new();
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                class_rep
                    .entry(root)
                    .and_modify(|cur| {
                        if keys[i] < *cur {
                            *cur = keys[i].clone();
                        }
                    })
                    .or_insert_with(|| keys[i].clone());
            }
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                let canonical = class_rep[&root].clone();
                if keys[i] != canonical {
                    rewrite.insert(keys[i].clone(), canonical);
                }
            }
        }
    }

    if rewrite.is_empty() {
        return graph;
    }

    // Rewrite package dependency tails and keep only canonicals.
    let LockfileGraph {
        importers,
        packages,
        settings,
        overrides,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
    } = graph;

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (key, mut pkg) in packages {
        if rewrite.contains_key(&key) {
            continue;
        }
        for (dep_name, dep_tail) in pkg.dependencies.iter_mut() {
            let dep_key = format!("{dep_name}@{dep_tail}");
            if let Some(canonical) = rewrite.get(&dep_key) {
                let new_tail = canonical
                    .strip_prefix(&format!("{dep_name}@"))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| canonical.clone());
                *dep_tail = new_tail;
            }
        }
        new_packages.insert(key, pkg);
    }

    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    for (importer_path, deps) in importers {
        let mut new_deps = Vec::with_capacity(deps.len());
        for mut dep in deps {
            if let Some(canonical) = rewrite.get(&dep.dep_path) {
                dep.dep_path = canonical.clone();
            }
            new_deps.push(dep);
        }
        new_importers.insert(importer_path, new_deps);
    }

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings,
        overrides,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version,
    }
}

/// Single pass of the peer-context computation. See `apply_peer_contexts`
/// for the wrapping fixed-point loop.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue auto-installed it as a transitive, matching pnpm's
///     `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
fn apply_peer_contexts_once(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    let mut out_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();

    // Root-importer scope used by `resolve-peers-from-workspace-root`.
    // Computed once from the canonical input so it reflects the
    // contextualized state of every root dep on fixed-point iterations
    // 2+ — same logic as per-importer `importer_scope` below.
    let root_scope: BTreeMap<String, String> = canonical
        .importers
        .get(".")
        .map(|deps| {
            deps.iter()
                .map(|d| {
                    let tail = d
                        .dep_path
                        .strip_prefix(&format!("{}@", d.name))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| d.dep_path.clone());
                    (d.name.clone(), tail)
                })
                .collect()
        })
        .unwrap_or_default();

    for (importer_path, direct_deps) in &canonical.importers {
        // An importer's own direct deps are in scope for its children's
        // peer resolution — this is how pnpm's "auto-install at the root"
        // path gets peer links that point at root-level packages.
        //
        // Use the *full contextualized tail* off each DirectDep rather
        // than the package's plain version. On Pass 1 of the fixed-point
        // loop the tail is canonical and equal to `p.version`; on Pass 2+
        // it's already contextualized, and passing the plain version
        // would make descendants look up keys that don't exist in the
        // (now-nested) graph.
        let importer_scope: BTreeMap<String, String> = direct_deps
            .iter()
            .map(|d| {
                let tail = d
                    .dep_path
                    .strip_prefix(&format!("{}@", d.name))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| d.dep_path.clone());
                (d.name.clone(), tail)
            })
            .collect();

        let mut new_deps = Vec::with_capacity(direct_deps.len());
        for dep in direct_deps {
            // `visiting` is the DFS stack guard for this particular descent
            // — reset per direct dep so we don't incorrectly flag a package
            // as a cycle when it's reached again from a sibling subtree.
            // The shared `out_packages` still dedupes across siblings since
            // the second visit hits the `contains_key` short-circuit below.
            //
            // Invariant (see `visit_peer_context` for the detailed handling):
            // a dep_path returned from the cycle-break branch may not yet
            // be present in `out_packages` at the moment of return, because
            // the package is still being assembled up the call stack. The
            // parent that records the returned tail will complete its own
            // insertion before the recursion unwinds, so by the time
            // anything reads the graph, every referenced dep_path exists.
            let mut visiting: std::collections::HashSet<String> = std::collections::HashSet::new();
            let new_dep_path = visit_peer_context(
                &dep.dep_path,
                &canonical,
                &importer_scope,
                &root_scope,
                &mut out_packages,
                &mut visiting,
                options,
            )
            .unwrap_or_else(|| dep.dep_path.clone());
            new_deps.push(DirectDep {
                name: dep.name.clone(),
                dep_path: new_dep_path,
                dep_type: dep.dep_type,
                specifier: dep.specifier.clone(),
            });
        }
        new_importers.insert(importer_path.clone(), new_deps);
    }

    // Any canonical package that was never reached by the DFS (orphaned
    // from every importer) is dropped — that matches the filter_deps
    // semantics and avoids emitting dead entries into the lockfile.

    LockfileGraph {
        importers: new_importers,
        packages: out_packages,
        // The post-pass is pure — settings + overrides carry through
        // from the input graph untouched.
        settings: canonical.settings,
        overrides: canonical.overrides,
        ignored_optional_dependencies: canonical.ignored_optional_dependencies,
        times: canonical.times,
        skipped_optional_dependencies: canonical.skipped_optional_dependencies,
        catalogs: canonical.catalogs,
        bun_config_version: canonical.bun_config_version,
    }
}

/// DFS helper for `apply_peer_contexts`. Returns the peer-contextualized
/// dep_path of the visited package, or `None` if the canonical package is
/// missing (shouldn't happen in practice but we degrade gracefully).
/// Does `value` contain a peer-suffix reference to `canonical` as a
/// proper name@version boundary (i.e. preceded by `(` and followed by
/// `(` / `)` / end-of-string)? Used by the peer-context pass to detect
/// when a nested tail loops back to the current package so it can
/// short-circuit the chain instead of growing the suffix forever.
/// If `s` ends with `_<10 lowercase hex>` (the marker written by
/// `hash_peer_suffix`), strip it and return the prefix. Otherwise
/// return `s` unchanged.
///
/// Safe against false positives: `s` here is always a post-split
/// `name@version` base, and semver forbids `_` inside a version, so
/// an underscore 10 chars from the end of `name@version` can only be
/// our marker.
fn strip_hashed_peer_suffix(s: &str) -> &str {
    const MARKER_LEN: usize = 11; // `_` + 10 hex chars
    if s.len() < MARKER_LEN {
        return s;
    }
    let tail = &s[s.len() - MARKER_LEN..];
    if !tail.starts_with('_') {
        return s;
    }
    if tail[1..]
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        &s[..s.len() - MARKER_LEN]
    } else {
        s
    }
}

/// Hash a peer-ID suffix with SHA-256 and return `_<10-char-hex>`.
/// Used by the peer-context pass when the raw suffix length exceeds
/// `peersSuffixMaxLength`. Matches pnpm's format so lockfile dep_path
/// keys stay portable.
pub(crate) fn hash_peer_suffix(suffix: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(suffix.as_bytes());
    let mut out = String::with_capacity(11);
    out.push('_');
    for byte in digest.iter().take(5) {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub(crate) fn contains_canonical_back_ref(value: &str, canonical: &str) -> bool {
    let bytes = value.as_bytes();
    let target = canonical.as_bytes();
    if target.is_empty() || target.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i + target.len() <= bytes.len() {
        if &bytes[i..i + target.len()] == target {
            let before = if i == 0 { b'\0' } else { bytes[i - 1] };
            let after = bytes.get(i + target.len()).copied().unwrap_or(b'\0');
            let before_ok = before == b'(';
            let after_ok = after == b'(' || after == b')' || after == b'\0';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Dedupe-peers post-pass: strip the `name@` prefix from every
/// parenthesized peer segment in every dep_path key and reference,
/// turning `react-dom@18.2.0(react@18.2.0)` into
/// `react-dom@18.2.0(18.2.0)`. Nested segments get the same treatment
/// so `a@1(b@2(c@3))` becomes `a@1(2(3))`.
///
/// Running this as a final post-pass (instead of inline during suffix
/// assembly in `visit_peer_context`) keeps cycle detection correct:
/// the detection path works against the full `name@version` form
/// throughout the fixed-point loop, and only the serialized output
/// gets the shorter form. A version-only inline approach would
/// false-positive on unrelated packages that coincidentally share a
/// version with the current package's canonical base.
///
/// Pure: no-op when `dedupe_peers` is off (caller gates the call);
/// otherwise rewrites every package key, every `LockedPackage.dep_path`
/// and `LockedPackage.dependencies` value, and every `importers[*]`
/// DirectDep `dep_path` through the same `apply_dedupe_peers_to_tail`
/// helper. Package bodies (integrity, metadata, etc.) are cloned
/// verbatim.
pub(crate) fn dedupe_peer_suffixes(graph: LockfileGraph) -> LockfileGraph {
    // Pass 1: compute the intended deduped key for each package and
    // tally how many distinct full-form keys map to it. Stripping
    // `name@` from suffix segments is lossy — two variants whose peer
    // *names* differ but whose peer *versions* coincide would collapse
    // onto the same deduped key (e.g. `consumer@1.0.0(foo@1.0.0)` and
    // `consumer@1.0.0(bar@1.0.0)` both → `consumer@1.0.0(1.0.0)`).
    // `dedupe_peer_variants` already merged the peer-equivalent
    // duplicates, so any remaining collision here represents genuinely
    // distinct variants — losing one would silently drop its
    // dependency wiring. We detect those collisions and keep both
    // sides in full form.
    let mut target_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut intended: BTreeMap<String, String> = BTreeMap::new();
    for key in graph.packages.keys() {
        let new_key = apply_dedupe_peers_to_key(key);
        *target_counts.entry(new_key.clone()).or_insert(0) += 1;
        intended.insert(key.clone(), new_key);
    }
    let rewrite: BTreeMap<String, String> = intended
        .into_iter()
        .map(|(old, new)| {
            if target_counts.get(&new).copied().unwrap_or(0) > 1 {
                tracing::warn!(
                    "dedupe-peers: collision on {new} — keeping {old} in full form to avoid \
                     dropping a distinct peer-variant"
                );
                (old.clone(), old)
            } else {
                (old, new)
            }
        })
        .collect();

    // Rewrite a `(child_name, tail)` reference by reconstructing the
    // target's full-form key, looking up its effective rewrite, and
    // stripping `child_name@` off the result to recover the tail.
    // Tails always follow their target package's rewrite decision,
    // so references stay consistent when a collision forces a target
    // back to full form.
    let rewrite_tail = |child_name: &str, tail: &str| -> String {
        let old_key = format!("{child_name}@{tail}");
        match rewrite.get(&old_key) {
            Some(new_key) => new_key
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| tail.to_string()),
            None => apply_dedupe_peers_to_tail(tail),
        }
    };

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (old_key, pkg) in graph.packages {
        let new_key = rewrite
            .get(&old_key)
            .cloned()
            .unwrap_or_else(|| old_key.clone());
        let new_dependencies: BTreeMap<String, String> = pkg
            .dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        let new_optional_dependencies: BTreeMap<String, String> = pkg
            .optional_dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        new_packages.insert(
            new_key.clone(),
            LockedPackage {
                name: pkg.name,
                version: pkg.version,
                integrity: pkg.integrity,
                dependencies: new_dependencies,
                optional_dependencies: new_optional_dependencies,
                peer_dependencies: pkg.peer_dependencies,
                peer_dependencies_meta: pkg.peer_dependencies_meta,
                dep_path: new_key,
                local_source: pkg.local_source,
                os: pkg.os,
                cpu: pkg.cpu,
                libc: pkg.libc,
                bundled_dependencies: pkg.bundled_dependencies,
                tarball_url: pkg.tarball_url,
                alias_of: pkg.alias_of,
                yarn_checksum: pkg.yarn_checksum,
                engines: pkg.engines,
                bin: pkg.bin,
                declared_dependencies: pkg.declared_dependencies,
                license: pkg.license,
                funding_url: pkg.funding_url,
            },
        );
    }

    let new_importers: BTreeMap<String, Vec<DirectDep>> = graph
        .importers
        .into_iter()
        .map(|(path, deps)| {
            let rewritten = deps
                .into_iter()
                .map(|d| {
                    let new_dep_path = rewrite
                        .get(&d.dep_path)
                        .cloned()
                        .unwrap_or_else(|| apply_dedupe_peers_to_key(&d.dep_path));
                    DirectDep {
                        name: d.name,
                        dep_path: new_dep_path,
                        dep_type: d.dep_type,
                        specifier: d.specifier,
                    }
                })
                .collect();
            (path, rewritten)
        })
        .collect();

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings: graph.settings,
        overrides: graph.overrides,
        ignored_optional_dependencies: graph.ignored_optional_dependencies,
        times: graph.times,
        skipped_optional_dependencies: graph.skipped_optional_dependencies,
        catalogs: graph.catalogs,
        bun_config_version: graph.bun_config_version,
    }
}

/// Strip `name@` from inside every parenthesized segment of a full
/// dep_path key (e.g. `react-dom@18.2.0(react@18.2.0)` →
/// `react-dom@18.2.0(18.2.0)`). The first `name@version` outside any
/// parens is preserved verbatim — that's the canonical head of the
/// dep_path and `dedupe-peers` only affects the peer suffix.
pub(crate) fn apply_dedupe_peers_to_key(key: &str) -> String {
    let mut parts = key.split('(');
    let Some(first) = parts.next() else {
        return key.to_string();
    };
    let mut out = String::with_capacity(key.len());
    out.push_str(first);
    for part in parts {
        out.push('(');
        // In a well-formed key, `part` looks like `name@version)` /
        // `name@version` / `version)` / ... We strip everything up to
        // and including the LAST `@` (scoped packages like
        // `@types/react@18.2.0` contain two `@`s; the separator is the
        // rightmost one). We only strip if that `@` comes before the
        // first `)` or `(` (i.e. the segment actually starts with
        // `name@`, not the outer parens closing with no name inside).
        if let Some(at_idx) = part.rfind('@') {
            let close_idx = part.find([')', '(']).unwrap_or(usize::MAX);
            if at_idx < close_idx {
                out.push_str(&part[at_idx + 1..]);
                continue;
            }
        }
        out.push_str(part);
    }
    out
}

/// Same as [`apply_dedupe_peers_to_key`] but for dep-tail values
/// stored in `LockedPackage.dependencies` (e.g. `18.2.0(react@18.2.0)`
/// → `18.2.0(18.2.0)`). Tails differ from keys only by lacking the
/// leading `name@` prefix — both use the same parens-based suffix
/// shape, so the algorithm is identical.
fn apply_dedupe_peers_to_tail(tail: &str) -> String {
    apply_dedupe_peers_to_key(tail)
}

fn visit_peer_context(
    input_dep_path: &str,
    graph: &LockfileGraph,
    ancestor_scope: &BTreeMap<String, String>,
    root_scope: &BTreeMap<String, String>,
    out_packages: &mut BTreeMap<String, LockedPackage>,
    visiting: &mut std::collections::HashSet<String>,
    options: &PeerContextOptions,
) -> Option<String> {
    let pkg = graph.packages.get(input_dep_path)?;

    // The input key may already carry a peer suffix (fixed-point loop
    // Pass 2+). Drop it before we build a new one — otherwise we'd
    // append the new suffix on top of the old and grow unboundedly
    // across iterations (classic mutual-peer-cycle blow-up).
    //
    // Two suffix forms can be present from a prior pass:
    //   1. `(name@version)(…)` — the normal nested peer suffix. Stripped
    //      by splitting on the first `(`.
    //   2. `_<10-char-sha256-hex>` — the hashed form produced when the
    //      normal suffix exceeded `peersSuffixMaxLength`. Must also be
    //      stripped; otherwise each pass re-hashes the already-hashed
    //      key and appends another marker (exposed by the
    //      `peer_suffix_is_hashed_when_exceeding_cap` unit test).
    let canonical_base = input_dep_path.split('(').next().unwrap_or(input_dep_path);
    let canonical_base = strip_hashed_peer_suffix(canonical_base).to_string();

    // Compute peer context: walk declared peers, resolve from ancestors
    // (nearest wins — the scope is rebuilt as we recurse) or from the
    // package's own dependency map as the auto-install fallback. Both
    // sides may produce nested tails on the second and later iterations
    // of the fixed-point loop.
    // Resolution source priority for each declared peer:
    //   1. Ancestor scope — if the ancestor's version actually
    //      satisfies the declared peer range. Different subtrees can
    //      pin different versions of the same peer name (classic
    //      `lib-a peers on react@^17`, `lib-b peers on react@^18`),
    //      and silently reusing the ancestor's version regardless of
    //      the declared range would force both libs onto the same
    //      version — exactly the behavior we want to fix here.
    //   2. The current package's own `pkg.dependencies` entry — the
    //      BFS peer-walk enqueued this peer with the declared range,
    //      so whatever got picked there is guaranteed to satisfy.
    //   3. A graph-wide scan as a last resort: any package whose name
    //      matches and whose version satisfies the declared range.
    //      This keeps nested-context callers from losing their peer
    //      resolution when neither ancestor nor own-deps has it.
    //   4. If no satisfying version exists, fall back to the nearest
    //      incompatible ancestor/root/pkg dependency. pnpm still wires
    //      that user-declared version into the peer context and then
    //      reports the semver mismatch; omitting it would produce a
    //      weaker "missing peer" warning and an unsuffixed snapshot.
    //
    // If nothing in the graph satisfies, the peer is left out of the
    // context entirely — `detect_unmet_peers` will surface it as a
    // warning after the pass.
    let mut peer_context: Vec<(String, String)> = Vec::new();
    for (peer_name, declared_range) in &pkg.peer_dependencies {
        let satisfies_declared = |v: &str| -> bool {
            // The tail may carry a nested peer suffix on fixed-point
            // iterations 2+; strip it before checking the semver.
            let canonical = v.split('(').next().unwrap_or(v);
            version_satisfies(canonical, declared_range)
        };

        let from_ancestor = ancestor_scope
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_ancestor_incompatible = ancestor_scope.get(peer_name).cloned();

        let from_pkg_deps = pkg
            .dependencies
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_pkg_deps_incompatible = pkg.dependencies.get(peer_name).cloned();

        // `resolve-peers-from-workspace-root`: fall back to the root
        // importer's direct deps before the graph-wide scan. Common in
        // monorepos where the workspace root pins shared peers (e.g.
        // `react`) that leaf packages peer on without declaring them
        // in their own subtree. Skipped when the setting is off —
        // matches pnpm's `resolve-peers-from-workspace-root=false`.
        let from_root = if options.resolve_from_workspace_root {
            root_scope
                .get(peer_name)
                .filter(|v| satisfies_declared(v))
                .cloned()
        } else {
            None
        };
        let from_root_incompatible = if options.resolve_from_workspace_root {
            root_scope.get(peer_name).cloned()
        } else {
            None
        };

        // Return the full dep_path TAIL (the part after `name@`), not
        // just `p.version`. On fixed-point iteration 2+, the input
        // graph's keys are contextualized — e.g. `react-dom` lives at
        // `react-dom@18.2.0(react@18.2.0)`. Downstream code
        // reconstructs the child lookup key with
        // `format!("{child_name}@{tail}")` and needs the tail to
        // match whatever the graph has keyed it under, otherwise the
        // lookup returns None and the peer gets silently dropped
        // from `new_dependencies`. The semver check is against the
        // package's canonical `version` field, not the tail, because
        // the tail may carry a peer suffix that isn't valid semver.
        let from_graph_scan = || {
            graph
                .packages
                .values()
                .filter(|p| p.name == *peer_name)
                .filter(|p| version_satisfies(&p.version, declared_range))
                .filter_map(|p| {
                    let tail = p
                        .dep_path
                        .strip_prefix(&format!("{}@", p.name))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| p.version.clone());
                    node_semver::Version::parse(&p.version)
                        .ok()
                        .map(|ver| (ver, tail))
                })
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, tail)| tail)
        };

        if let Some(version) = from_ancestor
            .or(from_pkg_deps)
            .or(from_root)
            .or_else(from_graph_scan)
            .or(from_ancestor_incompatible)
            .or(from_pkg_deps_incompatible)
            .or(from_root_incompatible)
        {
            peer_context.push((peer_name.clone(), version));
        }
    }
    peer_context.sort_by(|a, b| a.0.cmp(&b.0));

    // For the SUFFIX we build a cycle-broken copy: any peer value that
    // nests a reference back to the current package's canonical base
    // gets stripped to its plain version. Without this, mutual peer
    // cycles (a peers on b, b peers on a) grow the suffix one level
    // per iteration of the fixed-point loop and never converge.
    //
    // The non-cycle paths are untouched, so a regular nested chain
    // like `(react-dom@18.2.0(react@18.2.0))` still serializes fully.
    // We deliberately keep the full nested tails in `peer_context` for
    // downstream scope propagation and child lookups — suffix cycle-
    // breaking is cosmetic and should not change what packages exist
    // or which snapshot entries reference each other.
    //
    // Cycle detection is always done against the full `name@version`
    // canonical base — even when `dedupe-peers=true` is on, because
    // the version-only form is ambiguous (two unrelated packages at
    // the same version would false-positive). `dedupe-peers` is
    // applied as a post-pass over the final graph in
    // `dedupe_peer_suffixes` after cycle detection is done.
    let suffix: String = peer_context
        .iter()
        .map(|(n, v)| {
            let cycles_back = contains_canonical_back_ref(v, &canonical_base);
            let display_v = if cycles_back {
                v.split('(').next().unwrap_or(v).to_string()
            } else {
                v.clone()
            };
            format!("({n}@{display_v})")
        })
        .collect();
    // pnpm's `peersSuffixMaxLength`: when the built suffix exceeds the
    // cap, replace the entire suffix with `_<10-char-sha256-hex>` so the
    // lockfile key stays bounded. Matches pnpm's lockfile format, so
    // lockfiles shared between aube and pnpm stay comparable.
    let effective_suffix = if suffix.len() > options.peers_suffix_max_length {
        hash_peer_suffix(&suffix)
    } else {
        suffix
    };
    let contextualized = format!("{canonical_base}{effective_suffix}");

    if out_packages.contains_key(&contextualized) || visiting.contains(&contextualized) {
        return Some(contextualized);
    }
    visiting.insert(contextualized.clone());

    // Build the scope for P's children. This is ancestor_scope, overlaid
    // with P's own dependencies and its resolved peer map. Children see
    // their grandparents too — this mirrors pnpm's all-the-way-up peer
    // walk.
    //
    // We deliberately do NOT strip any existing peer-context suffix
    // off the tails we put into the scope. On the first pass the
    // values are plain (BFS output has no suffixes), so preserving
    // them is a no-op; on subsequent passes (see the fixed-point loop
    // in `apply_peer_contexts`) the input graph already carries
    // contextualized tails, and keeping them in scope is exactly how
    // nested peer suffixes propagate down to consumers — a package
    // that peers on `react-dom` and reaches it through a parent whose
    // `react-dom` entry is already `18.2.0(react@18.2.0)` will see
    // that nested tail in its own scope, and its own suffix will
    // serialize as `(react-dom@18.2.0(react@18.2.0))`. That's the
    // nested form pnpm writes.
    let mut child_scope = ancestor_scope.clone();
    for (name, version) in &pkg.dependencies {
        child_scope.insert(name.clone(), version.clone());
    }
    for (name, version) in &peer_context {
        child_scope.insert(name.clone(), version.clone());
    }

    // Recurse into each child, rewriting its dependency map entry to
    // point at the contextualized dep_path's tail. A child whose visit
    // fails (orphaned / missing) keeps its own tail.
    //
    // For declared peer names, the peer context (filled from the
    // ancestor scope) is authoritative — we override whatever the BFS
    // peer walk auto-installed. Otherwise the snapshot suffix and the
    // actual wired `dependencies[peer]` could disagree, which made the
    // sibling symlink target inconsistent with the peer-context claim.
    // When the ancestor's version doesn't satisfy the declared range,
    // `detect_unmet_peers` will flag it as a warning after the pass.
    let peer_context_versions: BTreeMap<String, String> = peer_context.iter().cloned().collect();

    let mut new_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut visited_dep_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (child_name, child_version_tail) in &pkg.dependencies {
        // If this child is a declared peer, its tail comes from the
        // peer context (which may be nested). Otherwise we use the
        // tail we already have — also possibly nested on a 2nd pass.
        let lookup_tail = match peer_context_versions.get(child_name) {
            Some(v) => v.clone(),
            None => child_version_tail.clone(),
        };
        let child_canonical_dep_path = format!("{child_name}@{lookup_tail}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        let new_tail = match child_new {
            Some(new_dep_path) => new_dep_path
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| lookup_tail.clone()),
            None => lookup_tail.clone(),
        };
        new_dependencies.insert(child_name.clone(), new_tail);
        visited_dep_names.insert(child_name.clone());
    }

    // Peers that were satisfied purely from the ancestor scope may not
    // have been in `pkg.dependencies` at all (no auto-install needed).
    // Wire them as deps now so the linker creates the sibling symlink
    // and the lockfile snapshot records them.
    for (peer_name, peer_version) in &peer_context {
        if visited_dep_names.contains(peer_name) {
            continue;
        }
        let child_canonical_dep_path = format!("{peer_name}@{peer_version}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        if let Some(new_dep_path) = child_new {
            let new_tail = new_dep_path
                .strip_prefix(&format!("{peer_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| peer_version.clone());
            new_dependencies.insert(peer_name.clone(), new_tail);
        }
    }

    visiting.remove(&contextualized);
    let new_optional_dependencies: BTreeMap<String, String> = pkg
        .optional_dependencies
        .keys()
        .filter_map(|name| {
            new_dependencies
                .get(name)
                .map(|tail| (name.clone(), tail.clone()))
        })
        .collect();

    out_packages.insert(
        contextualized.clone(),
        LockedPackage {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            integrity: pkg.integrity.clone(),
            dependencies: new_dependencies,
            optional_dependencies: new_optional_dependencies,
            peer_dependencies: pkg.peer_dependencies.clone(),
            peer_dependencies_meta: pkg.peer_dependencies_meta.clone(),
            dep_path: contextualized.clone(),
            local_source: pkg.local_source.clone(),
            os: pkg.os.clone(),
            cpu: pkg.cpu.clone(),
            libc: pkg.libc.clone(),
            bundled_dependencies: pkg.bundled_dependencies.clone(),
            tarball_url: pkg.tarball_url.clone(),
            alias_of: pkg.alias_of.clone(),
            yarn_checksum: pkg.yarn_checksum.clone(),
            engines: pkg.engines.clone(),
            bin: pkg.bin.clone(),
            declared_dependencies: pkg.declared_dependencies.clone(),
            license: pkg.license.clone(),
            funding_url: pkg.funding_url.clone(),
        },
    );
    Some(contextualized)
}
