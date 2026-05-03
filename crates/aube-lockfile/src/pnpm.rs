use crate::{
    CatalogEntry, DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph,
    PeerDepMeta,
};
use aube_manifest::PackageJson;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Parse a pnpm-lock.yaml file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let raw = parse_raw_lockfile(&content)
        .map_err(|e| Error::parse_yaml_err(path, content.clone(), &e))?;

    // Parse importers (direct deps of each workspace package).
    // We track synthesized LockedPackages for local (`file:` / `link:`)
    // deps here so the main packages loop below doesn't try to process
    // them off the canonical lockfile key.
    let mut importers = BTreeMap::new();
    let mut local_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
        BTreeMap::new();
    // pnpm v9 encodes npm-aliases implicitly: the importer key is
    // the alias (`express-fork`), `specifier:` carries `npm:<real>@<range>`,
    // and `version:` is `<real>@<resolved>`. There is no `aliasOf:`
    // field — that's an aube-specific writer extension. We record
    // each alias here and synthesize an alias-keyed LockedPackage
    // after the canonical packages loop, mirroring the shape the
    // resolver-fresh path emits so the linker stays single-shape.
    // Tuple: (alias_dep_path, real_dep_path, alias_name, real_name).
    let mut alias_remaps: Vec<(String, String, String, String)> = Vec::new();

    let mut push_direct = |deps: &mut Vec<DirectDep>,
                           alias_remaps: &mut Vec<(String, String, String, String)>,
                           name: &str,
                           info: &RawDepSpec,
                           dep_type: DepType| {
        // pnpm appends a `(peer@ver)` suffix to the importer
        // `version:` of URL- and git-based direct deps when the
        // resolved snapshot carries peer context, the same way it
        // does for semver versions. `LocalSource::parse` treats the
        // whole string as the URL, so a RemoteTarballSource built
        // from the raw value fetches `…/tar.gz/SHA(peer@ver)` and
        // 404s. Strip it here so the URL that reaches the fetcher
        // and the dep_path hash are both peer-context-free —
        // consistent with what `parse_dep_path` does for snapshot
        // keys downstream.
        let classify_version = info.version.split('(').next().unwrap_or(&info.version);
        if let Some(local) = LocalSource::parse(classify_version, Path::new("")) {
            // `Path::new("")` means tarball-vs-dir classification is
            // skipped; we default to Directory and rely on the
            // resolver's on-disk re-read for the authoritative source
            // type during a subsequent `aube install` (lockfile-only
            // path never materializes local deps anyway before the
            // fetch step re-classifies).
            //
            // Re-classify Directory → Tarball if the path looks
            // like a tarball filename, so `.tgz`/`.tar.gz`
            // targets round-trip correctly even when the file
            // isn't present at parse time. The filename
            // heuristic lives on `LocalSource` so this stays in
            // lockstep with `LocalSource::parse`.
            let local = match local {
                LocalSource::Directory(p) if LocalSource::path_looks_like_tarball(&p) => {
                    LocalSource::Tarball(p)
                }
                // Importer `version:` for git deps is the canonical
                // `<url>#<commit>` form pnpm writes. The parser
                // puts the `<commit>` into `committish`; since
                // this is a lockfile round-trip (not a raw user
                // spec), treat it as the pinned commit.
                LocalSource::Git(mut g) if g.resolved.is_empty() => {
                    if let Some(c) = g.committish.take() {
                        g.resolved = c;
                    }
                    LocalSource::Git(g)
                }
                other => other,
            };
            let dep_path = local.dep_path(name);
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
            local_packages
                .entry(dep_path.clone())
                .or_insert_with(|| LockedPackage {
                    name: name.to_string(),
                    version: "0.0.0".to_string(),
                    integrity: None,
                    dependencies: BTreeMap::new(),
                    peer_dependencies: BTreeMap::new(),
                    peer_dependencies_meta: BTreeMap::new(),
                    dep_path,
                    local_source: Some(local),
                    ..Default::default()
                });
        } else {
            // Detect npm-aliased deps purely from the shape of
            // `version:`. pnpm encodes aliases as
            // `<real_name>@<resolved>(peers…)` regardless of how the
            // alias was declared:
            //   - direct:  `specifier: npm:beamcoder-prebuild@0.7.1`
            //   - catalog: `specifier: 'catalog:'` (the alias lives
            //              in `pnpm-workspace.yaml#catalog`)
            // The earlier `specifier.starts_with("npm:")` gate missed
            // the catalog flavor and silently dropped those deps.
            // Strip any peer suffix before parsing so `version:
            // 18.2.0(react@18.2.0)` (a regular dep with peers) does
            // not parse as `name="18.2.0(react"`.
            let bare_version = info
                .version
                .split('(')
                .next()
                .unwrap_or(info.version.as_str());
            let dep_path = if let Some((real_name, resolved)) = parse_dep_path(bare_version)
                && real_name != name
            {
                let peer_suffix = info
                    .version
                    .find('(')
                    .map(|i| &info.version[i..])
                    .unwrap_or("");
                let alias_dep_path = format!("{name}@{resolved}{peer_suffix}");
                let real_dep_path = info.version.clone();
                alias_remaps.push((
                    alias_dep_path.clone(),
                    real_dep_path,
                    name.to_string(),
                    real_name,
                ));
                alias_dep_path
            } else {
                version_to_dep_path(name, &info.version)
            };
            deps.push(DirectDep {
                name: name.to_string(),
                dep_path,
                dep_type,
                specifier: Some(info.specifier.clone()),
            });
        }
    };

    for (importer_path, importer) in &raw.importers {
        // pnpm writes the workspace root as either `'.'` (most
        // common / current) or `''` (seen on v9 lockfiles in the
        // wild, e.g. npmx.dev). Both mean "the repo root" — we key
        // the graph on `.` everywhere downstream (linker, filters,
        // stats), so normalize at parse time and keep the rest of
        // the pipeline single-shape.
        let importer_path = if importer_path.is_empty() {
            "."
        } else {
            importer_path.as_str()
        };

        // Guard against a malformed lockfile that writes both `''`
        // and `'.'` for root — `BTreeMap` iteration visits `''`
        // first, so the real `'.'` entry would otherwise silently
        // overwrite the normalized empty-key entry. pnpm never
        // emits this, but skipping the second visit is cheap and
        // makes the intent explicit.
        if importers.contains_key(importer_path) {
            continue;
        }

        let mut deps = Vec::new();

        if let Some(ref d) = importer.dependencies {
            for (name, info) in d {
                push_direct(
                    &mut deps,
                    &mut alias_remaps,
                    name,
                    info,
                    DepType::Production,
                );
            }
        }
        if let Some(ref d) = importer.dev_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, &mut alias_remaps, name, info, DepType::Dev);
            }
        }
        if let Some(ref d) = importer.optional_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, &mut alias_remaps, name, info, DepType::Optional);
            }
        }

        if let Some(ref d) = importer.skipped_optional_dependencies
            && !d.is_empty()
        {
            let mut map = BTreeMap::new();
            for (name, info) in d {
                map.insert(name.clone(), info.specifier.clone());
            }
            skipped_optional_dependencies.insert(importer_path.to_string(), map);
        }

        importers.insert(importer_path.to_string(), deps);
    }

    // pnpm v9 splits packages (canonical, keyed by `name@version`) from
    // snapshots (contextualized, keyed by the full dep_path with any
    // `(peer@ver)` suffix). The LockfileGraph needs one entry per snapshot
    // — the same canonical package can produce multiple snapshots when
    // different parts of the tree resolve its peers differently.
    //
    // If `snapshots:` is missing (older aube lockfiles where we wrote
    // everything into packages), fall back to iterating packages directly.
    let mut packages = BTreeMap::new();

    // Harvest snapshot dependencies for any local (`file:`) package
    // that showed up in the importers loop. The canonical snapshot
    // key for a local dep is `<name>@<specifier>` — e.g.
    // `foo@file:./vendor/foo` — so we construct it from each
    // synthesized entry and pull its `dependencies` block out of the
    // raw snapshots map.
    for local_pkg in local_packages.values_mut() {
        if let Some(ref local) = local_pkg.local_source {
            let canonical = format!("{}@{}", local_pkg.name, local.specifier());
            // URL-based direct deps have their peer-context suffix
            // stripped (see `push_direct`), but the matching snapshot
            // entry pnpm wrote still carries the suffix. Fall back to
            // any snapshot whose peer-stripped canonical matches so
            // transitive dependency metadata still flows through.
            let snap = raw.snapshots.get(&canonical).or_else(|| {
                raw.snapshots.iter().find_map(|(k, v)| {
                    parse_dep_path(k)
                        .filter(|(n, ver)| format!("{n}@{ver}") == canonical)
                        .map(|_| v)
                })
            });
            if let Some(snap) = snap
                && let Some(mut deps) = snap.dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut deps, &mut alias_remaps);
                local_pkg.dependencies = deps;
            }
            if let Some(snap) = snap
                && let Some(mut opt_deps) = snap.optional_dependencies.clone()
            {
                rewrite_snapshot_alias_deps(&mut opt_deps, &mut alias_remaps);
                local_pkg.dependencies.extend(opt_deps.clone());
                local_pkg.optional_dependencies = opt_deps;
            }
            // Prefer the authoritative LocalSource classification
            // from the `resolution:` block over the guess the
            // importers loop made from the bare specifier. For git
            // deps, preserve any `path:` selector already captured
            // from the importer's `version:` URL — pnpm v9 encodes
            // the subpath in the snapshot key and doesn't always
            // echo it on the resolution block.
            if let Some(pkg_info) = raw.packages.get(&canonical)
                && let Some(ref res) = pkg_info.resolution
                && let Some(mut ls) = local_source_from_resolution(res)
            {
                if let LocalSource::Git(ref mut g) = ls
                    && g.subpath.is_none()
                    && let Some(LocalSource::Git(prior)) = &local_pkg.local_source
                {
                    g.subpath = prior.subpath.clone();
                }
                local_pkg.local_source = Some(ls);
            }
        }
    }
    // Rebuild keys in case the local_source rewrite above changed
    // the classification — kind alone doesn't affect the encoded
    // dep_path (the hash is over the path string only), but the
    // `resolution:` block can also hand us a *different path* than
    // the importer's specifier, which does. Recompute both the map
    // key and the struct field from the final `local_source` so
    // `graph.packages.get(&dep.dep_path)` stays consistent with how
    // DirectDeps were keyed up in the importer loop above. Note
    // that any reclassification with a *new path* would leave the
    // DirectDep still pointing at the old key; pnpm's lockfiles
    // don't do that today, so we treat the re-keying as
    // defensive-only and assert equality in debug builds.
    let mut rekeyed: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (old_key, mut pkg) in local_packages {
        let new_key = pkg.local_source.as_ref().unwrap().dep_path(&pkg.name);
        pkg.dep_path = new_key.clone();
        debug_assert_eq!(
            old_key, new_key,
            "local dep_path shifted during reclassification — DirectDeps still reference {old_key}"
        );
        rekeyed.insert(new_key, pkg);
    }
    let local_packages = rekeyed;
    // Canonical keys the main loop should ignore — those are the
    // snapshot keys we already absorbed above.
    let local_canonical_keys: std::collections::HashSet<String> = local_packages
        .values()
        .filter_map(|p| {
            p.local_source
                .as_ref()
                .map(|l| format!("{}@{}", p.name, l.specifier()))
        })
        .collect();

    let snapshot_keys: Vec<String> = if raw.snapshots.is_empty() {
        raw.packages.keys().cloned().collect()
    } else {
        raw.snapshots.keys().cloned().collect()
    };

    for dep_path in snapshot_keys {
        if local_canonical_keys.contains(&dep_path) {
            continue;
        }
        let (name, version) = parse_dep_path(&dep_path)
            .ok_or_else(|| Error::parse(path, format!("invalid dep path: {dep_path}")))?;
        // URL-based direct deps are absorbed into `local_packages`
        // under the peer-stripped URL form (see `push_direct`), but the
        // snapshot key still carries any `(peer@ver)` suffix pnpm
        // appended. Check the peer-stripped canonical too so we don't
        // create a duplicate entry that round-trips as a stray
        // `packages:` block.
        if local_canonical_keys.contains(&format!("{name}@{version}")) {
            continue;
        }

        // Look up the canonical package entry by stripping any peer suffix.
        let canonical_key = version_to_dep_path(&name, &version);
        let pkg_info = raw
            .packages
            .get(&canonical_key)
            .or_else(|| raw.packages.get(&dep_path));

        let integrity = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.integrity.clone());

        // Registry packages record a `tarball:` URL only when
        // `lockfileIncludeTarballUrl=true` was active at write time.
        // Preserve it on read so the round-trip writes the same URL
        // back without having to reconsult the registry client.
        //
        // pnpm also writes a `tarball:` entry for non-registry transitive
        // deps whose key is a URL (remote tarball from a github override,
        // pkg.pr.new, etc.) — capture those on the same field so the
        // install path can fetch them verbatim instead of deriving a
        // registry URL that would 404.
        let tarball_url = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.tarball.as_ref())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
            .cloned();

        // pnpm writes `version: <semver>` alongside non-registry entries
        // whose dep-path key is a URL. Prefer that over the URL itself
        // when the dep-path version isn't a real semver — the install
        // path uses `pkg.version` for the store-content cross-check,
        // and comparing a URL to the tarball's declared `2.4.1` would
        // fail every github override'd package.
        //
        // Gated on `tarball_url.is_some()` so the swap only applies to
        // the remote-tarball case (where the URL is recoverable from
        // `resolution.tarball` at write time). `git+`/`git://` /
        // `.git#sha` transitive entries resolve through
        // `resolution: {type: git, commit, repo}` and need a separate
        // round-trip path — they stay on the pre-existing URL-as-
        // version behavior until that path lands.
        let version_is_http_url = version.starts_with("http://") || version.starts_with("https://");
        let version = if version_is_http_url && tarball_url.is_some() {
            pkg_info.and_then(|p| p.version.clone()).unwrap_or(version)
        } else {
            version
        };

        let snapshot = raw.snapshots.get(&dep_path);
        let mut optional_dependencies = snapshot
            .and_then(|s| s.optional_dependencies.clone())
            .unwrap_or_default();
        let mut dependencies = snapshot
            .and_then(|s| s.dependencies.clone())
            .unwrap_or_default();
        rewrite_snapshot_alias_deps(&mut dependencies, &mut alias_remaps);
        rewrite_snapshot_alias_deps(&mut optional_dependencies, &mut alias_remaps);
        dependencies.extend(optional_dependencies.clone());
        let bundled_dependencies = snapshot
            .and_then(|s| s.bundled_dependencies.clone())
            .unwrap_or_default();
        let optional = snapshot.and_then(|s| s.optional).unwrap_or(false);
        let transitive_peer_dependencies = snapshot
            .and_then(|s| s.transitive_peer_dependencies.clone())
            .unwrap_or_default();

        let peer_dependencies = pkg_info
            .and_then(|p| p.peer_dependencies.clone())
            .unwrap_or_default();
        let peer_dependencies_meta = pkg_info
            .and_then(|p| p.peer_dependencies_meta.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                (
                    k,
                    PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();
        let os = pkg_info.map(|p| p.os.clone()).unwrap_or_default();
        let cpu = pkg_info.map(|p| p.cpu.clone()).unwrap_or_default();
        let libc = pkg_info.map(|p| p.libc.clone()).unwrap_or_default();
        let engines = pkg_info.map(|p| p.engines.clone()).unwrap_or_default();
        // pnpm's lockfile only stores `hasBin: true/false` (no paths);
        // reconstruct an opaque single-entry map on parse so
        // `!bin.is_empty()` stays equivalent to `hasBin`, then let
        // downstream writers fill in real paths when they have them.
        // The map key + value are placeholders — writers that care
        // about bin names (bun) read from richer sources.
        let bin = if pkg_info.map(|p| p.has_bin).unwrap_or(false) {
            let mut m = BTreeMap::new();
            m.insert(String::new(), String::new());
            m
        } else {
            BTreeMap::new()
        };
        // Aube-specific extension (see `WritablePackageInfo::alias_of`)
        // — ordinary pnpm lockfiles never carry it, so this stays
        // `None` on pnpm-authored input and round-trips the resolver-
        // emitted value for aliased packages.
        let alias_of = pkg_info.and_then(|p| p.alias_of.clone());

        // Reclassify transitive URL-keyed entries — github forks,
        // pkg.pr.new, `file:` targets — so they round-trip with the
        // right `local_source`. Without this, the install path sees
        // `local_source: None` + a URL-form version and tries to
        // fetch the dep from the npm registry (404).
        let local_source = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(local_source_from_resolution);
        // `lockfileIncludeTarballUrl` puts registry tarball URLs on
        // ordinary `name@version` entries; only URL-keyed entries are
        // true remote-tarball deps.
        let local_source = match local_source {
            Some(LocalSource::RemoteTarball(_)) if !version_is_http_url => None,
            other => other,
        };

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity,
                dependencies,
                optional_dependencies,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                os: os.into(),
                cpu: cpu.into(),
                libc: libc.into(),
                bundled_dependencies,
                optional,
                transitive_peer_dependencies,
                tarball_url,
                alias_of,
                yarn_checksum: None,
                engines,
                bin,
                // pnpm's `snapshots:` only records resolved pins, so
                // the parser has no declared ranges to restore. Left
                // empty; npm / yarn / bun writers fall back to pins
                // when re-emitting a pnpm-sourced graph into one of
                // their formats.
                declared_dependencies: BTreeMap::new(),
                // pnpm's format doesn't carry per-package license or
                // funding metadata, so a pnpm → npm conversion
                // degrades to empty rather than re-fetching each
                // packument. npm writers skip these fields when
                // `None`.
                license: None,
                funding_url: None,
                extra_meta: BTreeMap::new(),
            },
        );
    }

    // Synthesize alias-keyed LockedPackages for npm-aliased importer
    // deps. pnpm v9 only writes the canonical (real-name-keyed) entry
    // in `packages:`; we clone it under the alias dep_path with
    // `name=alias` and `alias_of=Some(real)` so the linker — which
    // already supports this shape via the resolver-fresh path — can
    // create `node_modules/<alias>` symlinks correctly.
    for (alias_dep_path, real_dep_path, alias_name, real_name) in alias_remaps {
        // Skip if the alias entry already exists (aube-written
        // lockfile that emitted both `aliasOf:` and an alias-keyed
        // packages entry).
        if packages.contains_key(&alias_dep_path) {
            continue;
        }
        let Some(real_pkg) = packages
            .get(&real_dep_path)
            .or_else(|| peerless_alias_target(&packages, &real_dep_path))
        else {
            return Err(Error::parse(
                path,
                format!(
                    "npm-alias references missing package {real_dep_path} (alias dep_path: {alias_dep_path})"
                ),
            ));
        };
        let mut aliased = real_pkg.clone();
        aliased.name = alias_name;
        aliased.dep_path = alias_dep_path.clone();
        aliased.alias_of = Some(real_name);
        packages.insert(alias_dep_path, aliased);
    }

    for (k, v) in local_packages {
        packages.insert(k, v);
    }

    let settings = raw
        .settings
        .map(|s| crate::LockfileSettings {
            auto_install_peers: s.auto_install_peers.unwrap_or(true),
            exclude_links_from_lockfile: s.exclude_links_from_lockfile.unwrap_or(false),
            lockfile_include_tarball_url: s.lockfile_include_tarball_url.unwrap_or(false),
        })
        .unwrap_or_default();

    let times = raw.time.unwrap_or_default();

    let catalogs = raw
        .catalogs
        .unwrap_or_default()
        .into_iter()
        .map(|(name, entries)| {
            let inner = entries
                .into_iter()
                .map(|(pkg, e)| {
                    (
                        pkg,
                        CatalogEntry {
                            specifier: e.specifier,
                            version: e.version,
                        },
                    )
                })
                .collect();
            (name, inner)
        })
        .collect();

    let patched_dependencies: BTreeMap<String, String> = raw
        .patched_dependencies
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| (k, v.into_path()))
        .collect();

    Ok(LockfileGraph {
        importers,
        packages,
        settings,
        overrides: raw.overrides.unwrap_or_default(),
        ignored_optional_dependencies: raw
            .ignored_optional_dependencies
            .unwrap_or_default()
            .into_iter()
            .collect(),
        times,
        skipped_optional_dependencies,
        catalogs,
        bun_config_version: None,
        patched_dependencies,
        trusted_dependencies: Vec::new(),
        extra_fields: BTreeMap::new(),
        workspace_extra_fields: BTreeMap::new(),
    })
}

