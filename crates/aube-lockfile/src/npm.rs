//! Reader and writer for npm's package-lock.json (v2/v3) and npm-shrinkwrap.json.
//!
//! The v2/v3 format uses a flat `packages` map keyed by install path:
//! - `""` is the root project
//! - `"node_modules/foo"` is a top-level dep
//! - `"node_modules/foo/node_modules/bar"` is a nested dep
//!
//! Each entry carries `version`, `integrity`, `dependencies`, `dev`,
//! `optional`, etc. On read, we flatten into one `LockedPackage` per
//! unique `(name, version)` pair, discarding the nesting (aube uses a
//! hoisted virtual store layout). On write, we walk the flat graph and
//! rebuild a hoist + nest layout so consumers (npm, aube's own parser)
//! get a valid v3 package-lock.json back.
//!
//! v1 lockfiles (npm 5-6, uses nested `dependencies` tree) are rejected.

use crate::{DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct RawNpmLockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    #[serde(default)]
    packages: BTreeMap<String, RawNpmPackage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNpmPackage {
    /// npm emits this field only when the entry is an npm-alias
    /// (`"h3-v2": "npm:h3@..."` resolves to `node_modules/h3-v2` with
    /// `name: "h3"`). For non-aliased packages the name is recoverable
    /// from the install path and npm omits the field. We use the
    /// presence of this field — combined with inequality against the
    /// install-path segment — to detect aliases.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    /// Full registry tarball URL npm wrote when it locked this entry.
    /// We capture it so aliased packages (whose registry name differs
    /// from the install-path-derived name used to key the graph) don't
    /// need to re-derive the URL from the registry base — and so we
    /// can round-trip `resolved:` faithfully when we write back.
    #[serde(default)]
    resolved: Option<String>,
    #[serde(default)]
    link: bool,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    /// npm v7+ records `peerDependencies` verbatim on each package
    /// entry (pulled straight from the package's own `package.json`
    /// at lockfile-write time). The flat npm layout relies on peers
    /// being auto-installed into *some* ancestor `node_modules/` so
    /// Node's upward walk finds them, but aube's isolated layout
    /// wants them as explicit siblings — without this field, the
    /// resolver's peer-context pass has nothing to work with on the
    /// lockfile-driven install path and peers silently go missing
    /// from `.aube/<dep_path>/node_modules/`.
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    peer_dependencies_meta: BTreeMap<String, RawNpmPeerDepMeta>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    os: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    cpu: Vec<String>,
    #[serde(default, deserialize_with = "aube_util::string_or_seq")]
    libc: Vec<String>,
    /// Captured verbatim for round-trip. npm writes these on every
    /// package entry; dropping them on re-emit is one of the
    /// remaining sources of `aube install --no-frozen-lockfile`
    /// churn against native npm output.
    ///
    /// Uses `aube_manifest::engines_tolerant` so the legacy array
    /// shape (e.g. `ansi-html-community@0.0.8` ships
    /// `"engines": ["node >= 0.8.0"]` and npm preserves it verbatim
    /// in the lockfile) doesn't blow up the whole parse. We normalize
    /// the array to an empty map — same behavior modern npm gives the
    /// shape for engine-strict checks, and the same tolerance the
    /// manifest parser already applies.
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    engines: BTreeMap<String, String>,
    #[serde(default)]
    bin: BTreeMap<String, String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    funding: Option<RawNpmFunding>,
}

#[derive(Clone)]
struct InstallPathInfo {
    name: String,
    dep_path: String,
}

/// npm's `funding:` block on a package entry. npm copies the field
/// verbatim from the package's `package.json`, which means all three
/// shapes the registry permits show up in real lockfiles:
///
/// 1. bare URL string: `"funding": "https://example.com/sponsor"`
/// 2. object: `"funding": {"url": "…", "type": "github"}`
/// 3. mixed array: `"funding": ["https://…", {"url": "…"}]`
///
/// Aube only carries a single `funding_url: Option<String>` on
/// `LockedPackage`, so on read we collapse to the first URL we find;
/// on write we always emit the single-key `{"url": …}` form (which
/// npm itself accepts on a re-read).
#[derive(Debug, Clone, Default)]
struct RawNpmFunding {
    url: Option<String>,
}

impl<'de> Deserialize<'de> for RawNpmFunding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        use std::fmt;

        struct FundingVisitor;

        impl<'de> Visitor<'de> for FundingVisitor {
            type Value = RawNpmFunding;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a funding URL string, a {url: ...} object, or an array of either")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmFunding {
                    url: Some(v.to_owned()),
                })
            }

            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(RawNpmFunding { url: Some(v) })
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut url: Option<String> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "url" {
                        url = map.next_value::<Option<String>>()?;
                    } else {
                        // Skip unknown fields (e.g. `type`).
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(RawNpmFunding { url })
            }

            fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
            where
                S: SeqAccess<'de>,
            {
                // Pick the first usable URL from the array; aube's
                // single-URL model can't represent a list. Drain the
                // rest so the deserializer state stays consistent.
                let mut chosen: Option<String> = None;
                while let Some(item) = seq.next_element::<RawNpmFunding>()? {
                    if chosen.is_none() {
                        chosen = item.url;
                    }
                }
                Ok(RawNpmFunding { url: chosen })
            }
        }

        deserializer.deserialize_any(FundingVisitor)
    }
}

/// `peerDependenciesMeta` value — only `optional` is meaningful to
/// us today (matches pnpm's model). Other fields that might appear
/// (`description`, etc.) are preserved only as far as serde's
/// `deny_unknown_fields` stays off.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawNpmPeerDepMeta {
    #[serde(default)]
    optional: bool,
}

