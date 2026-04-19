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
    let content = std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let raw: RawPnpmLockfile = serde_yaml::from_str(&content)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;

    // Parse importers (direct deps of each workspace package).
    // We track synthesized LockedPackages for local (`file:` / `link:`)
    // deps here so the main packages loop below doesn't try to process
    // them off the canonical lockfile key.
    let mut importers = BTreeMap::new();
    let mut local_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
        BTreeMap::new();

    let mut push_direct =
        |deps: &mut Vec<DirectDep>, name: &str, info: &RawDepSpec, dep_type: DepType| {
            if let Some(local) = LocalSource::parse(&info.version, Path::new("")) {
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
                deps.push(DirectDep {
                    name: name.to_string(),
                    dep_path: version_to_dep_path(name, &info.version),
                    dep_type,
                    specifier: Some(info.specifier.clone()),
                });
            }
        };

    for (importer_path, importer) in &raw.importers {
        let mut deps = Vec::new();

        if let Some(ref d) = importer.dependencies {
            for (name, info) in d {
                push_direct(&mut deps, name, info, DepType::Production);
            }
        }
        if let Some(ref d) = importer.dev_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, name, info, DepType::Dev);
            }
        }
        if let Some(ref d) = importer.optional_dependencies {
            for (name, info) in d {
                push_direct(&mut deps, name, info, DepType::Optional);
            }
        }

        if let Some(ref d) = importer.skipped_optional_dependencies
            && !d.is_empty()
        {
            let mut map = BTreeMap::new();
            for (name, info) in d {
                map.insert(name.clone(), info.specifier.clone());
            }
            skipped_optional_dependencies.insert(importer_path.clone(), map);
        }

        importers.insert(importer_path.clone(), deps);
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
            if let Some(snap) = raw.snapshots.get(&canonical)
                && let Some(deps) = snap.dependencies.clone()
            {
                local_pkg.dependencies = deps;
            }
            if let Some(snap) = raw.snapshots.get(&canonical)
                && let Some(opt_deps) = snap.optional_dependencies.clone()
            {
                local_pkg.dependencies.extend(opt_deps.clone());
                local_pkg.optional_dependencies = opt_deps;
            }
            if let Some(pkg_info) = raw.packages.get(&canonical)
                && let Some(ref res) = pkg_info.resolution
            {
                // Prefer the authoritative LocalSource classification
                // from the `resolution:` block over the guess the
                // importers loop made from the bare specifier.
                if let Some(ref tb) = res.tarball {
                    if let Some(rel) = tb.strip_prefix("file:") {
                        local_pkg.local_source =
                            Some(LocalSource::Tarball(std::path::PathBuf::from(rel)));
                    } else if tb.starts_with("http://") || tb.starts_with("https://") {
                        local_pkg.local_source =
                            Some(LocalSource::RemoteTarball(crate::RemoteTarballSource {
                                url: tb.clone(),
                                integrity: res.integrity.clone().unwrap_or_default(),
                            }));
                    }
                } else if let Some(ref dir) = res.directory {
                    local_pkg.local_source =
                        Some(LocalSource::Directory(std::path::PathBuf::from(dir)));
                } else if let (Some(repo), Some(commit)) = (res.repo.as_ref(), res.commit.as_ref())
                {
                    local_pkg.local_source = Some(LocalSource::Git(GitSource {
                        url: repo.clone(),
                        committish: None,
                        resolved: commit.clone(),
                    }));
                }
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
        let (name, version) = parse_dep_path(&dep_path).ok_or_else(|| {
            Error::Parse(path.to_path_buf(), format!("invalid dep path: {dep_path}"))
        })?;

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
        let tarball_url = pkg_info
            .and_then(|p| p.resolution.as_ref())
            .and_then(|r| r.tarball.as_ref())
            .filter(|t| t.starts_with("http://") || t.starts_with("https://"))
            .cloned();

        let dependencies = raw
            .snapshots
            .get(&dep_path)
            .and_then(|s| s.dependencies.clone())
            .unwrap_or_default();
        let optional_dependencies = raw
            .snapshots
            .get(&dep_path)
            .and_then(|s| s.optional_dependencies.clone())
            .unwrap_or_default();
        let mut dependencies = dependencies;
        dependencies.extend(optional_dependencies.clone());

        let bundled_dependencies = raw
            .snapshots
            .get(&dep_path)
            .and_then(|s| s.bundled_dependencies.clone())
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
        let has_bin = pkg_info.map(|p| p.has_bin).unwrap_or(false);
        // Aube-specific extension (see `WritablePackageInfo::alias_of`)
        // — ordinary pnpm lockfiles never carry it, so this stays
        // `None` on pnpm-authored input and round-trips the resolver-
        // emitted value for aliased packages.
        let alias_of = pkg_info.and_then(|p| p.alias_of.clone());

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
                local_source: None,
                os,
                cpu,
                libc,
                bundled_dependencies,
                tarball_url,
                alias_of,
                yarn_checksum: None,
                engines,
                has_bin,
            },
        );
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
    })
}