/// Write a LockfileGraph as pnpm-lock.yaml v9 format.
pub fn write(path: &Path, graph: &LockfileGraph, manifest: &PackageJson) -> Result<(), Error> {
    let native_pnpm_aliases = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "pnpm-lock.yaml");
    let mut importers = BTreeMap::new();
    let exclude_links = graph.settings.exclude_links_from_lockfile;
    for (importer_path, deps) in &graph.importers {
        let mut importer = WritableImporter::default();

        for dep in deps {
            // `excludeLinksFromLockfile: true` drops `link:` entries
            // from importer dep maps so a sibling-workspace symlink
            // change doesn't churn the lockfile. We check the package
            // table rather than `dep.specifier` because the importer's
            // DirectDep only carries the manifest-written range, not
            // the resolved source kind — the LocalSource lives on the
            // LockedPackage the dep_path points to.
            if exclude_links
                && matches!(
                    graph
                        .packages
                        .get(&dep.dep_path)
                        .and_then(|p| p.local_source.as_ref()),
                    Some(LocalSource::Link(_))
                )
            {
                continue;
            }
            // Specifier sources, in priority order:
            //   1. The specifier recorded on the DirectDep. For workspace
            //      importers this is the only manifest-local specifier the
            //      writer has, because `manifest` is the root package.json.
            //      Hoisted auto-installed peers also use this path.
            //   2. The root manifest entry for old hand-built graphs that
            //      omitted DirectDep.specifier.
            //   3. Fall back to `*` as a last resort.
            let root_manifest_specifier = (importer_path == ".")
                .then(|| match dep.dep_type {
                    DepType::Production => manifest.dependencies.get(&dep.name),
                    DepType::Dev => manifest.dev_dependencies.get(&dep.name),
                    DepType::Optional => manifest.optional_dependencies.get(&dep.name),
                })
                .flatten()
                .map(|s| s.as_str());
            let specifier = dep
                .specifier
                .as_deref()
                .or(root_manifest_specifier)
                .unwrap_or("*");

            // Local deps render with the canonical `file:<path>` /
            // `link:<path>` specifier, not the FS-safe encoded form
            // that lives in `dep_path`.
            let version = if let Some(local) = graph
                .packages
                .get(&dep.dep_path)
                .and_then(|p| p.local_source.as_ref())
            {
                local.specifier()
            } else if native_pnpm_aliases
                && let Some(pkg) = graph.packages.get(&dep.dep_path)
                && let Some(real_name) = pkg.alias_of.as_deref()
            {
                format!("{real_name}@{}", dep_path_tail(&dep.dep_path, &dep.name))
            } else {
                dep.dep_path
                    .strip_prefix(&format!("{}@", dep.name))
                    .unwrap_or(&dep.dep_path)
                    .to_string()
            };

            let spec = WritableDepSpec {
                specifier: specifier.to_string(),
                version,
            };

            match dep.dep_type {
                DepType::Production => {
                    importer
                        .dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Dev => {
                    importer
                        .dev_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
                DepType::Optional => {
                    importer
                        .optional_dependencies
                        .get_or_insert_with(BTreeMap::new)
                        .insert(dep.name.clone(), spec);
                }
            }
        }

        if let Some(skipped) = graph.skipped_optional_dependencies.get(importer_path)
            && !skipped.is_empty()
        {
            let mut map: BTreeMap<String, WritableDepSpec> = BTreeMap::new();
            for (name, specifier) in skipped {
                map.insert(
                    name.clone(),
                    WritableDepSpec {
                        specifier: specifier.clone(),
                        // No installed version on this platform — use a
                        // sentinel that's still parseable as a dep_path
                        // tail by `parse_dep_path` if older code happens
                        // to walk it.
                        version: "0.0.0".to_string(),
                    },
                );
            }
            importer.skipped_optional_dependencies = Some(map);
        }

        importers.insert(importer_path.clone(), importer);
    }

    // pnpm v9 splits the lockfile into two sections:
    //   `packages:` — keyed by the canonical `name@version` (no peer suffix),
    //                 holds the integrity hash and declared peer deps. The
    //                 same package-version with two different peer contexts
    //                 dedupes to a single entry here.
    //   `snapshots:` — keyed by the full contextualized dep_path including
    //                  any `(peer@ver)` suffix, holds the resolved
    //                  `dependencies:` map that the linker walks.
    //
    // We dedupe the packages map via BTreeMap::insert so repeated canonical
    // keys (one per peer context) collapse cleanly, and we take the last
    // writer's integrity/peer decls — they should all agree because they
    // come from the same canonical package.
    let mut packages = BTreeMap::new();
    for pkg in graph.packages.values() {
        // Local deps use the canonical specifier in their key (e.g.
        // `foo@file:./vendor/foo`) so pnpm can read the lockfile.
        // `link:` deps are omitted from the packages section entirely,
        // matching pnpm.
        // Non-registry transitive entries (github overrides, remote
        // tarballs fetched by URL) keep the URL in their dep-path key
        // and carry the real semver on `pkg.version`. `tarball_url`
        // carries the URL through the graph — when the dep-path's
        // version segment is that same URL, the entry was parsed from
        // a URL-keyed pnpm snapshot and needs to round-trip under the
        // same URL key. Paired with the parser's `version_is_http_url
        // && tarball_url.is_some()` gate.
        let url_keyed = pkg
            .tarball_url
            .as_ref()
            .is_some_and(|url| parse_dep_path(&pkg.dep_path).is_some_and(|(_, v)| v == *url));
        let canonical = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    version_to_dep_path(real_name, &pkg.version)
                } else if url_keyed {
                    // Strip any peer suffix; the packages section keys the
                    // canonical form (no peer contexts), the snapshots
                    // section keys the full dep_path.
                    let (name, version) = parse_dep_path(&pkg.dep_path)
                        .unwrap_or_else(|| (pkg.name.clone(), pkg.version.clone()));
                    format!("{name}@{version}")
                } else {
                    version_to_dep_path(&pkg.name, &pkg.version)
                }
            }
        };
        let peer_deps = if pkg.peer_dependencies.is_empty() {
            None
        } else {
            Some(pkg.peer_dependencies.clone())
        };
        let peer_meta = if pkg.peer_dependencies_meta.is_empty() {
            None
        } else {
            Some(
                pkg.peer_dependencies_meta
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            WritablePeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect(),
            )
        };
        // Always render the path through `path_posix()` so the
        // lockfile uses forward slashes regardless of the host OS —
        // a lockfile written on Windows must resolve identically on
        // Unix and vice versa. `Path::display()` honors the host
        // separator, so it would leak `\` into the YAML.
        let is_jsr_registry_pkg = pkg.registry_name().starts_with("@jsr/");
        debug_assert!(
            !is_jsr_registry_pkg || pkg.tarball_url.is_some(),
            "JSR packages must preserve dist.tarball for cold lockfile installs"
        );
        let resolution = match pkg.local_source.as_ref() {
            Some(local @ LocalSource::Directory(_)) => Some(WritableResolution {
                integrity: None,
                directory: Some(local.path_posix()),
                tarball: None,
                commit: None,
                repo: None,
                type_: Some("directory".to_string()),
                path: None,
            }),
            Some(local @ LocalSource::Tarball(_)) => Some(WritableResolution {
                integrity: None,
                directory: None,
                tarball: Some(format!("file:{}", local.path_posix())),
                commit: None,
                repo: None,
                type_: None,
                path: None,
            }),
            Some(LocalSource::Link(_)) => None,
            Some(LocalSource::Git(g)) => Some(WritableResolution {
                integrity: None,
                directory: None,
                tarball: None,
                commit: Some(g.resolved.clone()),
                repo: Some(g.url.clone()),
                type_: Some("git".to_string()),
                // pnpm v9 emits `path: /<sub>` (with leading `/`) on
                // the resolution block when a git dep was installed
                // with a `&path:/<sub>` selector. Keep the same shape
                // so byte-identical round-trips survive.
                path: g.subpath.as_ref().map(|s| format!("/{s}")),
            }),
            Some(LocalSource::RemoteTarball(t)) => Some(WritableResolution {
                integrity: if t.integrity.is_empty() {
                    None
                } else {
                    Some(t.integrity.clone())
                },
                directory: None,
                tarball: Some(t.url.clone()),
                commit: None,
                repo: None,
                type_: None,
                path: None,
            }),
            None if url_keyed => {
                // URL-keyed transitive entries (github overrides, etc.)
                // typically carry no integrity — just the tarball URL
                // in `resolution:`. Gating on `pkg.integrity` would
                // silently drop the tarball on round-trip, and a
                // re-parse would then have no way to fetch the package.
                Some(WritableResolution {
                    integrity: pkg.integrity.clone(),
                    directory: None,
                    tarball: pkg.tarball_url.clone(),
                    commit: None,
                    repo: None,
                    type_: None,
                    path: None,
                })
            }
            None => pkg.integrity.as_ref().map(|i| WritableResolution {
                integrity: Some(i.clone()),
                directory: None,
                // Emit the full registry tarball URL when the setting
                // opts in. JSR packages are the exception: npm.jsr.io
                // uses opaque `dist.tarball` paths that cannot be
                // reconstructed from package name + version, so the
                // URL must be preserved for cold installs from the
                // lockfile.
                tarball: if graph.settings.lockfile_include_tarball_url || is_jsr_registry_pkg {
                    pkg.tarball_url.clone()
                } else {
                    None
                },
                commit: None,
                repo: None,
                type_: None,
                path: None,
            }),
        };
        // Mirror pnpm: emit `version:` alongside the resolution block
        // for URL-keyed transitive entries so tooling that matches
        // packages by (name, version) still has a handle on the real
        // semver. Ordinary registry entries skip this — the key already
        // carries the version, and adding a field would diverge from
        // byte-for-byte pnpm output.
        let write_version = url_keyed.then(|| pkg.version.clone());
        packages.insert(
            canonical,
            WritablePackageInfo {
                resolution,
                version: write_version,
                engines: if pkg.engines.is_empty() {
                    None
                } else {
                    Some(pkg.engines.clone())
                },
                os: pkg.os.to_vec(),
                cpu: pkg.cpu.to_vec(),
                libc: pkg.libc.to_vec(),
                has_bin: !pkg.bin.is_empty(),
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_meta,
                alias_of: (!native_pnpm_aliases)
                    .then(|| pkg.alias_of.clone())
                    .flatten(),
            },
        );
    }

    // Translate internal dep_path tails (`git+<hash>`, `url+<hash>`,
    // `file+<hash>`) to the specifier form pnpm expects in snapshot
    // dependency maps (`<url>#<sha>` for git, raw URL for tarball,
    // `file:<path>` for local). Registry deps keep their plain semver
    // values. The target package's `local_source` is authoritative:
    // the tail alone doesn't encode the URL.
    let rewrite_local_deps = |deps: BTreeMap<String, String>| -> BTreeMap<String, String> {
        deps.into_iter()
            .map(|(name, value)| {
                let dp = version_to_dep_path(&name, &value);
                if let Some(target) = graph
                    .packages
                    .get(&dp)
                    .or_else(|| graph.packages.get(&peerless_dep_path(&name, &value)))
                    && let Some(ref local) = target.local_source
                    && !matches!(local, LocalSource::Link(_))
                {
                    (name, local.specifier())
                } else if native_pnpm_aliases
                    && let Some(target) = graph
                        .packages
                        .get(&dp)
                        .or_else(|| graph.packages.get(&peerless_dep_path(&name, &value)))
                    && let Some(real_name) = target.alias_of.as_deref()
                {
                    (name, format!("{real_name}@{value}"))
                } else {
                    (name, value)
                }
            })
            .collect()
    };
    let mut snapshots = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        // `link:` deps are omitted from snapshots (pnpm parity); other
        // local deps use the canonical specifier key so pnpm's parser
        // lines them up with the packages entry above.
        let key = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => {
                if native_pnpm_aliases && let Some(real_name) = pkg.alias_of.as_deref() {
                    format!("{real_name}@{}", dep_path_tail(dep_path, &pkg.name))
                } else {
                    dep_path.clone()
                }
            }
        };
        let pkg_deps = rewrite_local_deps(pkg.dependencies.clone());
        let pkg_opt_deps = rewrite_local_deps(pkg.optional_dependencies.clone());
        snapshots.insert(
            key,
            WritableSnapshot {
                dependencies: {
                    let mut deps = pkg_deps;
                    for name in pkg_opt_deps.keys() {
                        deps.remove(name);
                    }
                    if deps.is_empty() { None } else { Some(deps) }
                },
                optional_dependencies: if pkg_opt_deps.is_empty() {
                    None
                } else {
                    Some(pkg_opt_deps)
                },
                transitive_peer_dependencies: if pkg.transitive_peer_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.transitive_peer_dependencies.clone())
                },
                optional: if pkg.optional { Some(true) } else { None },
                bundled_dependencies: if pkg.bundled_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.bundled_dependencies.clone())
                },
            },
        );
    }

    let time = if graph.times.is_empty() {
        None
    } else {
        Some(graph.times.clone())
    };

    let catalogs = if graph.catalogs.is_empty() {
        None
    } else {
        Some(
            graph
                .catalogs
                .iter()
                .map(|(name, entries)| {
                    let inner: BTreeMap<String, WritableCatalogEntry> = entries
                        .iter()
                        .map(|(pkg, e)| {
                            (
                                pkg.clone(),
                                WritableCatalogEntry {
                                    specifier: e.specifier.clone(),
                                    version: e.version.clone(),
                                },
                            )
                        })
                        .collect();
                    (name.clone(), inner)
                })
                .collect(),
        )
    };

    let lockfile = WritablePnpmLockfile {
        lockfile_version: "9.0".to_string(),
        settings: WritableSettings {
            auto_install_peers: graph.settings.auto_install_peers,
            exclude_links_from_lockfile: graph.settings.exclude_links_from_lockfile,
            lockfile_include_tarball_url: graph.settings.lockfile_include_tarball_url,
        },
        catalogs,
        // Skipped at serialization time when empty so the YAML stays
        // byte-identical to a no-overrides install.
        overrides: if graph.overrides.is_empty() {
            None
        } else {
            Some(graph.overrides.clone())
        },
        ignored_optional_dependencies: if graph.ignored_optional_dependencies.is_empty() {
            None
        } else {
            Some(
                graph
                    .ignored_optional_dependencies
                    .iter()
                    .cloned()
                    .collect(),
            )
        },
        // pnpm v9 emits patched deps as `{ path, hash }`. We don't
        // track the patch hash on the graph (install-time concern),
        // so write the path form which pnpm still accepts. Skipped
        // when empty to keep parity with no-patch installs.
        patched_dependencies: if graph.patched_dependencies.is_empty() {
            None
        } else {
            Some(graph.patched_dependencies.clone())
        },
        time,
        importers,
        packages,
        snapshots,
    };

    let yaml = yaml_serde::to_string(&lockfile).map_err(|e| Error::parse(path, e.to_string()))?;
    let yaml = reformat_for_pnpm_parity(&yaml);
    // Atomic via tempfile + persist. Crash, Ctrl+C, or AV
    // quarantine during the write used to leave the user with a
    // truncated pnpm-lock.yaml on disk, next install failed to
    // parse and the user thought their lockfile was gone. See
    // atomic_write_lockfile for full rationale.
    crate::atomic_write_lockfile(path, yaml.as_bytes())?;
    Ok(())
}