/// Parse a package-lock.json or npm-shrinkwrap.json file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let content = crate::read_lockfile(path)?;
    let raw: RawNpmLockfile = crate::parse_json(path, content)?;

    if raw.lockfile_version < 2 {
        return Err(Error::parse(
            path,
            format!(
                "package-lock.json lockfileVersion {} is not supported (need v2 or v3)",
                raw.lockfile_version
            ),
        ));
    }

    let mut graph = LockfileGraph {
        importers: BTreeMap::new(),
        packages: BTreeMap::new(),
        ..Default::default()
    };

    // npm workspace links come in pairs:
    // - `node_modules/@scope/pkg: { resolved: "packages/pkg", link: true }`
    // - `packages/pkg: { name, version, dependencies, ... }`
    //
    // The `node_modules/` entry is the actual edge consumers resolve through;
    // the target path entry carries the package metadata. Skip the target-path
    // record during the main loop and let the link entry synthesize a local
    // package from it.
    let link_targets: BTreeSet<String> = raw
        .packages
        .values()
        .filter_map(|entry| entry.link.then(|| entry.resolved.clone()).flatten())
        .collect();

    // Map each install_path to the locked dep_path it resolves to. We need
    // this for the nested-resolution walk, including local/workspace links
    // whose dep_path isn't just `name@version`.
    let mut install_path_info: BTreeMap<String, InstallPathInfo> = BTreeMap::new();

    for (install_path, entry) in &raw.packages {
        if install_path.is_empty() {
            continue; // root project, handled separately
        }
        if link_targets.contains(install_path) {
            continue;
        }

        // The install-path segment is what every other package in the
        // tree refers to. For non-aliased deps that's the real package
        // name; for `"h3-v2": "npm:h3@..."` it's the alias `h3-v2`.
        // Keep it as the LockedPackage.name so the linker drops the
        // dep into `node_modules/<alias>/` and transitive symlinks
        // resolve by the string that appears in consumers'
        // `dependencies` maps.
        let install_name = package_name_from_install_path(install_path)
            .or_else(|| entry.name.clone())
            .ok_or_else(|| {
                Error::parse(
                    path,
                    format!("could not determine package name for '{install_path}'"),
                )
            })?;
        // npm writes `name:` only for aliases. If present and different
        // from the install-path segment, this is `"<alias>": "npm:<real>@..."`
        // and the real name is what we hit the registry with. If absent
        // or equal, it's a regular dep.
        let alias_of = entry
            .name
            .as_ref()
            .filter(|real| real.as_str() != install_name.as_str())
            .cloned();
        let (package_entry, version, dep_path, local_source) = if entry.link {
            let target = entry.resolved.as_ref().ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' has no resolved target"),
                )
            })?;
            let target_entry = raw.packages.get(target).ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' points to missing target '{target}'"),
                )
            })?;
            let version = target_entry.version.clone().ok_or_else(|| {
                Error::parse(
                    path,
                    format!("linked package '{install_name}' target '{target}' has no version"),
                )
            })?;
            let local = LocalSource::Link(PathBuf::from(target));
            (
                target_entry,
                version,
                local.dep_path(&install_name),
                Some(local),
            )
        } else {
            let version = entry.version.clone().ok_or_else(|| {
                Error::parse(path, format!("package '{install_name}' has no version"))
            })?;
            let local_source = entry
                .resolved
                .as_deref()
                .and_then(local_git_source_from_resolved);
            let dep_path = local_source.as_ref().map_or_else(
                || format!("{install_name}@{version}"),
                |l| l.dep_path(&install_name),
            );
            (entry, version.clone(), dep_path, local_source)
        };
        install_path_info.insert(
            install_path.clone(),
            InstallPathInfo {
                name: install_name.clone(),
                dep_path: dep_path.clone(),
            },
        );

        // Same (name, version) may appear at multiple nest levels; keep the first occurrence.
        if graph.packages.contains_key(&dep_path) {
            continue;
        }

        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for dep_name in package_entry
            .dependencies
            .keys()
            .chain(package_entry.optional_dependencies.keys())
        {
            // Forward references — we'll resolve them in a second pass using
            // the node nested-resolution walk.
            deps.insert(dep_name.clone(), String::new());
        }
        // Preserve the declared ranges npm writes on each nested package
        // entry. Round-tripping these is what keeps
        // `aube install --no-frozen-lockfile` from rewriting every
        // `"^4.1.0"` to `"4.3.0"` on re-emit.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in package_entry
            .dependencies
            .iter()
            .chain(package_entry.optional_dependencies.iter())
        {
            declared.insert(k.clone(), v.clone());
        }

        // Keep the `resolved` URL on every registry package so the
        // npm writer can emit `resolved:` on every entry verbatim
        // (what npm itself writes), not just the aliased /
        // JSR-specific cases where the URL is strictly unrecoverable
        // from name+version. Dropping it was the single largest
        // source of churn against npm's own output.
        let tarball_url = package_entry
            .resolved
            .as_ref()
            .filter(|_| local_source.is_none())
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .cloned();

        // Peer fields are copied verbatim from the lockfile entry.
        // Downstream (`aube-resolver::apply_peer_contexts`) reads
        // these two maps to decide which packages need a peer-context
        // suffix and which sibling symlinks to create in the isolated
        // virtual store. An npm lockfile without these fields
        // populated here would silently produce a tree where
        // peer-dependent packages can't find their peers at runtime.
        let peer_dependencies = package_entry.peer_dependencies.clone();
        let peer_dependencies_meta: BTreeMap<String, crate::PeerDepMeta> = package_entry
            .peer_dependencies_meta
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::PeerDepMeta {
                        optional: v.optional,
                    },
                )
            })
            .collect();

        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: install_name,
                version,
                integrity: package_entry.integrity.clone(),
                dependencies: deps,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                os: package_entry.os.iter().cloned().collect(),
                cpu: package_entry.cpu.iter().cloned().collect(),
                libc: package_entry.libc.iter().cloned().collect(),
                alias_of,
                tarball_url,
                declared_dependencies: declared,
                engines: package_entry.engines.clone(),
                bin: package_entry.bin.clone(),
                license: package_entry.license.clone(),
                funding_url: package_entry.funding.as_ref().and_then(|f| f.url.clone()),
                ..Default::default()
            },
        );
    }

    // Second pass: for each raw entry, resolve its transitive deps by walking
    // the npm nesting hierarchy. For an entry at `node_modules/foo`, a dep
    // `bar` resolves to whichever of `node_modules/foo/node_modules/bar` or
    // `node_modules/bar` exists — npm hoists shared versions to the root but
    // keeps conflicting versions nested.
    //
    // We then write the resolved (name → dep_path tail) back onto the
    // LockedPackage keyed by the *first* dep_path (name@version) we
    // stored. The map value is the substring that follows `<name>@` in
    // the target dep_path (just the version for simple packages), per
    // `LockedPackage.dependencies` doc — the linker recombines the
    // name and tail with an `@` separator when walking siblings.
    // Emitting the full dep_path here doubled the name and produced
    // broken sibling symlinks like `rolldown@rolldown@1.0.0` for every
    // transitive dep. This may lose fidelity if two entries share
    // (name, version) but have different resolved transitives —
    // npm.rs's data model doesn't express that, and in practice npm
    // dedupes only when the transitives match anyway.
    type ResolvedDepMap = BTreeMap<String, String>;
    let mut resolved_by_dep_path: BTreeMap<String, (ResolvedDepMap, ResolvedDepMap)> =
        BTreeMap::new();
    for (install_path, entry) in &raw.packages {
        if install_path.is_empty() {
            continue;
        }
        if link_targets.contains(install_path) {
            continue;
        }
        let Some(info) = install_path_info.get(install_path) else {
            continue;
        };
        let package_entry = if entry.link {
            let Some(target) = entry.resolved.as_ref() else {
                continue;
            };
            let Some(target_entry) = raw.packages.get(target) else {
                unreachable!("first pass validates that linked package target '{target}' exists");
            };
            target_entry
        } else {
            entry
        };
        let dep_path = info.dep_path.clone();
        let lookup_path = if entry.link {
            entry.resolved.as_deref().unwrap_or(install_path.as_str())
        } else {
            install_path.as_str()
        };

        // Skip if another occurrence already produced a resolution for this
        // dep_path (first wins, matching how we built `graph.packages`).
        if resolved_by_dep_path.contains_key(&dep_path) {
            continue;
        }

        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        let mut resolved_optional: BTreeMap<String, String> = BTreeMap::new();
        for (dep_name, is_optional) in package_entry
            .dependencies
            .keys()
            .map(|name| (name, false))
            .chain(
                package_entry
                    .optional_dependencies
                    .keys()
                    .map(|name| (name, true)),
            )
        {
            if let Some(target_install_path) =
                resolve_nested(lookup_path, dep_name, &install_path_info)
                && let Some(target_info) = install_path_info.get(&target_install_path)
            {
                let tail = dep_path_tail(&target_info.name, &target_info.dep_path).to_string();
                resolved.insert(dep_name.clone(), tail.clone());
                if is_optional {
                    resolved_optional.insert(dep_name.clone(), tail);
                }
            }
        }
        resolved_by_dep_path.insert(dep_path, (resolved, resolved_optional));
    }
    for (dep_path, (deps, optional_deps)) in resolved_by_dep_path {
        if let Some(pkg) = graph.packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
            pkg.optional_dependencies = optional_deps;
        }
    }

    // Root importer: resolve direct deps from the "" entry. For root, the
    // only possible install path for `bar` is `node_modules/bar`.
    let root = raw.packages.get("").cloned().unwrap_or_default();

    let mut direct: Vec<DirectDep> = Vec::new();
    let push_direct = |dep_name: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        let root_path = format!("node_modules/{dep_name}");
        if let Some(info) = install_path_info.get(&root_path) {
            direct.push(DirectDep {
                name: info.name.clone(),
                dep_path: info.dep_path.clone(),
                dep_type,
                specifier: None,
            });
        }
    };
    for dep_name in root.dependencies.keys() {
        push_direct(dep_name, DepType::Production, &mut direct);
    }
    for dep_name in root.dev_dependencies.keys() {
        push_direct(dep_name, DepType::Dev, &mut direct);
    }
    for dep_name in root.optional_dependencies.keys() {
        push_direct(dep_name, DepType::Optional, &mut direct);
    }

    // npm symlinks every workspace member (and any other top-level
    // `npm install ../local-pkg` link) into the root `node_modules/`
    // regardless of what the root manifest declares. Each one shows
    // up in the lockfile as `node_modules/<name>: { link: true,
    // resolved: "<rel>" }`. Surface those as direct deps of the
    // root importer so the linker recreates the same symlinks on
    // `aube install`. Without this, builds that resolve workspace
    // packages from the repo root (Angular CLI / Nx / many monorepo
    // build tools) silently break when migrating npm-managed
    // workspaces over to aube — the root `node_modules/<ws-pkg>`
    // entry simply isn't created. Sorted by name for deterministic
    // ordering.
    let already_added: BTreeSet<&str> = direct.iter().map(|d| d.name.as_str()).collect();
    let mut workspace_links: Vec<DirectDep> = Vec::new();
    for (install_path, raw_entry) in &raw.packages {
        if !raw_entry.link {
            continue;
        }
        let Some(rest) = install_path.strip_prefix("node_modules/") else {
            continue;
        };
        // Only consider top-level entries: `node_modules/<name>` or
        // `node_modules/@scope/<name>`. A nested `node_modules/`
        // segment means this is a non-hoisted nested link, not a
        // root symlink.
        if rest.contains("/node_modules/") {
            continue;
        }
        let segments = rest.split('/').count();
        let expected = if rest.starts_with('@') { 2 } else { 1 };
        if segments != expected {
            continue;
        }
        let Some(info) = install_path_info.get(install_path) else {
            continue;
        };
        if already_added.contains(info.name.as_str()) {
            continue;
        }
        workspace_links.push(DirectDep {
            name: info.name.clone(),
            dep_path: info.dep_path.clone(),
            dep_type: DepType::Production,
            specifier: None,
        });
    }
    workspace_links.sort_by(|a, b| a.name.cmp(&b.name));
    direct.extend(workspace_links);

    graph.importers.insert(".".to_string(), direct);

    // Workspace importers: npm records each workspace package twice:
    // `node_modules/<name>` is a link, while the target path (`web`,
    // `packages/app`, ...) carries that package's own dependency sections.
    // Preserve those target paths as graph importers so install/link and a
    // later package-lock rewrite keep each workspace's node_modules tree.
    for target in &link_targets {
        if target.is_empty() {
            continue;
        }
        let Some(package_entry) = raw.packages.get(target) else {
            continue;
        };
        let mut direct = Vec::new();
        for (dep_name, specifier, dep_type) in package_entry
            .dependencies
            .iter()
            .map(|(name, spec)| (name, spec, DepType::Production))
            .chain(
                package_entry
                    .dev_dependencies
                    .iter()
                    .map(|(name, spec)| (name, spec, DepType::Dev)),
            )
            .chain(
                package_entry
                    .optional_dependencies
                    .iter()
                    .map(|(name, spec)| (name, spec, DepType::Optional)),
            )
        {
            if let Some(target_install_path) = resolve_nested(target, dep_name, &install_path_info)
                && let Some(info) = install_path_info.get(&target_install_path)
            {
                direct.push(DirectDep {
                    name: info.name.clone(),
                    dep_path: info.dep_path.clone(),
                    dep_type,
                    specifier: Some(specifier.clone()),
                });
            }
        }
        graph.importers.insert(target.clone(), direct);
    }
    Ok(graph)
}

pub(crate) fn dep_path_tail<'a>(name: &str, dep_path: &'a str) -> &'a str {
    dep_path
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix('@'))
        .unwrap_or_else(|| {
            debug_assert!(
                false,
                "dep_path '{dep_path}' does not start with name '{name}'"
            );
            dep_path
        })
}