/// Write a LockfileGraph as pnpm-lock.yaml v9 format.
pub fn write(path: &Path, graph: &LockfileGraph, manifest: &PackageJson) -> Result<(), Error> {
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
            //   1. The manifest entry for this dep (what the user wrote
            //      in their own package.json — authoritative for user-
            //      declared deps).
            //   2. The specifier recorded on the DirectDep — used for
            //      hoisted auto-installed peers, where the manifest has
            //      nothing and the resolver synthesized the DirectDep
            //      using the peer's declared range.
            //   3. Fall back to `*` as a last resort.
            let manifest_specifier = match dep.dep_type {
                DepType::Production => manifest.dependencies.get(&dep.name),
                DepType::Dev => manifest.dev_dependencies.get(&dep.name),
                DepType::Optional => manifest.optional_dependencies.get(&dep.name),
            }
            .map(|s| s.as_str());
            let specifier = manifest_specifier
                .or(dep.specifier.as_deref())
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
        let canonical = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => version_to_dep_path(&pkg.name, &pkg.version),
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
            }),
            Some(local @ LocalSource::Tarball(_)) => Some(WritableResolution {
                integrity: None,
                directory: None,
                tarball: Some(format!("file:{}", local.path_posix())),
                commit: None,
                repo: None,
                type_: None,
            }),
            Some(LocalSource::Link(_)) => None,
            Some(LocalSource::Git(g)) => Some(WritableResolution {
                integrity: None,
                directory: None,
                tarball: None,
                commit: Some(g.resolved.clone()),
                repo: Some(g.url.clone()),
                type_: Some("git".to_string()),
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
            }),
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
            }),
        };
        packages.insert(
            canonical,
            WritablePackageInfo {
                resolution,
                engines: if pkg.engines.is_empty() {
                    None
                } else {
                    Some(pkg.engines.clone())
                },
                os: pkg.os.clone(),
                cpu: pkg.cpu.clone(),
                libc: pkg.libc.clone(),
                has_bin: pkg.has_bin,
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_meta,
                // Preserve the alias→real-name mapping so a subsequent
                // install from this lockfile still hits the real
                // registry instead of re-404ing on the alias-qualified
                // tarball URL.
                alias_of: pkg.alias_of.clone(),
            },
        );
    }

    let mut snapshots = BTreeMap::new();
    for (dep_path, pkg) in &graph.packages {
        // `link:` deps are omitted from snapshots (pnpm parity); other
        // local deps use the canonical specifier key so pnpm's parser
        // lines them up with the packages entry above.
        let key = match pkg.local_source.as_ref() {
            Some(LocalSource::Link(_)) => continue,
            Some(local) => format!("{}@{}", pkg.name, local.specifier()),
            None => dep_path.clone(),
        };
        snapshots.insert(
            key,
            WritableSnapshot {
                dependencies: {
                    let mut deps = pkg.dependencies.clone();
                    for name in pkg.optional_dependencies.keys() {
                        deps.remove(name);
                    }
                    if deps.is_empty() { None } else { Some(deps) }
                },
                optional_dependencies: if pkg.optional_dependencies.is_empty() {
                    None
                } else {
                    Some(pkg.optional_dependencies.clone())
                },
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
        time,
        importers,
        packages,
        snapshots,
    };

    let yaml = serde_yaml::to_string(&lockfile)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
    let yaml = reformat_for_pnpm_parity(&yaml);
    std::fs::write(path, yaml).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    Ok(())
}

/// Post-process a `serde_yaml`-emitted pnpm-lock.yaml into the exact
/// shape real pnpm writes. Two tweaks:
///
///   1. Collapse `resolution:` / `engines:` block maps into flow form
///      (`resolution: {integrity: sha512-…}`). pnpm writes both inline
///      and `serde_yaml` can't be coerced into flow style per-field
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

// -- Writable serde types for pnpm-lock.yaml v9 --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WritablePnpmLockfile {
    lockfile_version: String,
    settings: WritableSettings,
    // pnpm v9 places `overrides:` immediately after `settings:` and
    // before `importers:`. Field order matters because we serialize
    // through serde_yaml and want byte-for-byte parity with pnpm output
    // for the no-overrides case (the field is skipped when empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    overrides: Option<BTreeMap<String, String>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_dependencies: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundled_dependencies: Option<Vec<String>>,
}

// -- Raw serde types for pnpm-lock.yaml v9 (deserialization) --

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPnpmLockfile {
    #[allow(dead_code)]
    lockfile_version: serde_yaml::Value,
    #[serde(default)]
    settings: Option<RawSettings>,
    #[serde(default)]
    overrides: Option<BTreeMap<String, String>>,
    #[serde(default)]
    catalogs: Option<BTreeMap<String, BTreeMap<String, RawCatalogEntry>>>,
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
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    #[serde(default)]
    has_bin: bool,
    /// Paired writer field. See `WritablePackageInfo::alias_of`. `None`
    /// for ordinary (non-aliased) packages.
    #[serde(default)]
    alias_of: Option<String>,
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
        let original = std::fs::read_to_string(&fixture).unwrap();

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
    fn similar_diff(a: &str, b: &str) -> String {
        let al: Vec<&str> = a.lines().collect();
        let bl: Vec<&str> = b.lines().collect();
        let mut out = String::new();
        let max = al.len().max(bl.len());
        for i in 0..max {
            match (al.get(i), bl.get(i)) {
                (Some(x), Some(y)) if x == y => {}
                (Some(x), Some(y)) => {
                    out.push_str(&format!(
                        "  line {}: expected {x:?}\n           got      {y:?}\n",
                        i + 1
                    ));
                }
                (Some(x), None) => {
                    out.push_str(&format!("  line {}: missing (expected {x:?})\n", i + 1))
                }
                (None, Some(y)) => out.push_str(&format!("  line {}: extra    ({y:?})\n", i + 1)),
                (None, None) => {}
            }
        }
        out
    }
}