/// Post-process a `yaml_serde`-emitted pnpm-lock.yaml into the exact
/// shape real pnpm writes. Two tweaks:
///
///   1. Collapse `resolution:` / `engines:` block maps into flow form
///      (`resolution: {integrity: sha512-…}`). pnpm writes both inline
///      and `yaml_serde` can't be coerced into flow style per-field
///      without a custom emitter.
///   2. Insert blank-line separators above every top-level section
///      (`settings:`, `importers:`, `packages:`, `snapshots:`, …) and
///      between 2-indent entries inside the entry-bearing sections
///      (`importers:`, `packages:`, `snapshots:`, `catalogs:`).
///
/// The rewrites are textual — not YAML-aware — but the keys aube emits
/// are all simple scalars in the fixed set above, so there's nothing to
/// quote-escape. Validated by `test_write_byte_identical_to_native_pnpm`.
fn reformat_for_pnpm_parity(yaml: &str) -> String {
    let lines: Vec<&str> = yaml.lines().collect();

    // Pass 1: flow-style `resolution:` / `engines:` blocks.
    let mut compact: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let key = stripped.strip_suffix(':');
        let is_flow_candidate = matches!(key, Some("resolution") | Some("engines"));
        if is_flow_candidate && i + 1 < lines.len() {
            let inner_indent = indent + 2;
            let mut entries: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j];
                let n_stripped = next.trim_start();
                let n_indent = next.len() - n_stripped.len();
                if n_stripped.is_empty() || n_indent != inner_indent {
                    break;
                }
                match n_stripped.split_once(": ") {
                    Some((k, v)) => entries.push(format!("{k}: {v}")),
                    None => break,
                }
                j += 1;
            }
            if !entries.is_empty() {
                compact.push(format!(
                    "{}{}: {{{}}}",
                    " ".repeat(indent),
                    key.unwrap(),
                    entries.join(", ")
                ));
                i = j;
                continue;
            }
        }
        compact.push(line.to_string());
        i += 1;
    }

    // Pass 2: blank-line separators.
    // Sections where each 2-indent key-ending-in-`:` is an entry header
    // that pnpm separates with a blank line above. `overrides:` /
    // `time:` / `settings:` carry scalar key→value pairs instead and
    // stay tight.
    const ENTRY_SECTIONS: &[&str] = &["importers:", "packages:", "snapshots:", "catalogs:"];
    let mut out = String::with_capacity(yaml.len() + 512);
    let mut in_entries = false;
    for (idx, line) in compact.iter().enumerate() {
        let stripped = line.trim_start();
        let indent = line.len() - stripped.len();
        let is_top = indent == 0 && !stripped.is_empty();
        // Entry headers inside `packages:` / `snapshots:` are always at
        // 2-indent with a `:` in the line. Either trailing (`foo@1:`
        // with a child block below) or inline (`foo@1: {}` for empty
        // snapshots). List markers (`- …`) never appear at this level,
        // so a leading `-` rules out false positives on
        // `ignoredOptionalDependencies:` items.
        let is_entry_header =
            in_entries && indent == 2 && !stripped.starts_with('-') && stripped.contains(':');

        if (is_top && idx > 0) || is_entry_header {
            out.push('\n');
        }
        out.push_str(line);
        out.push('\n');

        if is_top {
            in_entries = ENTRY_SECTIONS.contains(&stripped);
        }
    }
    out
}

fn version_to_dep_path(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

fn dep_path_tail<'a>(dep_path: &'a str, name: &str) -> &'a str {
    dep_path
        .strip_prefix(&format!("{name}@"))
        .unwrap_or(dep_path)
}

fn peerless_dep_path(name: &str, value: &str) -> String {
    version_to_dep_path(name, value.split('(').next().unwrap_or(value))
}

fn peerless_alias_target<'a>(
    packages: &'a BTreeMap<String, LockedPackage>,
    real_dep_path: &str,
) -> Option<&'a LockedPackage> {
    let (real_name, real_version) = parse_dep_path(real_dep_path)?;
    packages.get(&version_to_dep_path(&real_name, &real_version))
}

/// Parse a dep path like "@scope/name@1.0.0" or "name@1.0.0" into (name, version).
fn parse_dep_path(dep_path: &str) -> Option<(String, String)> {
    // Strip leading "/" if present (pnpm v6-v8 format)
    let s = dep_path.strip_prefix('/').unwrap_or(dep_path);

    // Find the last '@' that separates name from version
    let at_idx = if s.starts_with('@') {
        // Scoped package: find '@' after the first '/'
        let after_scope = s.find('/')? + 1;
        after_scope + s[after_scope..].find('@')?
    } else {
        s.find('@')?
    };

    let name = s[..at_idx].to_string();
    let version_str = &s[at_idx + 1..];

    // Strip any peer suffix from version (e.g., "1.0.0(react@18.0.0)" -> "1.0.0")
    let version = version_str
        .split('(')
        .next()
        .unwrap_or(version_str)
        .to_string();

    Some((name, version))
}

/// Detect npm-aliased entries inside a snapshot's `dependencies` /
/// `optionalDependencies` map and rewrite them to aube's internal shape.
///
/// pnpm encodes a transitive npm alias as `<alias>: <real>@<resolved>(peers…)`
/// (e.g. `@isaacs/cliui@8.0.2` records `string-width-cjs: string-width@4.2.3`
/// for its `"string-width-cjs": "npm:string-width@^4.2.0"` dep). Aube's
/// linker keys sibling symlinks against `<dep_name>@<dep_value>`, so a raw
/// pnpm value yields a broken `string-width-cjs@string-width@4.2.3` virtual
/// store path. This helper rewrites the value to the bare resolved version
/// (preserving any peer-context suffix) and pushes onto `alias_remaps` so
/// the synthesis loop creates a `<alias>@<resolved>` `LockedPackage` with
/// `alias_of=Some(real)`. After that the linker resolves the alias symlink
/// to the synthetic dir and the resolver's lockfile-reuse path enqueues
/// transitives with `range = <resolved>` (not the malformed
/// `<real>@<resolved>` that no `<alias>` packument can satisfy).
fn rewrite_snapshot_alias_deps(
    deps: &mut BTreeMap<String, String>,
    alias_remaps: &mut Vec<(String, String, String, String)>,
) {
    for (dep_name, dep_value) in deps.iter_mut() {
        let bare = dep_value.split('(').next().unwrap_or(dep_value);
        let Some((real_name, resolved)) = parse_dep_path(bare) else {
            continue;
        };
        if real_name == *dep_name {
            continue;
        }
        let peer_suffix = dep_value.find('(').map(|i| &dep_value[i..]).unwrap_or("");
        let alias_dep_path = format!("{dep_name}@{resolved}{peer_suffix}");
        let real_dep_path = dep_value.clone();
        alias_remaps.push((alias_dep_path, real_dep_path, dep_name.clone(), real_name));
        *dep_value = format!("{resolved}{peer_suffix}");
    }
}