fn local_git_source_from_resolved(resolved: &str) -> Option<LocalSource> {
    let (url, committish, subpath) = crate::parse_git_spec(resolved)?;
    let resolved = committish.clone()?;
    Some(LocalSource::Git(GitSource {
        url,
        committish,
        resolved,
        subpath,
    }))
}

fn npm_resolved_field(pkg: &LockedPackage) -> Option<String> {
    pkg.tarball_url.clone().or_else(|| match &pkg.local_source {
        Some(LocalSource::Git(git)) => {
            let url = if git.url.starts_with("git://") || git.url.starts_with("git+") {
                git.url.clone()
            } else {
                format!("git+{}", git.url)
            };
            match &git.subpath {
                Some(subpath) => Some(format!("{url}#{}&path:/{subpath}", git.resolved)),
                None => Some(format!("{url}#{}", git.resolved)),
            }
        }
        _ => None,
    })
}

/// Resolve a transitive dep name from the perspective of a package at
/// `pkg_install_path` using npm's nested-resolution walk: look first inside
/// the package's own `node_modules`, then walk up each ancestor's
/// `node_modules`, finally falling back to the root `node_modules`.
fn resolve_nested(
    pkg_install_path: &str,
    dep_name: &str,
    install_paths: &BTreeMap<String, InstallPathInfo>,
) -> Option<String> {
    let mut base = pkg_install_path.to_string();
    loop {
        let candidate = if base.is_empty() {
            format!("node_modules/{dep_name}")
        } else {
            format!("{base}/node_modules/{dep_name}")
        };
        if install_paths.contains_key(&candidate) {
            return Some(candidate);
        }
        if base.is_empty() {
            return None;
        }
        // Walk up one level: strip the trailing "/node_modules/<pkg>" segment.
        if let Some(idx) = base.rfind("/node_modules/") {
            base.truncate(idx);
        } else {
            // We're at a top-level path like "node_modules/foo" — next step is root.
            base.clear();
        }
    }
}

/// Extract a package name from an install path like `node_modules/foo`,
/// `node_modules/@scope/foo`, or `node_modules/foo/node_modules/bar`.
fn package_name_from_install_path(install_path: &str) -> Option<String> {
    // Find the last "node_modules/" segment and return everything after it,
    // preserving a scope prefix (`@scope/pkg`).
    let nm_idx = install_path.rfind("node_modules/")?;
    let tail = &install_path[nm_idx + "node_modules/".len()..];

    if tail.is_empty() {
        return None;
    }

    if let Some(rest) = tail.strip_prefix('@') {
        // @scope/pkg
        let slash = rest.find('/')?;
        let scoped_end = slash + 1;
        let name_end = rest[scoped_end..]
            .find('/')
            .map(|i| scoped_end + i)
            .unwrap_or(rest.len());
        return Some(format!("@{}", &rest[..name_end]));
    }

    let end = tail.find('/').unwrap_or(tail.len());
    Some(tail[..end].to_string())
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → package-lock.json v3
// ---------------------------------------------------------------------------

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
///  - `resolved` tarball URLs are omitted for non-aliased packages —
///    we don't persist the origin URL in [`LockedPackage`]. npm's own
///    consumers tolerate missing `resolved` (they refetch from the
///    registry); aube's own parser only needs `integrity`, so round-trip
///    through the parser is lossless for the data it inspects. Aliased
///    entries always emit `resolved:` because the install-path name is
///    the alias — without the URL the consumer can't recover the real
///    registry location.
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
            .entry(canonical_key_from_dep_path(&pkg.dep_path))
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
    let tree = build_hoist_tree(&canonical, &root_tree_roots);
    // For the npm writer, re-key the tree by install_path strings.
    let mut placed: BTreeMap<String, String> = tree
        .into_iter()
        .map(|(segs, key)| (segments_to_install_path(&segs), key))
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
        let workspace_tree = build_hoist_tree(&canonical, &workspace_tree_roots);
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
            let install_path = format!("{importer_path}/{}", segments_to_install_path(&segs));
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
            .filter(|(n, value)| canonical.contains_key(&child_canonical_key(n, value)))
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
                    .unwrap_or_else(|| dep_value_as_version(n, value));
                (n.as_str(), rendered)
            })
            .collect();
        let deps: BTreeMap<&str, &str> = pkg
            .dependencies
            .iter()
            .filter(|(n, value)| {
                !pkg.optional_dependencies.contains_key(*n)
                    && canonical.contains_key(&child_canonical_key(n, value))
            })
            .map(|(n, value)| {
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .map(String::as_str)
                    .unwrap_or_else(|| dep_value_as_version(n, value));
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
        let resolved = npm_resolved_field(pkg);

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
            dep_value_as_version(&dep.name, dep_path_tail(&dep.name, &dep.dep_path))
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

/// Render a segment list `["foo", "bar"]` as an npm-style install
/// path `node_modules/foo/node_modules/bar`. Empty list → empty
/// string (the root importer key).
pub(crate) fn segments_to_install_path(segs: &[String]) -> String {
    if segs.is_empty() {
        return String::new();
    }
    let mut out = String::from("node_modules/");
    for (i, s) in segs.iter().enumerate() {
        if i > 0 {
            out.push_str("/node_modules/");
        }
        out.push_str(s);
    }
    out
}

/// Build a hoist + nest tree from a flat [`LockfileGraph`]-derived
/// `canonical` map. Returned keys are segment lists — an empty list
/// is the root importer; `["foo"]` is the hoisted top-level `foo`;
/// `["foo", "bar"]` is a nested `bar` living under `foo` when the
/// version conflict forced it off the top.
///
/// Shared by the npm and bun writers, which both model a hoisted
/// nested `node_modules` layout and differ only in how they render
/// the segment list as a lookup key. Yarn v1 has no nesting and
/// doesn't use this function.
///
/// Algorithm:
///   1. Place each root direct dep at `[name]`.
///   2. BFS: for each placed node, walk its declared deps. For every
///      child, search the ancestor chain for an existing entry —
///      nearest-ancestor first. If an ancestor already carries the
///      right version, the child resolves through that and needs no
///      new entry. If an ancestor has the *wrong* version (or we
///      reach the root empty-handed), try hoisting to `[child]`;
///      if that slot is occupied by a different version, nest at
///      `[...parent, child]`.
///   3. Cycles terminate because each segment-list is placed at most once.
pub(crate) fn build_hoist_tree(
    canonical: &BTreeMap<String, &LockedPackage>,
    roots: &[DirectDep],
) -> BTreeMap<Vec<String>, String> {
    let mut placed: BTreeMap<Vec<String>, String> = BTreeMap::new();
    let mut queue: VecDeque<(Vec<String>, String)> = VecDeque::new();

    for dep in roots {
        let key = canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&key) {
            continue;
        }
        let segs = vec![dep.name.clone()];
        if placed.insert(segs.clone(), key.clone()).is_none() {
            queue.push_back((segs, key));
        }
    }

    while let Some((parent_segs, parent_key)) = queue.pop_front() {
        let Some(pkg) = canonical.get(&parent_key).copied() else {
            continue;
        };
        let mut child_entries: Vec<(String, String)> = Vec::new();
        for (child_name, child_value) in &pkg.dependencies {
            let child_key = child_canonical_key(child_name, child_value);
            if !canonical.contains_key(&child_key) {
                continue;
            }
            child_entries.push((child_name.clone(), child_key));
        }

        for (child_name, child_key) in child_entries {
            match ancestor_resolution(&parent_segs, &child_name, &child_key, &placed) {
                AncestorHit::Match => continue,
                AncestorHit::Shadowed => {
                    // An intermediate ancestor carries a *different*
                    // version of `child_name`, which shadows anything
                    // at root. Node's runtime walk would stop at the
                    // ancestor and resolve the wrong version, so we
                    // must place a new entry directly inside the
                    // parent's own `node_modules` to short-circuit
                    // the shadow. Never fall through to the root-slot
                    // logic here, even if root happens to already
                    // carry the right version.
                    let mut nested = parent_segs.clone();
                    nested.push(child_name.clone());
                    if placed.insert(nested.clone(), child_key.clone()).is_none() {
                        queue.push_back((nested, child_key));
                    }
                }
                AncestorHit::Miss => {
                    // Ancestor chain is empty (including root). Hoist.
                    // Today the walk guarantees the root slot is empty
                    // when we get here, so `.is_none()` always holds —
                    // but match the `Shadowed` branch's insert-guard
                    // pattern exactly so a future change to when Miss
                    // is returned can't silently introduce duplicate
                    // queue entries or an unguarded overwrite.
                    let root_slot = vec![child_name.clone()];
                    if placed
                        .insert(root_slot.clone(), child_key.clone())
                        .is_none()
                    {
                        queue.push_back((root_slot, child_key));
                    }
                }
            }
        }
    }

    placed
}

/// Three-way result of an ancestor-chain lookup. Differentiating
/// `Miss` (nothing anywhere — safe to hoist) from `Shadowed` (a
/// wrong-version ancestor blocks hoisting and forces a nested
/// placement) is load-bearing: conflating them caused a real bug
/// where an intermediate ancestor carrying the wrong version would
/// silently shadow a correct root entry at runtime.
enum AncestorHit {
    Match,
    Shadowed,
    Miss,
}

/// Walk the ancestor chain of `parent_segs` nearest-first looking
/// for an entry named `child_name`, and classify the first hit
/// against `child_key`. `Match` iff the nearest hit equals
/// `child_key`; `Shadowed` iff it's a different version; `Miss` iff
/// the entire chain (including root) is empty.
fn ancestor_resolution(
    parent_segs: &[String],
    child_name: &str,
    child_key: &str,
    placed: &BTreeMap<Vec<String>, String>,
) -> AncestorHit {
    // Candidate layering, nearest first:
    //   parent_segs + [child]
    //   parent_segs[..-1] + [child]
    //   ...
    //   [child]  (root)
    for i in (0..=parent_segs.len()).rev() {
        let mut candidate: Vec<String> = parent_segs[..i].to_vec();
        candidate.push(child_name.to_string());
        if let Some(existing) = placed.get(&candidate) {
            return if existing == child_key {
                AncestorHit::Match
            } else {
                AncestorHit::Shadowed
            };
        }
    }
    AncestorHit::Miss
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
        let key = canonical_key_from_dep_path(&dep.dep_path);
        if canonical.contains_key(&key) && out.insert(key.clone()) {
            queue.push_back(key);
        }
    }
    while let Some(key) = queue.pop_front() {
        let Some(pkg) = canonical.get(&key).copied() else {
            continue;
        };
        for (child_name, child_value) in &pkg.dependencies {
            let child_key = child_canonical_key(child_name, child_value);
            if canonical.contains_key(&child_key) && out.insert(child_key.clone()) {
                queue.push_back(child_key);
            }
        }
    }
    out
}