// -- Writable serde types for pnpm-lock.yaml v9 --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePnpmLockfile {
    lockfile_version: String,
    settings: WritableSettings,
    // pnpm v9 places `overrides:` immediately after `settings:` and
    // before `importers:`. Field order matters because we serialize
    // through yaml_serde and want byte-for-byte parity with pnpm output
    // for the no-overrides case (the field is skipped when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<BTreeMap<String, String>>,
    /// pnpm v9+ top-level `patchedDependencies:` — preserved so a
    /// bun→aube-lock conversion keeps the user's patches and a
    /// re-emit doesn't strip the block. pnpm emits this block right
    /// after `overrides:` and before `catalogs:`, so the field order
    /// here follows the same sequence for byte-identical output.
    #[serde(skip_serializing_if = "Option::is_none")]
    patched_dependencies: Option<BTreeMap<String, String>>,
    /// pnpm v9 emits a top-level `catalogs:` map after
    /// `overrides:` and before `importers:` when `pnpm-workspace.yaml`
    /// declares any referenced catalog entries.
    /// Skipped when empty so a no-catalogs install stays byte-identical
    /// to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    catalogs: Option<BTreeMap<String, BTreeMap<String, WritableCatalogEntry>>>,
    /// pnpm v9 emits a top-level `time:` map when `resolution-mode=time-based`
    /// is active. Keyed by canonical `name@version`; values are ISO-8601
    /// publish timestamps pulled from the registry packument. Placed
    /// after `overrides:` and before `importers:` to match pnpm's
    /// field order.
    #[serde(skip_serializing_if = "Option::is_none")]
    time: Option<BTreeMap<String, String>>,
    importers: BTreeMap<String, WritableImporter>,
    packages: BTreeMap<String, WritablePackageInfo>,
    /// pnpm v9 emits a top-level `ignoredOptionalDependencies:` array
    /// after `packages:` and before `snapshots:` when the root
    /// manifest's `pnpm.ignoredOptionalDependencies` is non-empty.
    /// Skipped when empty so a no-ignored install stays byte-for-byte
    /// identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    ignored_optional_dependencies: Option<Vec<String>>,
    snapshots: BTreeMap<String, WritableSnapshot>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSettings {
    auto_install_peers: bool,
    exclude_links_from_lockfile: bool,
    /// Skipped at serialization time when false so pnpm-parity
    /// projects that don't opt into the tarball-URL recording keep
    /// byte-identical lockfiles.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    lockfile_include_tarball_url: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableImporter {
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dev_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
    /// Optionals the resolver intentionally skipped on this importer's
    /// platform — round-tripped so drift detection can distinguish
    /// "previously skipped" from "newly added". Aube-specific extension
    /// to pnpm v9's importer schema; the field is omitted when empty so
    /// no-skip projects stay byte-identical to pnpm output.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_optional_dependencies: Option<BTreeMap<String, WritableDepSpec>>,
}

#[derive(Debug, Serialize)]
struct WritableDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableCatalogEntry {
    specifier: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct WritableResolution {
    #[serde(skip_serializing_if = "Option::is_none")]
    integrity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tarball: Option<String>,
    // Git resolution fields (pnpm v9 `{type: git, repo, commit}` form).
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    type_: Option<String>,
    /// pnpm `&path:/<sub>` selector — emitted with leading `/` to
    /// match pnpm's own writer.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePeerDepMeta {
    // pnpm v9 omits `optional: false` entirely; only the truthy form
    // shows up in real-world lockfiles. Skip the default so we stay
    // byte-identical for the rare case where a packument explicitly
    // marks a peer as non-optional.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    optional: bool,
}

// Field order matches pnpm v9's `packages:` entries: resolution, then
// engines, then os/cpu/libc, then hasBin, then peerDependencies /
// peerDependenciesMeta. Don't reorder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePackageInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution: Option<WritableResolution>,
    /// Real semver for non-registry entries (remote tarball / git),
    /// where the dep-path key is a URL rather than a version. pnpm
    /// emits this field so tooling that reads lockfile entries by
    /// `(name, version)` still finds the right semver. Omitted for
    /// ordinary registry entries — the version lives in the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// pnpm writes `engines: {node: '>=8'}` in flow form immediately
    /// after `resolution:` when the package declared any engines.
    /// Emitted as a block map here — `reformat_for_pnpm_parity` flips it
    /// to flow form to match pnpm byte-for-byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    engines: Option<BTreeMap<String, String>>,
    // pnpm v9 emits os/cpu/libc after `engines` and before `hasBin`.
    // Keep this order to stay byte-identical with pnpm-written lockfiles
    // for native packages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    os: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cpu: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    libc: Vec<String>,
    /// pnpm emits `hasBin: true` only when the package has executables;
    /// `hasBin: false` is never written. Skip the default to match.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    has_bin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_dependencies_meta: Option<BTreeMap<String, WritablePeerDepMeta>>,
    /// Real registry name for npm-alias deps. Aube-specific extension
    /// (pnpm encodes aliases in the snapshot key itself — e.g.
    /// `odd-alias@npm:is-odd@3.0.1` — but aube keys by `alias@version`
    /// for linker simplicity, so the real name has to round-trip
    /// out-of-band via this field). Omitted for non-aliased packages
    /// so non-alias lockfiles stay byte-identical to pnpm's output.
    #[serde(skip_serializing_if = "Option::is_none")]
    alias_of: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritableSnapshot {
    // Order mirrors pnpm's `LockfilePackageSnapshot` emit order
    // (dependencies → optionalDependencies → transitivePeerDependencies
    // → optional) so a parse-then-write round-trip stays diff-clean
    // against pnpm's own output. `bundledDependencies` is not in pnpm's
    // snapshot schema (lives on `LockfilePackageInfo`, pre-existing
    // aube quirk) — placed last so it does not split the pnpm-
    // canonical block.
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transitive_peer_dependencies: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundled_dependencies: Option<Vec<String>>,
}

/// Parse `pnpm-lock.yaml` content, tolerating pnpm v11's multi-document
/// layout.
///
/// pnpm v11 splits the lockfile into two YAML documents: a bootstrap
/// document that tracks pnpm's own `packageManagerDependencies` /
/// `configDependencies`, and the "real" project lockfile (with the
/// workspace's `dependencies` / `devDependencies`, `settings`,
/// `catalogs`, `overrides`, `patchedDependencies`, etc.). We want the
/// second one. Heuristic: score every parseable document by
/// project-lockfile signal (real importer deps + settings/catalogs/
/// overrides + packages/snapshots count) and take the highest. If only
/// one document is present (pnpm v9/v10 and older) this reduces to the
/// previous single-document parse.
fn parse_raw_lockfile(content: &str) -> Result<RawPnpmLockfile, yaml_serde::Error> {
    // Hard cap on documents inspected. pnpm v11 emits exactly two;
    // anything beyond a handful is pathological. This also guards
    // against malformed YAML that puts
    // `yaml_serde::Deserializer::from_str`'s iterator into an
    // infinite-yield state — `test_parse_invalid_yaml` tripped that
    // mode on Windows CI with an unbounded loop.
    const MAX_DOCUMENTS: usize = 16;

    let mut best: Option<(u64, RawPnpmLockfile)> = None;
    let mut first_err: Option<yaml_serde::Error> = None;
    for (idx, doc) in yaml_serde::Deserializer::from_str(content)
        .enumerate()
        .take(MAX_DOCUMENTS)
    {
        match RawPnpmLockfile::deserialize(doc) {
            Ok(raw) => {
                let score = project_lockfile_score(&raw);
                best = match best {
                    Some((prev, _)) if prev >= score => best,
                    _ => Some((score, raw)),
                };
            }
            Err(e) => {
                // Log every per-document failure so a multi-doc
                // lockfile where every document fails surfaces all the
                // diagnostic signal at `RUST_LOG=aube_lockfile=debug`.
                // Break on the first failure: a malformed document
                // typically puts yaml_serde's iterator into a state
                // where further iteration is either more garbage or an
                // infinite loop (see `test_parse_invalid_yaml`). The
                // returned error is the first failure, which is both
                // most explanatory and the only one we actually
                // observed.
                tracing::debug!("pnpm-lock.yaml document {idx} failed to parse: {e}");
                first_err = Some(e);
                break;
            }
        }
    }
    match (best, first_err) {
        (Some((_, raw)), _) => Ok(raw),
        (None, Some(e)) => Err(e),
        // No documents at all — defer to the single-doc parser so the
        // error surface matches what callers saw before.
        (None, None) => yaml_serde::from_str(content),
    }
}

/// Score for picking the "main" document out of a multi-document
/// `pnpm-lock.yaml`. Weighted so a document with real importer
/// dependencies beats one with only `packageManagerDependencies`
/// (pnpm v11's bootstrap doc has the latter but no regular deps).
fn project_lockfile_score(raw: &RawPnpmLockfile) -> u64 {
    let importer_dep_count: usize = raw
        .importers
        .values()
        .map(|i| {
            i.dependencies.as_ref().map(|m| m.len()).unwrap_or(0)
                + i.dev_dependencies.as_ref().map(|m| m.len()).unwrap_or(0)
                + i.optional_dependencies
                    .as_ref()
                    .map(|m| m.len())
                    .unwrap_or(0)
        })
        .sum();
    let mut score = importer_dep_count as u64 * 1000;
    if raw.settings.is_some() {
        score += 100;
    }
    if raw.catalogs.as_ref().is_some_and(|c| !c.is_empty()) {
        score += 100;
    }
    if raw.overrides.as_ref().is_some_and(|o| !o.is_empty()) {
        score += 100;
    }
    score += raw.packages.len() as u64;
    score += raw.snapshots.len() as u64;
    score
}

// -- Raw serde types for pnpm-lock.yaml v9 (deserialization) --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPnpmLockfile {
    #[allow(dead_code)]
    lockfile_version: yaml_serde::Value,
    #[serde(default)]
    settings: Option<RawSettings>,
    #[serde(default)]
    overrides: Option<BTreeMap<String, String>>,
    #[serde(default)]
    catalogs: Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>>,
    /// pnpm v9+ top-level `patchedDependencies:` block. Map of
    /// `pkg@version` selector → patch entry (pnpm uses a nested
    /// `{ path, hash }` object, but we only model the path string
    /// on the shared graph). Round-tripped verbatim so a parse/
    /// write cycle doesn't drop user patches.
    #[serde(default)]
    patched_dependencies: Option<BTreeMap<String, RawPatchedDependency>>,
    #[serde(default)]
    ignored_optional_dependencies: Option<Vec<String>>,
    #[serde(default)]
    importers: BTreeMap<String, RawImporter>,
    #[serde(default)]
    packages: BTreeMap<String, RawPackageInfo>,
    #[serde(default)]
    snapshots: BTreeMap<String, RawSnapshot>,
    #[serde(default)]
    time: Option<BTreeMap<String, String>>,
}

/// pnpm writes `patchedDependencies` as either a bare path string
/// (v8 style) or a nested `{ path, hash }` object (v9+). We accept
/// both via an untagged enum and collapse to the path string on the
/// shared graph.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPatchedDependency {
    Path(String),
    Object {
        path: String,
        #[serde(default)]
        #[allow(dead_code)]
        hash: Option<String>,
    },
}

impl RawPatchedDependency {
    fn into_path(self) -> String {
        match self {
            RawPatchedDependency::Path(p) => p,
            RawPatchedDependency::Object { path, .. } => path,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSettings {
    #[serde(default)]
    auto_install_peers: Option<bool>,
    #[serde(default)]
    exclude_links_from_lockfile: Option<bool>,
    #[serde(default)]
    lockfile_include_tarball_url: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawImporter {
    dependencies: Option<BTreeMap<String, RawDepSpec>>,
    dev_dependencies: Option<BTreeMap<String, RawDepSpec>>,
    optional_dependencies: Option<BTreeMap<String, RawDepSpec>>,
    skipped_optional_dependencies: Option<BTreeMap<String, RawDepSpec>>,
}

#[derive(Debug, Deserialize)]
struct RawDepSpec {
    specifier: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct RawCatalogEntry {
    specifier: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPackageInfo {
    resolution: Option<Resolution>,
    #[serde(default)]
    engines: BTreeMap<String, String>,
    peer_dependencies: Option<BTreeMap<String, String>>,
    peer_dependencies_meta: Option<BTreeMap<String, RawPeerDepMeta>>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    libc: Vec<String>,
    #[serde(default)]
    has_bin: bool,
    /// Paired writer field. See `WritablePackageInfo::alias_of`. `None`
    /// for ordinary (non-aliased) packages.
    #[serde(default)]
    alias_of: Option<String>,
    /// pnpm emits `version: <semver>` on `packages:` entries whose dep-path
    /// key is a URL (remote tarball, git) rather than a bare semver —
    /// that way the key stays unique (one URL, one entry) while the real
    /// semver is still recorded for tooling. None for ordinary registry
    /// entries, where the version lives in the dep-path key itself.
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPeerDepMeta {
    #[serde(default)]
    optional: bool,
}

#[derive(Debug, Deserialize)]
struct Resolution {
    integrity: Option<String>,
    #[serde(default)]
    directory: Option<String>,
    #[serde(default)]
    tarball: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default, rename = "type")]
    #[allow(dead_code)]
    type_: Option<String>,
    /// pnpm `&path:/<sub>` selector for git deps. Newer pnpm
    /// (>= v9.x) emits this on the resolution block in addition to
    /// encoding it in the snapshot key.
    #[serde(default, deserialize_with = "deserialize_subpath")]
    path: Option<String>,
}

/// Strip the leading `/` from pnpm's `path:` field so the value lines
/// up with how `parse_git_fragment` stores it. Mirror the same
/// `..`/`.`/empty-component guard as the in-URL parser so a crafted
/// lockfile cannot direct the resolver to read a `package.json`
/// outside the clone dir.
fn deserialize_subpath<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<String> = serde::Deserialize::deserialize(de)?;
    Ok(raw.and_then(|s| {
        let trimmed = s.trim_start_matches('/');
        if trimmed.is_empty()
            || trimmed
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            None
        } else {
            Some(trimmed.to_string())
        }
    }))
}

/// Convert a pnpm `resolution:` block into a `LocalSource` classification.
/// Returns `None` for registry-sourced packages (plain integrity with no
/// tarball/directory/repo fields). Shared by the direct-dep and
/// transitive-dep reader paths so both stay in lockstep when new
/// resolution shapes are added.
fn local_source_from_resolution(res: &Resolution) -> Option<LocalSource> {
    if let Some(ref tb) = res.tarball {
        if let Some(rel) = tb.strip_prefix("file:") {
            return Some(LocalSource::Tarball(std::path::PathBuf::from(rel)));
        }
        if tb.starts_with("http://") || tb.starts_with("https://") {
            return Some(LocalSource::RemoteTarball(crate::RemoteTarballSource {
                url: tb.clone(),
                integrity: res.integrity.clone().unwrap_or_default(),
            }));
        }
        return None;
    }
    if let Some(ref dir) = res.directory {
        return Some(LocalSource::Directory(std::path::PathBuf::from(dir)));
    }
    if let (Some(repo), Some(commit)) = (res.repo.as_ref(), res.commit.as_ref()) {
        return Some(LocalSource::Git(GitSource {
            url: repo.clone(),
            committish: None,
            resolved: commit.clone(),
            subpath: res.path.clone(),
        }));
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSnapshot {
    #[serde(default)]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(default)]
    bundled_dependencies: Option<Vec<String>>,
    #[serde(default)]
    optional: Option<bool>,
    #[serde(default)]
    transitive_peer_dependencies: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dep_path_simple() {
        let (name, version) = parse_dep_path("lodash@4.17.21").unwrap();
        assert_eq!(name, "lodash");
        assert_eq!(version, "4.17.21");
    }

    #[test]
    fn test_parse_dep_path_scoped() {
        let (name, version) = parse_dep_path("@babel/core@7.24.0").unwrap();
        assert_eq!(name, "@babel/core");
        assert_eq!(version, "7.24.0");
    }

    #[test]
    fn test_parse_dep_path_scoped_nested() {
        let (name, version) = parse_dep_path("@types/node@20.11.0").unwrap();
        assert_eq!(name, "@types/node");
        assert_eq!(version, "20.11.0");
    }

    #[test]
    fn test_parse_dep_path_with_leading_slash() {
        let (name, version) = parse_dep_path("/lodash@4.17.21").unwrap();
        assert_eq!(name, "lodash");
        assert_eq!(version, "4.17.21");
    }

    #[test]
    fn test_parse_dep_path_with_peer_suffix() {
        let (name, version) = parse_dep_path("foo@1.0.0(react@18.0.0)").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(version, "1.0.0");
    }

    #[test]
    fn test_parse_dep_path_with_multiple_peer_suffixes() {
        let (name, version) = parse_dep_path("foo@2.0.0(react@18.0.0)(react-dom@18.0.0)").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(version, "2.0.0");
    }

    #[test]
    fn test_parse_dep_path_prerelease() {
        let (name, version) = parse_dep_path("foo@1.0.0-beta.1").unwrap();
        assert_eq!(name, "foo");
        assert_eq!(version, "1.0.0-beta.1");
    }

    #[test]
    fn test_parse_dep_path_no_at() {
        assert!(parse_dep_path("invalid").is_none());
    }

    #[test]
    fn test_version_to_dep_path() {
        assert_eq!(version_to_dep_path("foo", "1.0.0"), "foo@1.0.0");
        assert_eq!(
            version_to_dep_path("@scope/pkg", "2.0.0"),
            "@scope/pkg@2.0.0"
        );
    }

    #[test]
    fn test_parse_fixture_lockfile() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
        if !fixture.exists() {
            return;
        }

        let graph = parse(&fixture).unwrap();

        // Check importers
        let root_deps = graph.importers.get(".").unwrap();
        assert_eq!(root_deps.len(), 2);
        assert!(root_deps.iter().any(|d| d.name == "is-odd"));
        assert!(root_deps.iter().any(|d| d.name == "is-even"));

        // Check packages
        assert_eq!(graph.packages.len(), 7);
        assert!(graph.packages.contains_key("is-odd@3.0.1"));
        assert!(graph.packages.contains_key("is-even@1.0.0"));
        assert!(graph.packages.contains_key("is-buffer@1.1.6"));

        // Check dependencies in snapshots
        let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
        assert_eq!(is_odd.dependencies.get("is-number").unwrap(), "6.0.0");

        let is_even = graph.packages.get("is-even@1.0.0").unwrap();
        assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

        // Check integrity hashes exist
        assert!(is_odd.integrity.is_some());
    }

    #[test]
    fn test_parse_fixture_dep_types() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
        if !fixture.exists() {
            return;
        }

        let graph = parse(&fixture).unwrap();
        let root_deps = graph.importers.get(".").unwrap();

        // Both deps in basic fixture are production deps
        for dep in root_deps {
            assert_eq!(dep.dep_type, DepType::Production);
        }
    }

    #[test]
    fn test_parse_fixture_transitive_chain() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/basic/pnpm-lock.yaml");
        if !fixture.exists() {
            return;
        }

        let graph = parse(&fixture).unwrap();

        // is-odd@3.0.1 -> is-number@6.0.0 (no further deps)
        let is_odd = graph.packages.get("is-odd@3.0.1").unwrap();
        assert_eq!(is_odd.dependencies.len(), 1);
        let is_number_6 = graph.packages.get("is-number@6.0.0").unwrap();
        assert!(is_number_6.dependencies.is_empty());

        // is-even@1.0.0 -> is-odd@0.1.2 -> is-number@3.0.0 -> kind-of@3.2.2 -> is-buffer@1.1.6
        let is_even = graph.packages.get("is-even@1.0.0").unwrap();
        assert_eq!(is_even.dependencies.get("is-odd").unwrap(), "0.1.2");

        let is_odd_old = graph.packages.get("is-odd@0.1.2").unwrap();
        assert_eq!(is_odd_old.dependencies.get("is-number").unwrap(), "3.0.0");

        let is_number_3 = graph.packages.get("is-number@3.0.0").unwrap();
        assert_eq!(is_number_3.dependencies.get("kind-of").unwrap(), "3.2.2");

        let kind_of = graph.packages.get("kind-of@3.2.2").unwrap();
        assert_eq!(kind_of.dependencies.get("is-buffer").unwrap(), "1.1.6");
    }

    #[test]
    fn parse_normalizes_empty_root_importer_key() {
        // Some pnpm v9 lockfiles in the wild (e.g. npmx.dev) write the
        // root importer as `''` (empty key) rather than `'.'`. Both
        // mean "workspace root" — we must normalize so the linker's
        // `importers.get(".")` lookup still hits.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

snapshots:
  host@1.0.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let root = graph
            .importers
            .get(".")
            .expect("empty-string importer should normalize to `.`");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "host");
        assert!(!graph.importers.contains_key(""));
    }

    #[test]
    fn parse_handles_both_empty_and_dot_root_importer_keys() {
        // Degenerate case pnpm itself never emits: a lockfile with
        // *both* `''` and `'.'` as separate YAML keys for root. The
        // BTreeMap visits `''` first; without the collision guard
        // the real `'.'` entry silently overwrites the normalized
        // empty-key entry and its deps disappear. First-key wins is
        // arbitrary but deterministic; the important behavior is
        // that no deps get silently dropped on the floor.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  '':
    dependencies:
      from-empty:
        specifier: 1.0.0
        version: 1.0.0
  '.':
    dependencies:
      from-dot:
        specifier: 1.0.0
        version: 1.0.0

packages:
  from-empty@1.0.0:
    resolution: {integrity: sha512-empty}
  from-dot@1.0.0:
    resolution: {integrity: sha512-dot}

snapshots:
  from-empty@1.0.0: {}
  from-dot@1.0.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let root = graph.importers.get(".").expect("`.` importer present");
        let names: Vec<&str> = root.iter().map(|d| d.name.as_str()).collect();
        // The empty-key entry is visited first and wins; the `.`
        // entry's deps are ignored (rather than silently clobbering).
        assert_eq!(names, vec!["from-empty"]);
        assert!(!graph.importers.contains_key(""));
    }

    #[test]
    fn parse_snapshot_optional_dependencies_as_edges() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      host:
        specifier: 1.0.0
        version: 1.0.0

packages:
  host@1.0.0:
    resolution: {integrity: sha512-host}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  host@1.0.0:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let host = graph.packages.get("host@1.0.0").unwrap();
        assert_eq!(host.dependencies.get("native").unwrap(), "1.0.0");
        assert_eq!(host.optional_dependencies.get("native").unwrap(), "1.0.0");
    }

    #[test]
    fn parse_package_platform_fields_accept_scalar_strings() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      sass-embedded-linux-arm64:
        specifier: 1.99.0
        version: 1.99.0

packages:
  sass-embedded-linux-arm64@1.99.0:
    resolution: {integrity: sha512-native}
    engines: {node: '>=14.0.0'}
    cpu: arm64
    os: linux
    libc: glibc

snapshots:
  sass-embedded-linux-arm64@1.99.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let pkg = graph
            .packages
            .get("sass-embedded-linux-arm64@1.99.0")
            .unwrap();
        assert_eq!(pkg.os.as_slice(), &["linux".to_string()]);
        assert_eq!(pkg.cpu.as_slice(), &["arm64".to_string()]);
        assert_eq!(pkg.libc.as_slice(), &["glibc".to_string()]);
    }

    #[test]
    fn parse_local_snapshot_optional_dependencies_as_edges() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-host:
        specifier: file:./local-host
        version: file:./local-host

packages:
  local-host@file:./local-host:
    resolution: {directory: ./local-host, type: directory}

  native@1.0.0:
    resolution: {integrity: sha512-native}
    cpu: [arm64]
    os: [darwin]

snapshots:
  local-host@file:./local-host:
    optionalDependencies:
      native: 1.0.0

  native@1.0.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let local = graph
            .packages
            .values()
            .find(|pkg| pkg.name == "local-host")
            .unwrap();
        assert_eq!(local.dependencies.get("native").unwrap(), "1.0.0");
        assert_eq!(local.optional_dependencies.get("native").unwrap(), "1.0.0");
    }

    #[test]
    fn parse_transitive_url_entry_uses_pnpm_version_field() {
        // Regression: pnpm writes non-registry transitive entries with
        // the tarball URL in the dep-path key and the real semver in a
        // `version:` field. Parsing used the URL as the `version`
        // itself, and the install path's store-content cross-check then
        // compared the URL against the tarball's declared `2.4.1` and
        // failed every override'd github dep.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:
  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:
    resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}
    version: 2.4.1

snapshots:
  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let url = "https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65";
        let pkg = graph
            .packages
            .get(&format!("node-expat@{url}"))
            .expect("transitive remote-tarball entry present");
        assert_eq!(pkg.name, "node-expat");
        // pnpm's `version:` field, not the URL.
        assert_eq!(pkg.version, "2.4.1");
        // The URL drives the fetch path via `tarball_url`; dep-path
        // still carries the URL so xml2json's snapshot reference
        // resolves.
        assert_eq!(pkg.tarball_url.as_deref(), Some(url));
    }

    #[test]
    fn url_dep_path_round_trips_with_pnpm_version_field() {
        // Write-side companion: the URL has to stay in the canonical
        // key and the `version:` field has to reappear in the written
        // output so tooling reading the file back sees the same shape
        // pnpm wrote.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        let src = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      xml2json:
        specifier: ^0.12.0
        version: 0.12.0

packages:

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:
    resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}
    version: 2.4.1

  xml2json@0.12.0:
    resolution: {integrity: sha512-xxx}

snapshots:

  node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65: {}

  xml2json@0.12.0:
    dependencies:
      node-expat: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65