/// Strip any `(peer@ver)` suffix from a dep_path tail, returning just
/// the version. Input `"18.2.0(prop-types@15.8.1)"` → `"18.2.0"`.
fn version_from_tail(tail: &str) -> &str {
    tail.split_once('(').map(|(v, _)| v).unwrap_or(tail)
}

/// Compute the canonical `name@version` key for a child declared in
/// [`LockedPackage::dependencies`]. Tolerates both encodings seen in
/// practice: the documented "tail only" form (`"1.0.0"`) used by
/// `pnpm::parse` *and* the "full dep_path" form (`"bar@1.0.0"`)
/// currently emitted by [`parse`] above. Peer context suffixes are
/// stripped in both branches.
pub(crate) fn child_canonical_key(child_name: &str, value: &str) -> String {
    let no_peer = version_from_tail(value);
    let prefix = format!("{child_name}@");
    if no_peer.starts_with(&prefix) {
        no_peer.to_string()
    } else {
        format!("{prefix}{no_peer}")
    }
}

/// Render a child dep value back as a bare version string, regardless
/// of which encoding it was stored in. Used when writing out the
/// `dependencies` field of a nested package entry.
pub(crate) fn dep_value_as_version<'a>(child_name: &str, value: &'a str) -> &'a str {
    let no_peer = version_from_tail(value);
    let prefix = format!("{child_name}@");
    if let Some(rest) = no_peer.strip_prefix(&prefix) {
        rest
    } else {
        no_peer
    }
}

/// Extract `"name@version"` from a full dep_path, dropping any peer
/// context suffix. Strips the `(peer@ver)` tail *first* so the
/// `rfind('@')` that separates name from version can't land inside
/// the peer suffix — e.g. `"foo@1.0.0(react@18.2.0)"` must resolve
/// to `"foo@1.0.0"`, not `"foo@1.0.0(react@18.2.0)"` (which would
/// then miss the canonical map and silently drop the package from
/// the written lockfile).
pub(crate) fn canonical_key_from_dep_path(dep_path: &str) -> String {
    let trimmed = version_from_tail(dep_path);
    let (name, version) = match trimmed.rfind('@') {
        Some(0) | None => return trimmed.to_string(),
        Some(idx) => (&trimmed[..idx], &trimmed[idx + 1..]),
    };
    format!("{name}@{version}")
}