"#;
        std::fs::write(&lockfile_path, src).unwrap();
        let graph = parse(&lockfile_path).unwrap();

        let manifest = PackageJson {
            name: Some("root".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: [("xml2json".to_string(), "^0.12.0".to_string())]
                .into_iter()
                .collect(),
            ..PackageJson::default()
        };
        let out_path = dir.path().join("round-trip.yaml");
        write(&out_path, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(
            written.contains("node-expat@https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65:"),
            "URL canonical key missing from output: {written}"
        );
        assert!(
            written.contains("    version: 2.4.1"),
            "`version:` field missing from output: {written}"
        );
        // Round-trip must preserve the `resolution: {tarball: …}` block.
        // URL-keyed transitives typically have no integrity, so gating
        // the block on `pkg.integrity` would silently drop the tarball
        // URL and a re-parse would have no way to fetch the package.
        assert!(
            written.contains("resolution: {tarball: https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65}"),
            "`resolution: {{tarball: …}}` missing from output: {written}"
        );
        // Re-parse the written lockfile and assert the tarball URL
        // makes it all the way back onto `LockedPackage.tarball_url`.
        let reparsed = parse(&out_path).unwrap();
        let url = "https://codeload.github.com/PruvoNet/node-expat/tar.gz/0732e16b0b679da2d12e062f78b3a511f419bb65";
        let pkg = reparsed
            .packages
            .get(&format!("node-expat@{url}"))
            .expect("URL-keyed entry survives round-trip");
        assert_eq!(pkg.version, "2.4.1");
        assert_eq!(pkg.tarball_url.as_deref(), Some(url));
    }

    #[test]
    fn direct_url_importer_strips_peer_suffix_from_fetch_url() {
        // Regression: when a direct dep's importer `version:` is a
        // tarball URL *with* a pnpm peer-context suffix
        // (`(peer@ver)`), the parser used to bake the whole string
        // into `RemoteTarballSource.url`, so the install path fetched
        // `…/tar.gz/SHA(peer@ver)` and hit a 404.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &lockfile_path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      dep-a:
        specifier: github:owner/dep-a#abcdef1234567890abcdef1234567890abcdef12
        version: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13)

packages:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12:
    resolution: {tarball: https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12}
    version: 1.0.0

  encoding@0.1.13:
    resolution: {integrity: sha512-enc}

snapshots:
  dep-a@https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12(encoding@0.1.13):
    dependencies:
      encoding: 0.1.13

  encoding@0.1.13: {}
"#,
        )
        .unwrap();

        let graph = parse(&lockfile_path).unwrap();
        let clean_url = "https://codeload.github.com/owner/dep-a/tar.gz/abcdef1234567890abcdef1234567890abcdef12";

        let dep_a = graph
            .packages
            .values()
            .find(|pkg| pkg.name == "dep-a")
            .expect("dep-a present after parse");
        match dep_a.local_source.as_ref() {
            Some(LocalSource::RemoteTarball(t)) => {
                assert_eq!(
                    t.url, clean_url,
                    "peer suffix leaked into RemoteTarballSource.url — fetch would 404"
                );
            }
            other => panic!("expected RemoteTarball, got {other:?}"),
        }
        // The snapshot carrying the peer suffix shouldn't produce a
        // second entry — that would round-trip as a stray packages
        // block.
        let dep_a_entries: Vec<_> = graph
            .packages
            .values()
            .filter(|p| p.name == "dep-a")
            .collect();
        assert_eq!(
            dep_a_entries.len(),
            1,
            "exactly one dep-a entry expected (suffix'd snapshot should fold into the local)"
        );
        // Transitive deps declared on the peer-context'd snapshot flow
        // onto the local package.
        assert_eq!(
            dep_a.dependencies.get("encoding"),
            Some(&"0.1.13".to_string())
        );
    }

    #[test]
    fn test_write_and_reparse_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        // Build a graph
        let mut packages = BTreeMap::new();
        let mut foo_deps = BTreeMap::new();
        foo_deps.insert("bar".to_string(), "2.0.0".to_string());
        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-abc123==".to_string()),
                dependencies: foo_deps,
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            LockedPackage {
                name: "bar".to_string(),
                version: "2.0.0".to_string(),
                integrity: Some("sha512-def456==".to_string()),
                dependencies: BTreeMap::new(),
                dep_path: "bar@2.0.0".to_string(),
                ..Default::default()
            },
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1.0.0".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let mut deps = BTreeMap::new();
        deps.insert("foo".to_string(), "^1.0.0".to_string());
        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: deps,
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };

        write(&lockfile_path, &graph, &manifest).unwrap();

        // Re-parse and verify
        let reparsed = parse(&lockfile_path).unwrap();
        assert_eq!(reparsed.packages.len(), 2);
        assert_eq!(
            reparsed.packages.get("foo@1.0.0").unwrap().integrity,
            Some("sha512-abc123==".to_string())
        );
        assert_eq!(
            reparsed
                .packages
                .get("foo@1.0.0")
                .unwrap()
                .dependencies
                .get("bar")
                .unwrap(),
            "2.0.0"
        );

        let root_deps = reparsed.importers.get(".").unwrap();
        assert_eq!(root_deps.len(), 1);
        assert_eq!(root_deps[0].name, "foo");
        assert_eq!(root_deps[0].dep_type, DepType::Production);
    }

    #[test]
    fn writer_preserves_workspace_importer_specifiers() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut packages = BTreeMap::new();
        packages.insert(
            "@dev/build-tools@1.0.0".to_string(),
            LockedPackage {
                name: "@dev/build-tools".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "@dev/build-tools@1.0.0".to_string(),
                ..Default::default()
            },
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "@dev/build-tools".to_string(),
                dep_path: "@dev/build-tools@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("^1.0.0".to_string()),
            }],
        );
        importers.insert(
            "packages/public/umd/babylonjs".to_string(),
            vec![DirectDep {
                name: "@dev/build-tools".to_string(),
                dep_path: "@dev/build-tools@1.0.0".to_string(),
                dep_type: DepType::Dev,
                specifier: Some("1.0.0".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let mut root_dev_dependencies = BTreeMap::new();
        root_dev_dependencies.insert("@dev/build-tools".to_string(), "^1.0.0".to_string());
        let manifest = PackageJson {
            name: Some("root".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: BTreeMap::new(),
            dev_dependencies: root_dev_dependencies,
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };

        write(&lockfile_path, &graph, &manifest).unwrap();

        let reparsed = parse(&lockfile_path).unwrap();
        let workspace_deps = reparsed
            .importers
            .get("packages/public/umd/babylonjs")
            .unwrap();
        assert_eq!(workspace_deps[0].specifier.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn overrides_round_trip_through_pnpm_lock_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut overrides = BTreeMap::new();
        overrides.insert("lodash".to_string(), "4.17.21".to_string());
        overrides.insert("foo".to_string(), "npm:bar@^2".to_string());

        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages: BTreeMap::new(),
            overrides,
            ..Default::default()
        };

        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };

        write(&lockfile_path, &graph, &manifest).unwrap();

        // The serialized YAML must contain an `overrides:` block — guard
        // against a future serde change silently dropping the field.
        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
        assert!(
            yaml.contains("overrides:"),
            "expected `overrides:` block in:\n{yaml}"
        );

        let reparsed = parse(&lockfile_path).unwrap();
        assert_eq!(reparsed.overrides.len(), 2);
        assert_eq!(reparsed.overrides.get("lodash").unwrap(), "4.17.21");
        assert_eq!(reparsed.overrides.get("foo").unwrap(), "npm:bar@^2");
    }

    /// `patchedDependencies:` must land between `overrides:` and
    /// `catalogs:` in the emitted YAML — that's where pnpm itself
    /// writes it, and any other position produces a gratuitous diff
    /// against pnpm's output on every install.
    #[test]
    fn patched_dependencies_emitted_after_overrides_before_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut overrides = BTreeMap::new();
        overrides.insert("lodash".to_string(), "4.17.21".to_string());
        let mut patched_dependencies = BTreeMap::new();
        patched_dependencies.insert(
            "lodash@4.17.21".to_string(),
            "patches/lodash@4.17.21.patch".to_string(),
        );
        let mut default_catalog = BTreeMap::new();
        default_catalog.insert(
            "react".to_string(),
            CatalogEntry {
                specifier: "^18.2.0".to_string(),
                version: "18.2.0".to_string(),
            },
        );
        let mut catalogs = BTreeMap::new();
        catalogs.insert("default".to_string(), default_catalog);

        let graph = LockfileGraph {
            overrides,
            patched_dependencies,
            catalogs,
            ..Default::default()
        };

        let manifest = PackageJson {
            name: Some("test".to_string()),
            ..Default::default()
        };

        write(&lockfile_path, &graph, &manifest).unwrap();
        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();

        let overrides_at = yaml.find("overrides:").expect("overrides:");
        let patched_at = yaml
            .find("patchedDependencies:")
            .expect("patchedDependencies:");
        let catalogs_at = yaml.find("catalogs:").expect("catalogs:");
        assert!(
            overrides_at < patched_at && patched_at < catalogs_at,
            "expected order: overrides < patchedDependencies < catalogs, got\n{yaml}"
        );
    }

    #[test]
    fn empty_overrides_block_omitted_from_yaml() {
        // Default-empty overrides should not introduce an `overrides:` key
        // in the lockfile — important for byte-identical parity with pnpm
        // on the no-overrides path.
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");
        let graph = LockfileGraph::default();
        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };
        write(&lockfile_path, &graph, &manifest).unwrap();
        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
        assert!(
            !yaml.contains("overrides:"),
            "unexpected overrides block:\n{yaml}"
        );
    }

    #[test]
    fn test_write_dev_and_optional_deps() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut packages = BTreeMap::new();
        for (name, ver) in [("foo", "1.0.0"), ("bar", "2.0.0"), ("baz", "3.0.0")] {
            packages.insert(
                format!("{name}@{ver}"),
                LockedPackage {
                    name: name.to_string(),
                    version: ver.to_string(),
                    integrity: None,
                    dependencies: BTreeMap::new(),
                    dep_path: format!("{name}@{ver}"),
                    ..Default::default()
                },
            );
        }

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".to_string(),
                    dep_path: "foo@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".to_string()),
                },
                DirectDep {
                    name: "bar".to_string(),
                    dep_path: "bar@2.0.0".to_string(),
                    dep_type: DepType::Dev,
                    specifier: Some("^2.0.0".to_string()),
                },
                DirectDep {
                    name: "baz".to_string(),
                    dep_path: "baz@3.0.0".to_string(),
                    dep_type: DepType::Optional,
                    specifier: Some("^3.0.0".to_string()),
                },
            ],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let mut deps = BTreeMap::new();
        deps.insert("foo".to_string(), "^1.0.0".to_string());
        let mut dev_deps = BTreeMap::new();
        dev_deps.insert("bar".to_string(), "^2.0.0".to_string());
        let mut opt_deps = BTreeMap::new();
        opt_deps.insert("baz".to_string(), "^3.0.0".to_string());

        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: deps,
            dev_dependencies: dev_deps,
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: opt_deps,
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };

        write(&lockfile_path, &graph, &manifest).unwrap();

        let reparsed = parse(&lockfile_path).unwrap();
        let root_deps = reparsed.importers.get(".").unwrap();
        assert_eq!(root_deps.len(), 3);

        let bar = root_deps.iter().find(|d| d.name == "bar").unwrap();
        assert_eq!(bar.dep_type, DepType::Dev);

        let baz = root_deps.iter().find(|d| d.name == "baz").unwrap();
        assert_eq!(baz.dep_type, DepType::Optional);
    }

    #[test]
    fn test_catalogs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut default_cat = BTreeMap::new();
        default_cat.insert(
            "react".to_string(),
            CatalogEntry {
                specifier: "^18.0.0".to_string(),
                version: "18.2.0".to_string(),
            },
        );
        let mut catalogs = BTreeMap::new();
        catalogs.insert("default".to_string(), default_cat);

        let graph = LockfileGraph {
            catalogs,
            ..Default::default()
        };
        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            ..Default::default()
        };
        write(&lockfile_path, &graph, &manifest).unwrap();

        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
        assert!(
            yaml.contains("catalogs:"),
            "missing catalogs section: {yaml}"
        );
        assert!(yaml.contains("react"), "missing entry: {yaml}");

        let reparsed = parse(&lockfile_path).unwrap();
        let entry = reparsed
            .catalogs
            .get("default")
            .and_then(|c| c.get("react"))
            .expect("react catalog entry");
        assert_eq!(entry.specifier, "^18.0.0");
        assert_eq!(entry.version, "18.2.0");
    }

    #[test]
    fn ignored_optional_dependencies_section_matches_pnpm_order() {
        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut ignored_optional_dependencies = std::collections::BTreeSet::new();
        ignored_optional_dependencies.insert("fsevents".to_string());

        let mut default_cat = BTreeMap::new();
        default_cat.insert(
            "react".to_string(),
            CatalogEntry {
                specifier: "^18.0.0".to_string(),
                version: "18.2.0".to_string(),
            },
        );
        let mut catalogs = BTreeMap::new();
        catalogs.insert("default".to_string(), default_cat);

        let graph = LockfileGraph {
            ignored_optional_dependencies,
            catalogs,
            ..Default::default()
        };
        let manifest = PackageJson {
            name: Some("test".to_string()),
            version: Some("0.0.0".to_string()),
            ..Default::default()
        };
        write(&lockfile_path, &graph, &manifest).unwrap();

        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
        let catalogs = yaml.find("\ncatalogs:").expect("missing catalogs");
        let importers = yaml.find("\nimporters:").expect("missing importers");
        let packages = yaml.find("\npackages:").expect("missing packages");
        let ignored = yaml
            .find("\nignoredOptionalDependencies:")
            .expect("missing ignoredOptionalDependencies");
        let snapshots = yaml.find("\nsnapshots:").expect("missing snapshots");

        assert!(
            catalogs < importers
                && importers < packages
                && packages < ignored
                && ignored < snapshots,
            "unexpected pnpm section order:\n{yaml}"
        );
    }

    // Build a graph with one `link:` dep and one registry dep, write it
    // with `excludeLinksFromLockfile: true`, and confirm the `link:`
    // entry vanishes from the importer's `dependencies:` map while the
    // registry dep survives. Guards the filter in the importer loop.
    #[test]
    fn exclude_links_from_lockfile_drops_link_deps_from_importer() {
        use crate::{LocalSource, LockfileSettings};
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let lockfile_path = dir.path().join("pnpm-lock.yaml");

        let mut packages = BTreeMap::new();
        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-abc==".to_string()),
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        packages.insert(
            "sibling@link:../sibling".to_string(),
            LockedPackage {
                name: "sibling".to_string(),
                version: "0.0.0".to_string(),
                dep_path: "sibling@link:../sibling".to_string(),
                local_source: Some(LocalSource::Link(PathBuf::from("../sibling"))),
                ..Default::default()
            },
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".to_string(),
                    dep_path: "foo@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1.0.0".to_string()),
                },
                DirectDep {
                    name: "sibling".to_string(),
                    dep_path: "sibling@link:../sibling".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("link:../sibling".to_string()),
                },
            ],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            settings: LockfileSettings {
                auto_install_peers: true,
                exclude_links_from_lockfile: true,
                lockfile_include_tarball_url: false,
            },
            ..Default::default()
        };

        let mut deps = BTreeMap::new();
        deps.insert("foo".to_string(), "^1.0.0".to_string());
        deps.insert("sibling".to_string(), "link:../sibling".to_string());
        let manifest = PackageJson {
            name: Some("root".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: deps,
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            update_config: None,
            scripts: BTreeMap::new(),
            engines: BTreeMap::new(),
            workspaces: None,
            bundled_dependencies: None,
            extra: BTreeMap::new(),
        };

        write(&lockfile_path, &graph, &manifest).unwrap();

        let yaml = std::fs::read_to_string(&lockfile_path).unwrap();
        assert!(
            yaml.contains("excludeLinksFromLockfile: true"),
            "settings header must record the flag: {yaml}"
        );
        assert!(
            !yaml.contains("sibling:"),
            "sibling link dep should be filtered out of importers: {yaml}"
        );
        assert!(
            yaml.contains("foo:"),
            "registry dep foo must still appear: {yaml}"
        );

        // Sanity: with the flag off, the same graph keeps the link dep.
        let graph_off = LockfileGraph {
            settings: LockfileSettings::default(),
            ..graph
        };
        write(&lockfile_path, &graph_off, &manifest).unwrap();
        let yaml_off = std::fs::read_to_string(&lockfile_path).unwrap();
        assert!(
            yaml_off.contains("sibling:"),
            "with flag off, sibling must reappear: {yaml_off}"
        );
    }

    #[test]
    fn test_parse_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, "{{{{not yaml").unwrap();
        assert!(parse(&path).is_err());
    }

    #[test]
    fn test_parse_nonexistent_file() {
        let path = Path::new("/nonexistent/pnpm-lock.yaml");
        assert!(parse(path).is_err());
    }

    // Byte-parity with a real pnpm-lock.yaml. The fixture was produced by
    // `pnpm install` against a `{ chalk, picocolors, semver }` manifest and
    // lightly pinned — if pnpm's own output format drifts in a future
    // release, regenerate the fixture rather than loosening the assertion.
    // The test guards against silent regressions in the four churn sources
    // we fixed: stray `time:`, block-form `resolution:`, missing blank
    // lines, and dropped `engines:` / `hasBin:`.
    #[test]
    fn test_write_byte_identical_to_native_pnpm() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pnpm-native.yaml");
        // Windows' `core.autocrlf=true` rewrites checked-out files to
        // CRLF even when `.gitattributes` asks for LF; normalize both
        // sides before comparing so a misconfigured checkout gets a
        // meaningful failure rather than a line-ending false positive.
        let original = std::fs::read_to_string(&fixture)
            .unwrap()
            .replace("\r\n", "\n");

        let graph = parse(&fixture).unwrap();
        let manifest = PackageJson {
            name: Some("aube-lockfile-stability".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [
                ("chalk".to_string(), "^4.1.2".to_string()),
                ("picocolors".to_string(), "^1.1.1".to_string()),
                ("semver".to_string(), "^7.6.3".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("pnpm-lock.yaml");
        write(&out, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();

        if written != original {
            // pretty-print a short contextual diff so CI logs are actionable.
            let diff = similar_diff(&original, &written);
            panic!(
                "pnpm writer drifted from native pnpm output:\n{diff}\n\n--- full written output ---\n{written}"
            );
        }
    }

    // Minimal line diff for the byte-parity test failure message. We don't
    // pull in a diff crate just for this — the lockfile is small enough
    // that a line-by-line comparison is readable.
    /// Line-aligned diff with a bounded lookahead so a single
    /// insertion doesn't flag every following line as "modified".
    /// When sides diverge at `(i, j)`, scan up to `LOOKAHEAD` steps in
    /// both directions for the nearest `al[ii] == bl[jj]` and emit the
    /// skipped-over ranges as `- …` / `+ …` runs; that keeps the
    /// failure output readable for the ≤100-line fixtures this test
    /// exercises without pulling in a full LCS dependency.
    fn similar_diff(a: &str, b: &str) -> String {
        const LOOKAHEAD: usize = 8;
        let al: Vec<&str> = a.lines().collect();
        let bl: Vec<&str> = b.lines().collect();
        let mut out = String::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < al.len() || j < bl.len() {
            if i < al.len() && j < bl.len() && al[i] == bl[j] {
                i += 1;
                j += 1;
                continue;
            }
            // Find the nearest resync point within the lookahead
            // window. `k` is the combined distance from `(i, j)`;
            // smaller `k` wins, matching how a developer eyeballs
            // the diff.
            let mut sync: Option<(usize, usize)> = None;
            'outer: for k in 1..=LOOKAHEAD {
                for dx in 0..=k {
                    let dy = k - dx;
                    let ii = i + dx;
                    let jj = j + dy;
                    if ii < al.len() && jj < bl.len() && al[ii] == bl[jj] {
                        sync = Some((ii, jj));
                        break 'outer;
                    }
                }
            }
            match sync {
                Some((ii, jj)) => {
                    for line in &al[i..ii] {
                        out.push_str(&format!("  - {line:?}\n"));
                    }
                    for line in &bl[j..jj] {
                        out.push_str(&format!("  + {line:?}\n"));
                    }
                    i = ii;
                    j = jj;
                }
                None => {
                    // No sync in the window — dump the rest and stop.
                    for line in &al[i..] {
                        out.push_str(&format!("  - {line:?}\n"));
                    }
                    for line in &bl[j..] {
                        out.push_str(&format!("  + {line:?}\n"));
                    }
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn parse_multi_document_lockfile_picks_project_doc() {
        // pnpm v11 emits two YAML documents in one file: a bootstrap
        // doc for `packageManagerDependencies` and the real project
        // lockfile. We want the latter.
        let yaml = r#"---
lockfileVersion: '9.0'

importers:

  .:
    packageManagerDependencies:
      pnpm:
        specifier: 11.0.0-rc.1
        version: 11.0.0-rc.1

packages:

  'pnpm@11.0.0-rc.1':
    resolution: {integrity: sha512-aaa}

snapshots:

  'pnpm@11.0.0-rc.1': {}

---
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true

importers:

  .:
    dependencies:
      lodash:
        specifier: ^4.17.0
        version: 4.17.21

packages:

  'lodash@4.17.21':
    resolution: {integrity: sha512-bbb}

snapshots:

  'lodash@4.17.21': {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, yaml).unwrap();
        let graph = parse(&path).expect("multi-doc lockfile should parse");
        let root = graph.importers.get(".").expect("root importer");
        let names: Vec<_> = root.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.contains(&"lodash"),
            "expected lodash from project doc, got {names:?}"
        );
        assert!(
            !names.contains(&"pnpm"),
            "bootstrap doc's packageManagerDependencies should not leak in, got {names:?}"
        );
    }

    #[test]
    fn snapshot_optional_and_transitive_peer_deps_roundtrip() {
        let yaml = r#"lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
importers:
  .:
    dependencies:
      '@reflink/reflink':
        specifier: ^0.1.19
        version: 0.1.19
      '@babel/generator':
        specifier: ^7.29.1
        version: 7.29.1
packages:
  '@reflink/reflink-darwin-arm64@0.1.19':
    resolution: {integrity: sha512-darwin}
    cpu: [arm64]
    os: [darwin]
  '@reflink/reflink@0.1.19':
    resolution: {integrity: sha512-reflink}
  '@babel/generator@7.29.1':
    resolution: {integrity: sha512-gen}
  '@babel/parser@7.29.2':
    resolution: {integrity: sha512-parser}
snapshots:
  '@reflink/reflink-darwin-arm64@0.1.19':
    optional: true
  '@reflink/reflink@0.1.19':
    optionalDependencies:
      '@reflink/reflink-darwin-arm64': 0.1.19
  '@babel/generator@7.29.1':
    dependencies:
      '@babel/parser': 7.29.2
    transitivePeerDependencies:
      - supports-color
  '@babel/parser@7.29.2': {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, yaml).unwrap();

        let graph = parse(&path).unwrap();
        let darwin = graph
            .packages
            .get("@reflink/reflink-darwin-arm64@0.1.19")
            .expect("darwin snapshot present");
        assert!(darwin.optional, "optional: true must round-trip");

        let generator = graph
            .packages
            .get("@babel/generator@7.29.1")
            .expect("generator snapshot present");
        assert_eq!(
            generator.transitive_peer_dependencies,
            vec!["supports-color".to_string()],
        );

        let parser_pkg = graph.packages.get("@babel/parser@7.29.2").unwrap();
        assert!(!parser_pkg.optional);
        assert!(parser_pkg.transitive_peer_dependencies.is_empty());

        let manifest = PackageJson {
            name: Some("rt".to_string()),
            version: Some("0.0.0".to_string()),
            dependencies: [
                ("@reflink/reflink".to_string(), "^0.1.19".to_string()),
                ("@babel/generator".to_string(), "^7.29.1".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let out_path = dir.path().join("out.yaml");
        write(&out_path, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&out_path).unwrap();

        assert!(
            written.contains("optional: true"),
            "writer must emit optional: true; got:\n{written}"
        );
        assert!(
            written.contains("transitivePeerDependencies:"),
            "writer must emit transitivePeerDependencies; got:\n{written}"
        );
        assert!(
            written.contains("- supports-color"),
            "writer must list bubbled peers; got:\n{written}"
        );

        // Field order within a snapshot must match pnpm's
        // `LockfilePackageSnapshot` emit order so a round-trip stays
        // diff-clean against pnpm's own output: dependencies →
        // optionalDependencies → transitivePeerDependencies → optional.
        // The `@babel/generator` snapshot has `dependencies` followed
        // by `transitivePeerDependencies`, which is the pair Greptile
        // flagged as ordered wrong.
        let deps_line = "\n    dependencies:\n";
        let tpd_line = "\n    transitivePeerDependencies:\n";
        let deps_at = written.find(deps_line).expect("dependencies line emitted");
        let tpd_at = written
            .find(tpd_line)
            .expect("transitivePeerDependencies line emitted");
        assert!(
            deps_at < tpd_at,
            "dependencies must precede transitivePeerDependencies; got:\n{written}"
        );

        let reparsed = parse(&out_path).unwrap();
        assert!(
            reparsed
                .packages
                .get("@reflink/reflink-darwin-arm64@0.1.19")
                .unwrap()
                .optional
        );
        assert_eq!(
            reparsed
                .packages
                .get("@babel/generator@7.29.1")
                .unwrap()
                .transitive_peer_dependencies,
            vec!["supports-color".to_string()]
        );
    }

    #[test]
    fn adversarial_native_pnpm_features_roundtrip_together() {
        let yaml = r#"lockfileVersion: '9.0'

settings:
  autoInstallPeers: false
  excludeLinksFromLockfile: false
  lockfileIncludeTarballUrl: true

overrides:
  is-number: 6.0.0
  react: 'catalog:'

patchedDependencies:
  is-odd@3.0.1:
    path: patches/is-odd@3.0.1.patch
    hash: sha256-deadbeef

catalogs:
  default:
    react:
      specifier: ^18.2.0
      version: 18.2.0
  evens:
    is-even:
      specifier: ^1.0.0
      version: 1.0.0

importers:

  .:
    dependencies:
      odd-alias:
        specifier: npm:is-odd@3.0.1
        version: is-odd@3.0.1
      react:
        specifier: 'catalog:'
        version: 18.2.0
    devDependencies:
      peer-host:
        specifier: 1.0.0
        version: 1.0.0(@types/node@20.11.0)
    optionalDependencies:
      fsevents:
        specifier: ^2.3.3
        version: 2.3.3
    skippedOptionalDependencies:
      optional-native:
        specifier: ^1.0.0
        version: 1.0.0

packages:

  '@types/node@20.11.0':
    resolution: {integrity: sha512-types}

  fsevents@2.3.3:
    resolution: {integrity: sha512-fsevents, tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz}
    os: [darwin]
    cpu: [x64]

  is-number@6.0.0:
    resolution: {integrity: sha512-number}

  is-odd@3.0.1:
    resolution: {integrity: sha512-odd, tarball: https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz}

  peer-host@1.0.0(@types/node@20.11.0):
    resolution: {integrity: sha512-peer}
    peerDependencies:
      '@types/node': '>=20'
    peerDependenciesMeta:
      '@types/node':
        optional: true

  react@18.2.0:
    resolution: {integrity: sha512-react}

ignoredOptionalDependencies:
  - optional-native

snapshots:

  '@types/node@20.11.0': {}

  fsevents@2.3.3:
    optional: true

  is-number@6.0.0: {}

  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0
    transitivePeerDependencies:
      - '@types/node'

  peer-host@1.0.0(@types/node@20.11.0): {}

  react@18.2.0: {}
"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(&path, yaml).unwrap();

        let graph = parse(&path).unwrap();

        assert!(!graph.settings.auto_install_peers);
        assert!(graph.settings.lockfile_include_tarball_url);
        assert_eq!(graph.overrides.get("react").unwrap(), "catalog:");
        assert_eq!(
            graph.patched_dependencies.get("is-odd@3.0.1").unwrap(),
            "patches/is-odd@3.0.1.patch"
        );
        assert_eq!(
            graph.catalogs["evens"]["is-even"].specifier, "^1.0.0",
            "named catalogs must survive parse"
        );
        assert!(
            graph
                .ignored_optional_dependencies
                .contains("optional-native")
        );
        assert_eq!(
            graph.skipped_optional_dependencies["."]["optional-native"],
            "^1.0.0"
        );

        let root = graph.importers.get(".").expect("root importer");
        let alias_dep = root.iter().find(|d| d.name == "odd-alias").unwrap();
        assert_eq!(alias_dep.dep_path, "odd-alias@3.0.1");
        assert_eq!(alias_dep.specifier.as_deref(), Some("npm:is-odd@3.0.1"));
        let peer_dep = root.iter().find(|d| d.name == "peer-host").unwrap();
        assert_eq!(peer_dep.dep_type, DepType::Dev);
        let optional_dep = root.iter().find(|d| d.name == "fsevents").unwrap();
        assert_eq!(optional_dep.dep_type, DepType::Optional);

        let alias_pkg = graph.packages.get("odd-alias@3.0.1").unwrap();
        assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
        assert_eq!(
            alias_pkg
                .transitive_peer_dependencies
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["@types/node"]
        );
        let fsevents = graph.packages.get("fsevents@2.3.3").unwrap();
        assert!(fsevents.optional);
        assert_eq!(fsevents.os.as_slice(), ["darwin"]);
        assert_eq!(fsevents.cpu.as_slice(), ["x64"]);
        assert_eq!(
            fsevents.tarball_url.as_deref(),
            Some("https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz")
        );
        let peer_host = graph
            .packages
            .get("peer-host@1.0.0(@types/node@20.11.0)")
            .unwrap();
        assert_eq!(peer_host.peer_dependencies["@types/node"], ">=20");
        assert!(peer_host.peer_dependencies_meta["@types/node"].optional);

        let manifest = PackageJson {
            name: Some("adversarial-native-pnpm".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [
                ("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string()),
                ("react".to_string(), "catalog:".to_string()),
            ]
            .into_iter()
            .collect(),
            dev_dependencies: [("peer-host".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("fsevents".to_string(), "^2.3.3".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = dir.path().join("out.yaml");
        write(&out, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();

        for needle in [
            "lockfileIncludeTarballUrl: true",
            "overrides:",
            "patchedDependencies:",
            "catalogs:",
            "skippedOptionalDependencies:",
            "ignoredOptionalDependencies:",
            "aliasOf: is-odd",
            "peerDependencies:",
            "peerDependenciesMeta:",
            "transitivePeerDependencies:",
            "optional: true",
            "tarball: https://registry.npmjs.org/fsevents/-/fsevents-2.3.3.tgz",
        ] {
            assert!(
                written.contains(needle),
                "missing {needle:?} in:\n{written}"
            );
        }

        let overrides_at = written.find("\noverrides:").expect("overrides");
        let patched_at = written
            .find("\npatchedDependencies:")
            .expect("patchedDependencies");
        let catalogs_at = written.find("\ncatalogs:").expect("catalogs");
        let importers_at = written.find("\nimporters:").expect("importers");
        assert!(
            overrides_at < patched_at && patched_at < catalogs_at && catalogs_at < importers_at,
            "pnpm top-level section order drifted:\n{written}"
        );
        let packages_at = written.find("\npackages:").expect("packages");
        let ignored_at = written
            .find("\nignoredOptionalDependencies:")
            .expect("ignored optional");
        let snapshots_at = written.find("\nsnapshots:").expect("snapshots");
        assert!(
            packages_at < ignored_at && ignored_at < snapshots_at,
            "ignoredOptionalDependencies must stay between packages and snapshots:\n{written}"
        );

        let reparsed = parse(&out).unwrap();
        assert_eq!(
            reparsed
                .patched_dependencies
                .get("is-odd@3.0.1")
                .unwrap_or_else(|| panic!("patched deps lost after reparse:\n{written}")),
            "patches/is-odd@3.0.1.patch"
        );
        assert_eq!(reparsed.catalogs["default"]["react"].version, "18.2.0");
        assert_eq!(
            reparsed
                .packages
                .get("odd-alias@3.0.1")
                .unwrap_or_else(|| panic!("alias package lost after reparse:\n{written}"))
                .alias_of
                .as_deref(),
            Some("is-odd")
        );
        assert!(reparsed.packages.get("fsevents@2.3.3").unwrap().optional);
        assert_eq!(
            reparsed.skipped_optional_dependencies["."]["optional-native"],
            "^1.0.0"
        );
    }

    #[test]
    fn write_pnpm_lockfile_uses_native_alias_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        let manifest = PackageJson {
            name: Some("alias-native-pnpm".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let graph = LockfileGraph {
            importers: [(
                ".".to_string(),
                vec![DirectDep {
                    name: "odd-alias".to_string(),
                    dep_path: "odd-alias@3.0.1".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("npm:is-odd@3.0.1".to_string()),
                }],
            )]
            .into_iter()
            .collect(),
            packages: [
                (
                    "odd-alias@3.0.1".to_string(),
                    LockedPackage {
                        name: "odd-alias".to_string(),
                        version: "3.0.1".to_string(),
                        integrity: Some("sha512-odd".to_string()),
                        dep_path: "odd-alias@3.0.1".to_string(),
                        alias_of: Some("is-odd".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    "consumer@1.0.0".to_string(),
                    LockedPackage {
                        name: "consumer".to_string(),
                        version: "1.0.0".to_string(),
                        integrity: Some("sha512-consumer".to_string()),
                        dep_path: "consumer@1.0.0".to_string(),
                        dependencies: [(
                            "odd-alias".to_string(),
                            "3.0.1(peer-host@1.0.0)".to_string(),
                        )]
                        .into_iter()
                        .collect(),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        write(&path, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("version: is-odd@3.0.1"), "{written}");
        assert!(written.contains("is-odd@3.0.1:"), "{written}");
        assert!(
            written.contains("odd-alias: is-odd@3.0.1(peer-host@1.0.0)"),
            "{written}"
        );
        assert!(!written.contains("aliasOf:"), "{written}");

        let reparsed = parse(&path).unwrap();
        let alias_pkg = reparsed.packages.get("odd-alias@3.0.1").unwrap();
        assert_eq!(alias_pkg.alias_of.as_deref(), Some("is-odd"));
    }

    #[test]
    fn parse_synthesizes_npm_alias_from_pnpm_v9_lockfile() {
        // pnpm v9 encodes npm-aliases implicitly (importer key is the
        // alias, `version:` is `<real>@<resolved>`, no `aliasOf:`
        // field on the package entry). The reader must reconstruct
        // an alias-keyed LockedPackage with `alias_of=Some(real)` so
        // the linker creates `node_modules/<alias>` correctly.
        // Repro: https://github.com/rubnogueira/aube-exotic-bug
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      express-fork:
        specifier: npm:express@^4.22.1
        version: express@4.22.1

packages:
  express@4.22.1:
    resolution: {integrity: sha512-fake}
    engines: {node: '>= 0.10.0'}

snapshots:
  express@4.22.1: {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();

        let root = graph.importers.get(".").expect("root importer");
        assert_eq!(root.len(), 1);
        let dep = &root[0];
        assert_eq!(dep.name, "express-fork", "DirectDep keeps the alias name");
        assert_eq!(
            dep.dep_path, "express-fork@4.22.1",
            "DirectDep dep_path is alias-keyed (not the malformed express-fork@express@4.22.1)"
        );
        assert_eq!(dep.specifier.as_deref(), Some("npm:express@^4.22.1"));

        let pkg = graph
            .packages
            .get("express-fork@4.22.1")
            .expect("synthesized alias-keyed package");
        assert_eq!(pkg.name, "express-fork");
        assert_eq!(pkg.alias_of.as_deref(), Some("express"));
        assert_eq!(pkg.dep_path, "express-fork@4.22.1");
        // Real-keyed entry stays in place — other importers may
        // reference the package directly, and the canonical entry is
        // needed for byte-identical round-trips back to pnpm format.
        let real = graph.packages.get("express@4.22.1").expect("real entry");
        assert_eq!(real.name, "express");
        assert!(real.alias_of.is_none());
    }

    #[test]
    fn parse_synthesizes_npm_alias_from_pnpm_lockfile_catalog_specifier() {
        // pnpm-resolved catalog aliases keep `specifier: 'catalog:'`
        // in the importer block while the `version:` field already
        // carries the resolved alias (`<real>@<resolved>`). The
        // reader must detect the alias from the version shape alone
        // — gating on `specifier.starts_with("npm:")` would silently
        // drop the dep and leave node_modules empty.
        // Repro:
        //   https://github.com/endevco/aube/discussions/383#discussioncomment-16759640
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

catalogs:
  default:
    beamcoder:
      specifier: npm:beamcoder-prebuild@0.7.1-rc.18
      version: 0.7.1-rc.18

importers:
  packages/app:
    dependencies:
      beamcoder:
        specifier: 'catalog:'
        version: beamcoder-prebuild@0.7.1-rc.18

packages:
  beamcoder-prebuild@0.7.1-rc.18:
    resolution: {integrity: sha512-fake}

snapshots:
  beamcoder-prebuild@0.7.1-rc.18: {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();
        let app = graph
            .importers
            .get("packages/app")
            .expect("packages/app importer");
        assert_eq!(app.len(), 1, "alias-resolved catalog dep must be parsed");
        let dep = &app[0];
        assert_eq!(dep.name, "beamcoder", "DirectDep keeps the alias name");
        assert_eq!(
            dep.dep_path, "beamcoder@0.7.1-rc.18",
            "DirectDep dep_path is alias-keyed"
        );
        assert_eq!(dep.specifier.as_deref(), Some("catalog:"));

        let pkg = graph
            .packages
            .get("beamcoder@0.7.1-rc.18")
            .expect("synthesized alias-keyed package");
        assert_eq!(pkg.name, "beamcoder");
        assert_eq!(pkg.alias_of.as_deref(), Some("beamcoder-prebuild"));
    }

    #[test]
    fn parse_synthesizes_npm_alias_when_real_name_is_scoped() {
        // Scoped real package + non-scoped alias: `parse_dep_path` must
        // correctly split `@scope/pkg` from the version when the
        // version field is `@scope/pkg@1.0.0`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      types-alias:
        specifier: npm:@types/node@^20.0.0
        version: '@types/node@20.11.0'

packages:
  '@types/node@20.11.0':
    resolution: {integrity: sha512-fake}

snapshots:
  '@types/node@20.11.0': {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();

        let root = graph.importers.get(".").expect("root importer");
        assert_eq!(root[0].name, "types-alias");
        assert_eq!(root[0].dep_path, "types-alias@20.11.0");

        let pkg = graph
            .packages
            .get("types-alias@20.11.0")
            .expect("synthesized alias-keyed package");
        assert_eq!(pkg.name, "types-alias");
        assert_eq!(pkg.alias_of.as_deref(), Some("@types/node"));
        let real = graph
            .packages
            .get("@types/node@20.11.0")
            .expect("real entry");
        assert_eq!(real.name, "@types/node");
        assert!(real.alias_of.is_none());
    }

    #[test]
    fn parse_synthesizes_npm_alias_for_transitive_deps() {
        // pnpm encodes npm-aliased *transitive* deps as
        // `<alias>: <real>@<resolved>` inside a snapshot's
        // dependencies map (e.g. `@isaacs/cliui@8.0.2` declares
        // `"string-width-cjs": "npm:string-width@^4.2.0"` and
        // pnpm resolves it as `string-width-cjs: string-width@4.2.3`).
        // The reader must rewrite the dep value to the resolved
        // version and synthesize the alias-keyed package entry, or
        // the linker creates a broken symlink to a non-existent
        // `string-width-cjs@string-width@4.2.3` virtual store dir
        // and the resolver's lockfile-reuse path enqueues a
        // transitive task with a malformed `string-width@4.2.3`
        // range that no string-width-cjs version can satisfy.
        // Repro: https://github.com/stevelandeydescript/aube-bug-repros/tree/main/npm-alias-resolution-failure
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      jackspeak:
        specifier: 4.1.1
        version: 4.1.1

packages:
  '@isaacs/cliui@8.0.2':
    resolution: {integrity: sha512-fake}
  jackspeak@4.1.1:
    resolution: {integrity: sha512-fake}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}
  string-width@5.1.2:
    resolution: {integrity: sha512-fake}

snapshots:
  '@isaacs/cliui@8.0.2':
    dependencies:
      string-width: 5.1.2
      string-width-cjs: string-width@4.2.3
  jackspeak@4.1.1:
    dependencies:
      '@isaacs/cliui': 8.0.2
  string-width@4.2.3: {}
  string-width@5.1.2: {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();

        let cliui = graph
            .packages
            .get("@isaacs/cliui@8.0.2")
            .expect("cliui entry");
        assert_eq!(
            cliui.dependencies.get("string-width-cjs").unwrap(),
            "4.2.3",
            "transitive alias dep value rewritten from `string-width@4.2.3` to bare `4.2.3`"
        );
        assert_eq!(cliui.dependencies.get("string-width").unwrap(), "5.1.2");

        let alias = graph
            .packages
            .get("string-width-cjs@4.2.3")
            .expect("synthesized alias-keyed package for transitive");
        assert_eq!(alias.name, "string-width-cjs");
        assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
        assert_eq!(alias.dep_path, "string-width-cjs@4.2.3");

        let real = graph
            .packages
            .get("string-width@4.2.3")
            .expect("real entry stays put");
        assert_eq!(real.name, "string-width");
        assert!(real.alias_of.is_none());
    }

    #[test]
    fn parse_handles_npm_alias_for_transitive_deps_with_peer_suffix() {
        // Aliased transitive whose alias target carries a peer
        // suffix: `<alias>: <real>@<resolved>(peer@ver)`. The
        // peer-context tail must follow through to the synthetic
        // alias dep_path so the linker keys the same context.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      parent-pkg:
        specifier: 1.0.0
        version: 1.0.0

packages:
  parent-pkg@1.0.0:
    resolution: {integrity: sha512-fake}
  real-pkg@2.0.0:
    resolution: {integrity: sha512-fake}
  peer-pkg@3.0.0:
    resolution: {integrity: sha512-fake}

snapshots:
  parent-pkg@1.0.0:
    dependencies:
      alias-pkg: real-pkg@2.0.0(peer-pkg@3.0.0)
  real-pkg@2.0.0(peer-pkg@3.0.0):
    dependencies:
      peer-pkg: 3.0.0
  peer-pkg@3.0.0: {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();
        let parent = graph.packages.get("parent-pkg@1.0.0").expect("parent");
        assert_eq!(
            parent.dependencies.get("alias-pkg").unwrap(),
            "2.0.0(peer-pkg@3.0.0)",
            "peer-context suffix preserved on the rewritten alias dep value"
        );
        let alias = graph
            .packages
            .get("alias-pkg@2.0.0(peer-pkg@3.0.0)")
            .expect("synthesized alias entry with peer suffix");
        assert_eq!(alias.name, "alias-pkg");
        assert_eq!(alias.alias_of.as_deref(), Some("real-pkg"));
    }

    #[test]
    fn parse_synthesizes_npm_alias_for_transitive_deps_of_local_packages() {
        // The local-packages absorption loop runs before the main
        // snapshot loop and pulls a `file:` workspace package's
        // transitive deps directly out of `raw.snapshots`. Those
        // values must go through the same alias rewrite as the main
        // path, or a workspace package depending on
        // `"string-width-cjs": "npm:string-width@^4.2.0"` would still
        // produce the broken `string-width-cjs@string-width@4.2.3`
        // virtual store path on install.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-lock.yaml");
        std::fs::write(
            &path,
            r#"
lockfileVersion: '9.0'

importers:
  .:
    dependencies:
      local-pkg:
        specifier: file:./local-pkg
        version: file:./local-pkg

packages:
  local-pkg@file:./local-pkg:
    resolution: {directory: ./local-pkg, type: directory}
  string-width@4.2.3:
    resolution: {integrity: sha512-fake}

snapshots:
  local-pkg@file:./local-pkg:
    dependencies:
      string-width-cjs: string-width@4.2.3
  string-width@4.2.3: {}
"#,
        )
        .unwrap();

        let graph = parse(&path).unwrap();
        let local = graph
            .packages
            .values()
            .find(|p| p.name == "local-pkg")
            .expect("local-pkg entry");
        assert_eq!(
            local.dependencies.get("string-width-cjs").unwrap(),
            "4.2.3",
            "transitive alias on a local package gets rewritten too"
        );
        let alias = graph
            .packages
            .get("string-width-cjs@4.2.3")
            .expect("synthesized alias entry from local package's transitive");
        assert_eq!(alias.name, "string-width-cjs");
        assert_eq!(alias.alias_of.as_deref(), Some("string-width"));
    }
}