fn borrow_map(m: &BTreeMap<String, String>) -> BTreeMap<&str, &str> {
    m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_name_from_install_path() {
        assert_eq!(
            package_name_from_install_path("node_modules/foo"),
            Some("foo".to_string())
        );
        assert_eq!(
            package_name_from_install_path("node_modules/@scope/pkg"),
            Some("@scope/pkg".to_string())
        );
        assert_eq!(
            package_name_from_install_path("node_modules/foo/node_modules/bar"),
            Some("bar".to_string())
        );
        assert_eq!(
            package_name_from_install_path("node_modules/foo/node_modules/@scope/pkg"),
            Some("@scope/pkg".to_string())
        );
    }

    #[test]
    fn test_parse_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" },
                    "devDependencies": { "bar": "^2.0.0" }
                },
                "node_modules/foo": {
                    "version": "1.2.3",
                    "integrity": "sha512-aaa",
                    "dependencies": { "nested": "^3.0.0" }
                },
                "node_modules/nested": {
                    "version": "3.1.0",
                    "integrity": "sha512-bbb"
                },
                "node_modules/bar": {
                    "version": "2.5.0",
                    "integrity": "sha512-ccc",
                    "dev": true
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();

        assert_eq!(graph.packages.len(), 3);
        assert!(graph.packages.contains_key("foo@1.2.3"));
        assert!(graph.packages.contains_key("nested@3.1.0"));
        assert!(graph.packages.contains_key("bar@2.5.0"));

        let foo = &graph.packages["foo@1.2.3"];
        assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
        // `LockedPackage.dependencies` values are dep_path *tails* (the
        // substring after `<name>@`), not full dep_paths — matches the
        // pnpm parser and the linker's sibling-symlink builder.
        assert_eq!(
            foo.dependencies.get("nested").map(String::as_str),
            Some("3.1.0")
        );

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 2);
        assert!(
            root.iter()
                .any(|d| d.name == "foo" && d.dep_type == DepType::Production)
        );
        assert!(
            root.iter()
                .any(|d| d.name == "bar" && d.dep_type == DepType::Dev)
        );
    }

    #[test]
    fn test_parse_git_resolved_package() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        let content = format!(
            r#"{{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 2,
            "packages": {{
                "": {{
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {{ "git-only": "github:owner/repo#{sha}" }}
                }},
                "node_modules/git-only": {{
                    "version": "1.2.3",
                    "resolved": "git+ssh://git@github.com/owner/repo.git#{sha}",
                    "integrity": "sha512-aaa"
                }}
            }}
        }}"#
        );
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let root = &graph.importers["."];
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "git-only");
        assert!(!graph.packages.contains_key("git-only@1.2.3"));

        let pkg = &graph.packages[&root[0].dep_path];
        assert_eq!(pkg.name, "git-only");
        assert_eq!(pkg.version, "1.2.3");
        assert_eq!(pkg.integrity.as_deref(), Some("sha512-aaa"));
        assert!(pkg.tarball_url.is_none());

        let Some(LocalSource::Git(git)) = &pkg.local_source else {
            panic!("expected git local source, got {:?}", pkg.local_source);
        };
        assert_eq!(git.url, "ssh://git@github.com/owner/repo.git");
        assert_eq!(git.committish.as_deref(), Some(sha));
        assert_eq!(git.resolved, sha);
    }

    #[test]
    fn test_unpinned_git_resolved_url_is_not_locked_git_source() {
        assert!(local_git_source_from_resolved("git+https://github.com/owner/repo.git").is_none());
    }

    #[test]
    fn test_write_preserves_git_resolved_url() {
        let sha = "abcdef1234567890abcdef1234567890abcdef12";
        let mut graph = LockfileGraph::default();
        let local = LocalSource::Git(GitSource {
            url: "ssh://git@github.com/owner/repo.git".to_string(),
            committish: Some(sha.to_string()),
            resolved: sha.to_string(),
            subpath: None,
        });
        let dep_path = local.dep_path("git-only");
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "git-only".to_string(),
                version: "1.2.3".to_string(),
                dep_path: dep_path.clone(),
                local_source: Some(local),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "git-only".to_string(),
                dep_path,
                dep_type: DepType::Production,
                specifier: Some(format!("github:owner/repo#{sha}")),
            }],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("git-only".to_string(), format!("github:owner/repo#{sha}"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let body = std::fs::read_to_string(out.path()).unwrap();
        assert!(
            body.contains(&format!(
                "\"resolved\": \"git+ssh://git@github.com/owner/repo.git#{sha}\""
            )),
            "expected git resolved URL emitted; got:\n{body}"
        );

        let reparsed = parse(out.path()).unwrap();
        let pkg = &reparsed.packages[&graph.importers["."][0].dep_path];
        assert!(matches!(pkg.local_source, Some(LocalSource::Git(_))));
    }

    #[test]
    fn test_write_skips_non_git_local_sources() {
        let local = LocalSource::Directory(PathBuf::from("vendor/local-dir"));
        let dep_path = local.dep_path("local-dir");
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.clone(),
            LockedPackage {
                name: "local-dir".to_string(),
                version: "1.0.0".to_string(),
                dep_path: dep_path.clone(),
                local_source: Some(local),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "local-dir".to_string(),
                dep_path,
                dep_type: DepType::Production,
                specifier: Some("file:vendor/local-dir".to_string()),
            }],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("local-dir".to_string(), "file:vendor/local-dir".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let body = std::fs::read_to_string(out.path()).unwrap();
        assert!(!body.contains("\"node_modules/local-dir\""));
    }

    #[test]
    fn test_parse_scoped_package() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "@scope/pkg": "^1.0.0" }
                },
                "node_modules/@scope/pkg": {
                    "version": "1.0.0",
                    "integrity": "sha512-zzz"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
        let root = graph.importers.get(".").unwrap();
        assert_eq!(root[0].name, "@scope/pkg");
        assert_eq!(root[0].dep_path, "@scope/pkg@1.0.0");
    }

    #[test]
    fn test_parse_multi_version_nested() {
        // bar exists at two versions: 2.0.0 hoisted to root, 1.0.0 nested under foo.
        // foo's transitive dep on bar must resolve to 1.0.0, not 2.0.0.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        // Both versions of bar should be present.
        assert!(graph.packages.contains_key("bar@2.0.0"));
        assert!(graph.packages.contains_key("bar@1.0.0"));
        assert!(graph.packages.contains_key("foo@1.0.0"));

        // foo's transitive dep must point to the nested (1.0.0), not the hoisted (2.0.0).
        // Value is the dep_path tail (version) — see the `LockedPackage.dependencies` doc.
        let foo = &graph.packages["foo@1.0.0"];
        assert_eq!(
            foo.dependencies.get("bar").map(String::as_str),
            Some("1.0.0")
        );

        // Root's direct bar dep points to the hoisted 2.0.0.
        let root = graph.importers.get(".").unwrap();
        let root_bar = root.iter().find(|d| d.name == "bar").unwrap();
        assert_eq!(root_bar.dep_path, "bar@2.0.0");
    }

    /// Regression: a package reachable from both a dev root and
    /// an optional root (but *not* from any production root) must
    /// be written with `devOptional: true`, not with both `dev: true`
    /// and `optional: true`. Emitting both trips `npm install
    /// --omit=dev` (and `--omit=optional`) into dropping a package
    /// the other chain still needs.
    #[test]
    fn test_write_dev_and_optional_reachable_uses_dev_optional() {
        let mut graph = LockfileGraph::default();
        let mk = |name: &str| LockedPackage {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            integrity: Some(format!("sha512-{name}")),
            dep_path: format!("{name}@1.0.0"),
            dependencies: [("shared".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        graph
            .packages
            .insert("dev-root@1.0.0".to_string(), mk("dev-root"));
        graph
            .packages
            .insert("opt-root@1.0.0".to_string(), mk("opt-root"));
        graph.packages.insert(
            "shared@1.0.0".to_string(),
            LockedPackage {
                name: "shared".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-shared".to_string()),
                dep_path: "shared@1.0.0".to_string(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "dev-root".to_string(),
                    dep_path: "dev-root@1.0.0".to_string(),
                    dep_type: DepType::Dev,
                    specifier: None,
                },
                DirectDep {
                    name: "opt-root".to_string(),
                    dep_path: "opt-root@1.0.0".to_string(),
                    dep_type: DepType::Optional,
                    specifier: None,
                },
            ],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dev_dependencies: [("dev-root".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: [("opt-root".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

        let shared = &json["packages"]["node_modules/shared"];
        assert_eq!(shared["devOptional"], true, "expected devOptional flag");
        assert!(
            shared.get("dev").is_none(),
            "must not emit dev: true alongside devOptional",
        );
        assert!(
            shared.get("optional").is_none(),
            "must not emit optional: true alongside devOptional",
        );

        // Roots themselves retain their specific flag.
        assert_eq!(json["packages"]["node_modules/dev-root"]["dev"], true);
        assert_eq!(json["packages"]["node_modules/opt-root"]["optional"], true);
    }

    /// Regression: the npm writer must drop `dependencies` entries
    /// whose target isn't in the canonical map. Platform-filtered
    /// optionals and `ignoredOptionalDependencies` leave the parent's
    /// declared `dependencies` map pointing at packages the resolver
    /// already removed; emitting them anyway produces a lockfile
    /// where `npm ci` sees a reference with no matching `packages`
    /// entry and refuses to install. Must match the bun/yarn
    /// writers, which already filter this way.
    #[test]
    fn test_write_filters_missing_canonical_deps() {
        let mut graph = LockfileGraph::default();
        // Root has one real package, `foo`, which declares a dep on
        // `ghost@1.0.0` — but `ghost` was filtered out of the graph
        // (e.g. a platform-gated optional). The canonical map won't
        // contain it.
        graph.packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-foo".to_string()),
                dep_path: "foo@1.0.0".to_string(),
                dependencies: [("ghost".to_string(), "1.0.0".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let manifest = test_manifest();
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        // Parse the raw JSON directly — the aube reparser tolerates
        // dangling references so we assert on the serialized shape.
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
        let foo_entry = &json["packages"]["node_modules/foo"];
        assert!(
            foo_entry
                .get("dependencies")
                .and_then(|d| d.get("ghost"))
                .is_none(),
            "writer emitted a ghost dep that has no packages entry: {foo_entry}",
        );
        // And there should be no node_modules/ghost entry at all.
        assert!(
            json["packages"].get("node_modules/ghost").is_none(),
            "writer hallucinated a ghost entry",
        );
    }

    /// Regression for the shadow-nesting bug: if an intermediate
    /// ancestor carries the *wrong* version of a dep, Node's
    /// runtime walk stops there and never reaches a correct entry
    /// at root. The writer must nest a fresh entry inside the
    /// current parent's own `node_modules` instead of assuming
    /// hoisting is fine just because root happens to have the
    /// right version.
    ///
    /// Shape:
    ///   root → foo → baz, baz depends on bar@2.0.0
    ///   foo already pulled in bar@1.0.0 for a sibling, so bar@1.0.0
    ///     lives at node_modules/foo/node_modules/bar
    ///   root has bar@2.0.0 at node_modules/bar
    ///
    ///   When we walk baz's deps and get to bar@2.0.0, the nearest
    ///   ancestor hit is bar@1.0.0 (shadowing), not root. We must
    ///   place a fresh entry at
    ///   `node_modules/foo/node_modules/baz/node_modules/bar` so
    ///   Node resolves the right version.
    #[test]
    fn test_nested_shadow_forces_nested_placement() {
        // Build a graph by hand to control the dep order deterministically.
        let mut graph = LockfileGraph::default();
        let mk = |name: &str, version: &str, deps: &[(&str, &str)]| LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            integrity: Some(format!("sha512-{name}-{version}")),
            dep_path: format!("{name}@{version}"),
            dependencies: deps
                .iter()
                .map(|(n, v)| (n.to_string(), (*v).to_string()))
                .collect(),
            ..Default::default()
        };
        graph.packages.insert(
            "foo@1.0.0".to_string(),
            mk(
                "foo",
                "1.0.0",
                &[
                    // foo pulls in bar@1.0.0 and baz@1.0.0 as siblings.
                    ("bar", "1.0.0"),
                    ("baz", "1.0.0"),
                ],
            ),
        );
        graph.packages.insert(
            "baz@1.0.0".to_string(),
            // baz wants bar@2.0.0, which matches the root version.
            mk("baz", "1.0.0", &[("bar", "2.0.0")]),
        );
        graph
            .packages
            .insert("bar@1.0.0".to_string(), mk("bar", "1.0.0", &[]));
        graph
            .packages
            .insert("bar@2.0.0".to_string(), mk("bar", "2.0.0", &[]));
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".to_string(),
                    dep_path: "foo@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
                DirectDep {
                    name: "bar".to_string(),
                    dep_path: "bar@2.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
            ],
        );

        let manifest = test_manifest();
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let reparsed = parse(out.path()).unwrap();

        // baz's transitive dep must resolve to bar@2.0.0, not the
        // shadowing bar@1.0.0 under foo. Value is the dep_path tail
        // (version) so the linker can recombine it with the dep name.
        let baz = &reparsed.packages["baz@1.0.0"];
        assert_eq!(
            baz.dependencies.get("bar").map(String::as_str),
            Some("2.0.0"),
            "baz's bar dep was shadowed by foo/bar@1.0.0 — shadow-nest fix regressed",
        );
    }

    #[test]
    fn test_parse_npm_preserves_platform_optional_metadata() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "platform-optional-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "platform-optional-root",
                    "version": "1.0.0",
                    "dependencies": { "host": "file:host" }
                },
                "node_modules/host": {
                    "resolved": "host",
                    "link": true
                },
                "host": {
                    "name": "host",
                    "version": "1.0.0",
                    "optionalDependencies": { "native-win": "1.0.0" }
                },
                "node_modules/native-win": {
                    "version": "1.0.0",
                    "resolved": "https://registry.npmjs.org/native-win/-/native-win-1.0.0.tgz",
                    "integrity": "sha512-native",
                    "optional": true,
                    "os": ["win32"],
                    "cpu": ["x64"],
                    "libc": ["glibc"]
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let host_dep_path = &graph.importers["."][0].dep_path;
        let host = &graph.packages[host_dep_path];
        assert_eq!(
            host.dependencies.get("native-win").map(String::as_str),
            Some("1.0.0")
        );
        assert_eq!(
            host.optional_dependencies
                .get("native-win")
                .map(String::as_str),
            Some("1.0.0")
        );

        let native = &graph.packages["native-win@1.0.0"];
        assert_eq!(
            native.os.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["win32"]
        );
        assert_eq!(
            native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["x64"]
        );
        assert_eq!(
            native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["glibc"]
        );
    }

    /// npm sometimes emits `os` / `cpu` / `libc` as scalar strings instead
    /// of arrays (e.g. `sass-embedded-linux-arm@1.99.0` ships
    /// `"libc": "glibc"`). Verbatim-roundtripped into package-lock.json,
    /// the field stays scalar — accept both shapes the same way the
    /// pnpm + bun parsers already do.
    #[test]
    fn parse_npm_package_platform_fields_accept_scalar_strings() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "scalar-platform-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "scalar-platform-root",
                    "version": "1.0.0",
                    "dependencies": { "sass-embedded-linux-arm": "1.99.0" }
                },
                "node_modules/sass-embedded-linux-arm": {
                    "version": "1.99.0",
                    "resolved": "https://registry.npmjs.org/sass-embedded-linux-arm/-/sass-embedded-linux-arm-1.99.0.tgz",
                    "integrity": "sha512-native",
                    "cpu": "arm",
                    "os": "linux",
                    "libc": "glibc"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let pkg = &graph.packages["sass-embedded-linux-arm@1.99.0"];
        assert_eq!(
            pkg.os.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["linux"]
        );
        assert_eq!(
            pkg.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["arm"]
        );
        assert_eq!(
            pkg.libc.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["glibc"]
        );
    }

    #[test]
    fn test_write_npm_preserves_platform_optional_metadata() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "host@1.0.0".to_string(),
            LockedPackage {
                name: "host".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-host".to_string()),
                dep_path: "host@1.0.0".to_string(),
                dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                    .into_iter()
                    .collect(),
                optional_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                    .into_iter()
                    .collect(),
                declared_dependencies: [("native-win".to_string(), "1.0.0".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-win@1.0.0".to_string(),
            LockedPackage {
                name: "native-win".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-native".to_string()),
                dep_path: "native-win@1.0.0".to_string(),
                os: vec!["win32".to_string()].into(),
                cpu: vec!["x64".to_string()].into(),
                libc: vec!["glibc".to_string()].into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "host".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        let manifest = aube_manifest::PackageJson {
            name: Some("platform-optional-root".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("host".to_string(), "1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();

        let host = &json["packages"]["node_modules/host"];
        assert_eq!(host["optionalDependencies"]["native-win"], "1.0.0");
        assert!(
            host.get("dependencies")
                .and_then(|deps| deps.get("native-win"))
                .is_none(),
            "optional child must not be duplicated as a required dependency: {host}",
        );

        let native = &json["packages"]["node_modules/native-win"];
        assert_eq!(native["os"], serde_json::json!(["win32"]));
        assert_eq!(native["cpu"], serde_json::json!(["x64"]));
        assert_eq!(native["libc"], serde_json::json!(["glibc"]));

        let reparsed = parse(out.path()).unwrap();
        let host = &reparsed.packages["host@1.0.0"];
        assert_eq!(
            host.optional_dependencies
                .get("native-win")
                .map(String::as_str),
            Some("1.0.0")
        );
        assert_eq!(
            host.dependencies.get("native-win").map(String::as_str),
            Some("1.0.0")
        );
        let native = &reparsed.packages["native-win@1.0.0"];
        assert_eq!(
            native.os.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["win32"]
        );
        assert_eq!(
            native.cpu.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["x64"]
        );
        assert_eq!(
            native.libc.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["glibc"]
        );
    }

    /// Regression: `canonical_key_from_dep_path` must strip the
    /// `(peer@ver)` suffix *before* splitting on `@`. A naive
    /// `rfind('@')` lands inside the peer suffix and returns the
    /// input unchanged, which silently drops every peer-contextualized
    /// root dep from the written lockfile.
    #[test]
    fn test_canonical_key_strips_peer_suffix() {
        assert_eq!(canonical_key_from_dep_path("foo@1.0.0"), "foo@1.0.0");
        assert_eq!(
            canonical_key_from_dep_path("styled-components@6.1.0(react@18.2.0)"),
            "styled-components@6.1.0"
        );
        assert_eq!(
            canonical_key_from_dep_path("@scope/pkg@2.0.0(peer@1.0.0)"),
            "@scope/pkg@2.0.0"
        );
    }

    fn test_manifest() -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [
                ("foo".to_string(), "^1.0.0".to_string()),
                ("bar".to_string(), "^2.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    /// Parse a fixture, write it back, re-parse: the resulting graph
    /// must have the same packages, direct deps, and integrity hashes.
    /// Catches silent data loss in the hoist/nest walk.
    #[test]
    fn test_write_roundtrip_multi_version() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
                },
                "node_modules/bar": {
                    "version": "2.0.0",
                    "integrity": "sha512-top-bar"
                },
                "node_modules/foo": {
                    "version": "1.0.0",
                    "integrity": "sha512-foo",
                    "dependencies": { "bar": "^1.0.0" }
                },
                "node_modules/foo/node_modules/bar": {
                    "version": "1.0.0",
                    "integrity": "sha512-nested-bar"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let manifest = test_manifest();

        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let reparsed = parse(out.path()).unwrap();

        // Both versions of bar survived the round-trip.
        assert!(reparsed.packages.contains_key("bar@1.0.0"));
        assert!(reparsed.packages.contains_key("bar@2.0.0"));
        assert!(reparsed.packages.contains_key("foo@1.0.0"));
        assert_eq!(
            reparsed.packages["bar@2.0.0"].integrity.as_deref(),
            Some("sha512-top-bar")
        );
        assert_eq!(
            reparsed.packages["bar@1.0.0"].integrity.as_deref(),
            Some("sha512-nested-bar")
        );
        // foo's nested bar dep still resolves to 1.0.0, not the
        // hoisted 2.0.0. If the writer failed to nest, reparse would
        // snap this to bar@2.0.0. Value is the dep_path tail.
        assert_eq!(
            reparsed.packages["foo@1.0.0"]
                .dependencies
                .get("bar")
                .map(String::as_str),
            Some("1.0.0")
        );
    }

    /// Dev-only and optional-only packages get the right flags after
    /// round-trip so `npm install --omit=dev` on the written file
    /// does the right thing.
    #[test]
    fn test_write_dev_optional_flags() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-foo".to_string()),
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "devdep@1.0.0".to_string(),
            LockedPackage {
                name: "devdep".to_string(),
                version: "1.0.0".to_string(),
                integrity: Some("sha512-dev".to_string()),
                dep_path: "devdep@1.0.0".to_string(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "foo".to_string(),
                    dep_path: "foo@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
                DirectDep {
                    name: "devdep".to_string(),
                    dep_path: "devdep@1.0.0".to_string(),
                    dep_type: DepType::Dev,
                    specifier: None,
                },
            ],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("foo".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            dev_dependencies: [("devdep".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };

        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
        let packages = &json["packages"];
        assert_eq!(packages["node_modules/devdep"]["dev"], true);
        // Prod dep should have no dev field (skipped when false).
        assert!(packages["node_modules/foo"].get("dev").is_none());
    }

    #[test]
    fn test_reject_v1() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "lockfileVersion": 1,
            "dependencies": {}
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let err = parse(tmp.path()).unwrap_err();
        assert!(matches!(err, Error::Parse(_, msg) if msg.contains("lockfileVersion 1")));
    }

    /// Pre-npm-2.x packages (e.g. `ansi-html-community@0.0.8`) ship
    /// `"engines": ["node >= 0.8.0"]` as an array; npm preserves that
    /// shape verbatim in v2/v3 lockfiles. Without tolerant parsing, a
    /// single such entry blows up the whole `aube ci`. Normalize to an
    /// empty map (matches what modern npm does for engine-strict on
    /// the array shape) so the install proceeds.
    #[test]
    fn test_parse_legacy_array_engines() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "ansi-html-community": "0.0.8" }
                },
                "node_modules/ansi-html-community": {
                    "version": "0.0.8",
                    "integrity": "sha512-aaa",
                    "engines": ["node >= 0.8.0"]
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let pkg = &graph.packages["ansi-html-community@0.0.8"];
        // Array shape gets normalized to an empty map — same as the
        // manifest parser, and same as what modern npm honors for the
        // engine-strict check on the array form.
        assert!(pkg.engines.is_empty());
    }

    /// npm writes `"h3-v2": "npm:h3@..."` aliases as a packages entry
    /// at `node_modules/h3-v2` with `name: "h3"` and the real registry
    /// `resolved:` URL. Aube keys the graph on the *alias* (so
    /// `node_modules/h3-v2` ends up at `.aube/h3-v2@.../node_modules/h3-v2`)
    /// but remembers the real package name in `alias_of` so fetches
    /// and store-index lookups use the URL that actually exists.
    #[test]
    fn test_parse_npm_alias_dependency() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "h3-v2": "npm:h3@2.0.1-rc.20" }
                },
                "node_modules/h3-v2": {
                    "name": "h3",
                    "version": "2.0.1-rc.20",
                    "resolved": "https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz",
                    "integrity": "sha512-aliased"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        assert_eq!(graph.packages.len(), 1);
        // Graph key and LockedPackage.name both carry the alias —
        // that's what consumers (and the linker's folder-name logic)
        // refer to when they say "h3-v2".
        let pkg = graph
            .packages
            .get("h3-v2@2.0.1-rc.20")
            .expect("aliased entry should be keyed by the alias dep_path");
        assert_eq!(pkg.name, "h3-v2");
        assert_eq!(pkg.version, "2.0.1-rc.20");
        assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
        assert_eq!(pkg.registry_name(), "h3");
        // `resolved:` round-trips into `tarball_url` so the fetcher
        // skips re-deriving from the alias-qualified name (which
        // would 404 the registry).
        assert_eq!(
            pkg.tarball_url.as_deref(),
            Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz")
        );

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "h3-v2");
        assert_eq!(root[0].dep_path, "h3-v2@2.0.1-rc.20");
    }

    /// Non-aliased entries (the common case) leave `alias_of` unset
    /// and `registry_name()` degenerates to `name`. Regression guard
    /// against over-aggressive alias detection that would flag every
    /// entry carrying an explicit `name:` field (npm sometimes emits
    /// one for non-aliased roots too).
    #[test]
    fn test_parse_non_alias_preserves_empty_alias_of() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": { "foo": "^1.0.0" }
                },
                "node_modules/foo": {
                    "name": "foo",
                    "version": "1.2.3",
                    "integrity": "sha512-foo"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let pkg = &graph.packages["foo@1.2.3"];
        assert_eq!(pkg.name, "foo");
        assert!(pkg.alias_of.is_none());
        assert_eq!(pkg.registry_name(), "foo");
        assert!(pkg.tarball_url.is_none());
    }

    /// Round-trip: writer must emit `name:` and `resolved:` for the
    /// aliased entry so a subsequent `parse()` still recognizes it as
    /// an alias. Without both fields the re-parser would see
    /// `node_modules/h3-v2` with no `name:` and treat it as a plain
    /// package called `h3-v2` — which doesn't exist on the registry.
    #[test]
    fn test_write_roundtrip_npm_alias() {
        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            "h3-v2@2.0.1-rc.20".to_string(),
            LockedPackage {
                name: "h3-v2".to_string(),
                version: "2.0.1-rc.20".to_string(),
                integrity: Some("sha512-aliased".to_string()),
                dep_path: "h3-v2@2.0.1-rc.20".to_string(),
                alias_of: Some("h3".to_string()),
                tarball_url: Some("https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz".to_string()),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "h3-v2".to_string(),
                dep_path: "h3-v2@2.0.1-rc.20".to_string(),
                dep_type: DepType::Production,
                specifier: Some("npm:h3@2.0.1-rc.20".to_string()),
            }],
        );

        let manifest = test_manifest();
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let body = std::fs::read_to_string(out.path()).unwrap();
        assert!(
            body.contains("\"name\": \"h3\""),
            "expected `name: h3` emitted for aliased entry; got:\n{body}"
        );
        assert!(
            body.contains("\"resolved\": \"https://registry.npmjs.org/h3/-/h3-2.0.1-rc.20.tgz\""),
            "expected `resolved:` URL emitted for aliased entry; got:\n{body}"
        );

        let reparsed = parse(out.path()).unwrap();
        let pkg = &reparsed.packages["h3-v2@2.0.1-rc.20"];
        assert_eq!(pkg.alias_of.as_deref(), Some("h3"));
        assert_eq!(pkg.registry_name(), "h3");
    }

    /// npm v7+ writes `peerDependencies` / `peerDependenciesMeta` onto
    /// every package entry. The parser must populate the matching
    /// `LockedPackage` fields so the resolver's `apply_peer_contexts`
    /// pass (run on npm-lockfile installs to wire peer siblings in the
    /// isolated virtual store) actually has peer info to work with.
    /// Before this parser change, peer-dependent packages like
    /// `@tanstack/devtools-vite` would install without a sibling
    /// `vite` link and die at runtime.
    #[test]
    fn test_parse_peer_dependencies() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "peer-test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "peer-test",
                    "version": "1.0.0",
                    "dependencies": { "devtools-vite": "0.6.0", "vite": "8.0.0" }
                },
                "node_modules/devtools-vite": {
                    "version": "0.6.0",
                    "integrity": "sha512-a",
                    "peerDependencies": {
                        "vite": "^6.0.0 || ^7.0.0 || ^8.0.0"
                    },
                    "peerDependenciesMeta": {
                        "vite": { "optional": false }
                    }
                },
                "node_modules/vite": {
                    "version": "8.0.0",
                    "integrity": "sha512-b"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let devtools = &graph.packages["devtools-vite@0.6.0"];
        assert_eq!(
            devtools.peer_dependencies.get("vite").map(String::as_str),
            Some("^6.0.0 || ^7.0.0 || ^8.0.0")
        );
        assert_eq!(
            devtools
                .peer_dependencies_meta
                .get("vite")
                .map(|m| m.optional),
            Some(false)
        );
    }

    /// Packages without peer fields keep both maps empty — guard
    /// against accidental defaulting to `optional: true` or spurious
    /// keys showing up in the LockedPackage from serde leak paths.
    #[test]
    fn test_parse_no_peer_fields_stays_empty() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "no-peers",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "no-peers", "version": "1.0.0", "dependencies": { "foo": "1.0.0" } },
                "node_modules/foo": { "version": "1.0.0", "integrity": "sha512-x" }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let foo = &graph.packages["foo@1.0.0"];
        assert!(foo.peer_dependencies.is_empty());
        assert!(foo.peer_dependencies_meta.is_empty());
    }

    /// Writer round-trips `peerDependencies` so a second `parse()` on
    /// the rewritten lockfile still feeds the peer-context pass. The
    /// install path writes out the lockfile after every install; if
    /// peers vanished on the first write-back, the *next* install
    /// would ship without peer siblings again.
    #[test]
    fn test_write_roundtrip_peer_dependencies() {
        let mut graph = LockfileGraph::default();
        let mut peer_deps = BTreeMap::new();
        peer_deps.insert("vite".to_string(), "^6.0.0 || ^7.0.0 || ^8.0.0".to_string());
        // Include an `optional: true` entry so the round-trip covers
        // `peerDependenciesMeta` — without it, the writer's meta
        // block isn't exercised and the round-trip would silently
        // re-flag the peer as required on every subsequent install
        // (see `hoist_auto_installed_peers` + `detect_unmet_peers`,
        // which key off `optional`).
        let mut peer_deps_meta = BTreeMap::new();
        peer_deps_meta.insert("vite".to_string(), crate::PeerDepMeta { optional: true });
        graph.packages.insert(
            "devtools-vite@0.6.0".to_string(),
            LockedPackage {
                name: "devtools-vite".to_string(),
                version: "0.6.0".to_string(),
                integrity: Some("sha512-a".to_string()),
                dep_path: "devtools-vite@0.6.0".to_string(),
                peer_dependencies: peer_deps,
                peer_dependencies_meta: peer_deps_meta,
                ..Default::default()
            },
        );
        graph.packages.insert(
            "vite@8.0.0".to_string(),
            LockedPackage {
                name: "vite".to_string(),
                version: "8.0.0".to_string(),
                integrity: Some("sha512-b".to_string()),
                dep_path: "vite@8.0.0".to_string(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "devtools-vite".to_string(),
                    dep_path: "devtools-vite@0.6.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
                DirectDep {
                    name: "vite".to_string(),
                    dep_path: "vite@8.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
            ],
        );

        let manifest = test_manifest();
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let body = std::fs::read_to_string(out.path()).unwrap();
        assert!(
            body.contains("\"peerDependencies\""),
            "expected peerDependencies block to round-trip; got:\n{body}"
        );
        assert!(
            body.contains("\"peerDependenciesMeta\""),
            "expected peerDependenciesMeta block to round-trip; got:\n{body}"
        );

        let reparsed = parse(out.path()).unwrap();
        let devtools = &reparsed.packages["devtools-vite@0.6.0"];
        assert_eq!(
            devtools.peer_dependencies.get("vite").map(String::as_str),
            Some("^6.0.0 || ^7.0.0 || ^8.0.0")
        );
        assert_eq!(
            devtools
                .peer_dependencies_meta
                .get("vite")
                .map(|m| m.optional),
            Some(true),
            "peerDependenciesMeta.optional must survive write → parse round-trip"
        );
    }

    #[test]
    fn test_parse_npm_workspace_importers() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["web"]
                },
                "node_modules/mise-versions-web": {
                    "resolved": "web",
                    "link": true
                },
                "web": {
                    "name": "mise-versions-web",
                    "version": "0.0.1",
                    "dependencies": { "astro": "^6.0.0" },
                    "devDependencies": { "vite": "^7.3.2" }
                },
                "web/node_modules/astro": {
                    "version": "6.2.1",
                    "integrity": "sha512-astro"
                },
                "web/node_modules/vite": {
                    "version": "7.3.2",
                    "integrity": "sha512-vite"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let root = graph.importers.get(".").expect("root importer");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "mise-versions-web");
        assert!(matches!(
            graph.packages[&root[0].dep_path].local_source,
            Some(LocalSource::Link(_))
        ));

        let web = graph.importers.get("web").expect("web importer");
        assert_eq!(web.len(), 2);
        assert!(web.iter().any(|dep| {
            dep.name == "astro"
                && dep.dep_type == DepType::Production
                && dep.specifier.as_deref() == Some("^6.0.0")
        }));
        assert!(web.iter().any(|dep| {
            dep.name == "vite"
                && dep.dep_type == DepType::Dev
                && dep.specifier.as_deref() == Some("^7.3.2")
        }));
    }

    #[test]
    fn test_write_npm_workspace_importers() {
        let mut graph = LockfileGraph::default();
        let web_link = LocalSource::Link(PathBuf::from("web"));
        let web_dep_path = web_link.dep_path("mise-versions-web");
        graph.packages.insert(
            web_dep_path.clone(),
            LockedPackage {
                name: "mise-versions-web".to_string(),
                version: "0.0.1".to_string(),
                dep_path: web_dep_path.clone(),
                local_source: Some(web_link),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "astro@6.2.1".to_string(),
            LockedPackage {
                name: "astro".to_string(),
                version: "6.2.1".to_string(),
                integrity: Some("sha512-astro".to_string()),
                dep_path: "astro@6.2.1".to_string(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "vite@7.3.2".to_string(),
            LockedPackage {
                name: "vite".to_string(),
                version: "7.3.2".to_string(),
                integrity: Some("sha512-vite".to_string()),
                dep_path: "vite@7.3.2".to_string(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "mise-versions-web".to_string(),
                dep_path: web_dep_path.clone(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );
        graph.importers.insert(
            "web".to_string(),
            vec![
                DirectDep {
                    name: "astro".to_string(),
                    dep_path: "astro@6.2.1".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^6.0.0".to_string()),
                },
                DirectDep {
                    name: "vite".to_string(),
                    dep_path: "vite@7.3.2".to_string(),
                    dep_type: DepType::Dev,
                    specifier: Some("^7.3.2".to_string()),
                },
            ],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("workspace-root".to_string()),
            version: Some("1.0.0".to_string()),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
        assert_eq!(
            json["packages"]["node_modules/mise-versions-web"]["link"],
            true
        );
        assert_eq!(
            json["packages"]["node_modules/mise-versions-web"]["resolved"],
            "web"
        );
        assert_eq!(json["packages"]["web"]["dependencies"]["astro"], "^6.0.0");
        assert_eq!(json["packages"]["web"]["devDependencies"]["vite"], "^7.3.2");
        assert_eq!(
            json["packages"]["web/node_modules/astro"]["version"],
            "6.2.1"
        );
        assert_eq!(
            json["packages"]["web/node_modules/vite"]["version"],
            "7.3.2"
        );

        let reparsed = parse(out.path()).unwrap();
        assert!(reparsed.importers.contains_key("web"));
    }

    /// When the root tree already hoists a package to
    /// `node_modules/<name>`, the workspace tree must NOT emit a
    /// redundant `<workspace>/node_modules/<name>` for the same
    /// version — Node's upward `node_modules` walk resolves the root
    /// copy. Real `npm install` omits the redundant entry, and
    /// emitting it produces a diff on every round-trip.
    #[test]
    fn test_write_npm_workspace_skips_root_hoisted_dups() {
        let mut graph = LockfileGraph::default();
        let web_link = LocalSource::Link(PathBuf::from("web"));
        let web_dep_path = web_link.dep_path("workspace-web");
        graph.packages.insert(
            web_dep_path.clone(),
            LockedPackage {
                name: "workspace-web".to_string(),
                version: "0.0.1".to_string(),
                dep_path: web_dep_path.clone(),
                local_source: Some(web_link),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "astro@6.2.1".to_string(),
            LockedPackage {
                name: "astro".to_string(),
                version: "6.2.1".to_string(),
                integrity: Some("sha512-astro".to_string()),
                dep_path: "astro@6.2.1".to_string(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "astro".to_string(),
                    dep_path: "astro@6.2.1".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^6.0.0".to_string()),
                },
                DirectDep {
                    name: "workspace-web".to_string(),
                    dep_path: web_dep_path.clone(),
                    dep_type: DepType::Production,
                    specifier: None,
                },
            ],
        );
        graph.importers.insert(
            "web".to_string(),
            vec![DirectDep {
                name: "astro".to_string(),
                dep_path: "astro@6.2.1".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^6.0.0".to_string()),
            }],
        );

        let manifest = aube_manifest::PackageJson {
            name: Some("workspace-root".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [("astro".to_string(), "^6.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(out.path()).unwrap()).unwrap();
        assert_eq!(json["packages"]["node_modules/astro"]["version"], "6.2.1");
        assert!(
            json["packages"].get("web/node_modules/astro").is_none(),
            "redundant workspace-nested astro should not be emitted"
        );
    }

    /// Byte-parity with a real `npm install`-generated lockfile. The
    /// fixture at `tests/fixtures/npm-native.json` was produced by
    /// `npm install` (v11) against a `{ chalk, picocolors, semver }`
    /// manifest. A parse → write round-trip must reproduce the exact
    /// bytes. Covers `resolved:` on every entry, `license:` /
    /// `engines:` / `bin:` / `funding:` field preservation, and the
    /// sibling declared-range preservation that rides on
    /// `declared_dependencies`.
    #[test]
    fn test_write_byte_identical_to_native_npm() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/npm-native.json");
        // Same LF normalization as the pnpm / bun byte-parity tests —
        // Windows' `core.autocrlf=true` rewrites the checked-out
        // fixture to CRLF even with `.gitattributes eol=lf`.
        let original = std::fs::read_to_string(&fixture)
            .unwrap()
            .replace("\r\n", "\n");
        let graph = parse(&fixture).unwrap();
        let manifest = aube_manifest::PackageJson {
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

        let tmp = tempfile::NamedTempFile::new().unwrap();
        write(tmp.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(tmp.path()).unwrap();

        if written != original {
            panic!(
                "npm writer drifted from native npm output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
            );
        }
    }

    #[test]
    fn test_parse_workspace_links() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "dependencies": { "@scope/app": "file:packages/app" }
                },
                "node_modules/@scope/app": {
                    "resolved": "packages/app",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "packages/app": {
                    "name": "@scope/app",
                    "version": "0.68.1",
                    "dependencies": {
                        "chalk": "^5.4.1"
                    }
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let dep_path = LocalSource::Link(PathBuf::from("packages/app")).dep_path("@scope/app");

        let importer = &graph.importers["."];
        assert_eq!(importer.len(), 1);
        assert_eq!(importer[0].name, "@scope/app");
        assert_eq!(importer[0].dep_path, dep_path);
        assert!(matches!(importer[0].dep_type, DepType::Production));
        assert!(importer[0].specifier.is_none());

        let app = &graph.packages[&importer[0].dep_path];
        assert_eq!(app.version, "0.68.1");
        assert_eq!(
            app.local_source,
            Some(LocalSource::Link(PathBuf::from("packages/app")))
        );
        assert_eq!(
            app.dependencies.get("chalk").map(String::as_str),
            Some("5.4.1")
        );
        assert!(!graph.packages.contains_key("@scope/app@0.68.1"));
    }

    /// npm workspaces that aren't listed in the root manifest's
    /// `dependencies`/`devDependencies` still get a `node_modules/<name>`
    /// link entry in the lockfile — npm symlinks every workspace member
    /// at the workspace root regardless. The siemens/element repo
    /// (https://github.com/siemens/element) hits this: its 11 workspace
    /// projects under `projects/*` aren't declared as deps of the root
    /// `package.json`, so the linker had nothing to link at the root and
    /// `node_modules/@siemens/element-ng` (and friends) silently went
    /// missing — breaking Angular CLI builds that resolve workspace
    /// libraries from the repo root.
    #[test]
    fn test_parse_workspace_links_undeclared_in_root_deps() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "workspace-root",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "workspace-root",
                    "version": "1.0.0",
                    "workspaces": ["projects/element-ng", "projects/charts-ng"],
                    "dependencies": { "chalk": "^5.4.1" }
                },
                "node_modules/@siemens/element-ng": {
                    "resolved": "projects/element-ng",
                    "link": true
                },
                "node_modules/@siemens/charts-ng": {
                    "resolved": "projects/charts-ng",
                    "link": true
                },
                "node_modules/chalk": {
                    "version": "5.4.1",
                    "integrity": "sha512-chalk"
                },
                "projects/element-ng": {
                    "name": "@siemens/element-ng",
                    "version": "21.0.0"
                },
                "projects/charts-ng": {
                    "name": "@siemens/charts-ng",
                    "version": "21.0.0"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        let importer = &graph.importers["."];

        let names: Vec<&str> = importer.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"chalk"));
        assert!(
            names.contains(&"@siemens/element-ng"),
            "workspace package `@siemens/element-ng` should be a direct dep of root \
             so the linker creates `node_modules/@siemens/element-ng`, even though \
             the root manifest doesn't list it; got importer deps {names:?}"
        );
        assert!(
            names.contains(&"@siemens/charts-ng"),
            "workspace package `@siemens/charts-ng` should be a direct dep of root; \
             got importer deps {names:?}"
        );

        // Each workspace dep_path round-trips through LocalSource::Link.
        let element_ng = importer
            .iter()
            .find(|d| d.name == "@siemens/element-ng")
            .unwrap();
        assert_eq!(
            graph.packages[&element_ng.dep_path].local_source,
            Some(LocalSource::Link(PathBuf::from("projects/element-ng")))
        );
    }

    /// npm copies `funding:` verbatim from each package's
    /// `package.json`, so all three registry-permitted shapes (bare
    /// string, `{url}` object, mixed array of either) appear in real
    /// lockfiles. The pre-fix parser only accepted the object form
    /// and would hard-fail on any project pulling in `htmlparser2`,
    /// `@csstools/*`, etc. Aube only carries one URL per package, so
    /// the contract is "first URL wins, no shape rejected".
    #[test]
    fn test_parse_funding_all_shapes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
            "name": "test",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "test",
                    "version": "1.0.0",
                    "dependencies": {
                        "string-funding": "1.0.0",
                        "object-funding": "1.0.0",
                        "array-funding": "1.0.0",
                        "mixed-array-funding": "1.0.0",
                        "no-funding": "1.0.0"
                    }
                },
                "node_modules/string-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-aaa",
                    "funding": "https://example.com/sponsor"
                },
                "node_modules/object-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-bbb",
                    "funding": { "type": "github", "url": "https://github.com/sponsors/foo" }
                },
                "node_modules/array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ccc",
                    "funding": [
                        { "type": "github", "url": "https://github.com/sponsors/csstools" },
                        { "type": "opencollective", "url": "https://opencollective.com/csstools" }
                    ]
                },
                "node_modules/mixed-array-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-ddd",
                    "funding": [
                        "https://github.com/fb55/htmlparser2?sponsor=1",
                        { "type": "github", "url": "https://github.com/sponsors/fb55" }
                    ]
                },
                "node_modules/no-funding": {
                    "version": "1.0.0",
                    "integrity": "sha512-eee"
                }
            }
        }"#;
        std::fs::write(tmp.path(), content).unwrap();

        let graph = parse(tmp.path()).unwrap();
        assert_eq!(
            graph.packages["string-funding@1.0.0"]
                .funding_url
                .as_deref(),
            Some("https://example.com/sponsor"),
        );
        assert_eq!(
            graph.packages["object-funding@1.0.0"]
                .funding_url
                .as_deref(),
            Some("https://github.com/sponsors/foo"),
        );
        // Array form: aube collapses to the first URL.
        assert_eq!(
            graph.packages["array-funding@1.0.0"].funding_url.as_deref(),
            Some("https://github.com/sponsors/csstools"),
        );
        // Mixed array (bare string + object): first element is a
        // string, so its value is the URL.
        assert_eq!(
            graph.packages["mixed-array-funding@1.0.0"]
                .funding_url
                .as_deref(),
            Some("https://github.com/fb55/htmlparser2?sponsor=1"),
        );
        assert!(graph.packages["no-funding@1.0.0"].funding_url.is_none());
    }
}
