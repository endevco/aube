//! Parser for bun's `bun.lock` (text JSONC format, bun 1.1+).
//!
//! The `bun.lockb` binary format is NOT supported — users should run
//! `bun install --save-text-lockfile` first (or upgrade to bun 1.2+
//! where text is the default).
//!
//! Format overview:
//!
//! ```jsonc
//! {
//!   "lockfileVersion": 1,
//!   "workspaces": {
//!     "": {
//!       "name": "my-app",
//!       "dependencies": { "foo": "^1.0.0" },
//!       "devDependencies": { "bar": "^2.0.0" }
//!     }
//!   },
//!   "packages": {
//!     "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "sha512-..."],
//!     "nested": ["nested@3.1.0", "", {}, "sha512-..."]
//!   }
//! }
//! ```
//!
//! Each `packages` entry is a 4-tuple `[ident, resolved_url, metadata, integrity]`,
//! where `ident` is `name@version` and `metadata` may carry transitive
//! `dependencies` / `optionalDependencies`.
//!
//! The file uses JSONC: trailing commas and `//`/`/* */` comments are
//! allowed. We pre-process the content to strip those before handing it
//! to `serde_json`.

use crate::{
    DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph, PeerDepMeta,
    RemoteTarballSource,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Deserialize)]
struct RawBunLockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    /// bun 1.2+ emits a `configVersion:` field alongside
    /// `lockfileVersion:`. Default to `1` for older lockfiles that
    /// predate it so a v1.1 file round-trips without the field
    /// suddenly appearing.
    #[serde(default = "default_config_version", rename = "configVersion")]
    config_version: u32,
    #[serde(default)]
    workspaces: BTreeMap<String, RawBunWorkspace>,
    #[serde(default)]
    packages: BTreeMap<String, Vec<serde_json::Value>>,
    /// bun 1.1+ top-level `overrides:` block (mirrors the key under
    /// the same name in package.json). Map of selector → version.
    #[serde(default)]
    overrides: BTreeMap<String, String>,
    /// bun 1.1+ top-level `patchedDependencies:` block. Map of
    /// `pkg@version` selector → relative patch file path.
    #[serde(default, rename = "patchedDependencies")]
    patched_dependencies: BTreeMap<String, String>,
    /// bun 1.1+ top-level `trustedDependencies:` — a package-name
    /// allowlist for lifecycle script execution.
    #[serde(default, rename = "trustedDependencies")]
    trusted_dependencies: Vec<String>,
    /// bun 1.2+ unnamed catalog (`catalog: { foo: "^1.0.0" }`).
    /// Pairs with pnpm's `catalog:` in `pnpm-workspace.yaml`.
    #[serde(default)]
    catalog: BTreeMap<String, String>,
    /// bun 1.2+ named catalogs (`catalogs: { evens: { foo: "^2" } }`).
    #[serde(default)]
    catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Unknown top-level fields preserved verbatim so a future bun
    /// bump (or anything hand-authored we don't model) round-trips
    /// without getting silently stripped.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

fn default_config_version() -> u32 {
    1
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBunWorkspace {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    /// Unknown per-workspace fields (`name`, `version`, `bin`,
    /// `peerDependencies`, `optionalPeers`, and anything else bun
    /// adds in a future release) preserved verbatim. The writer's
    /// ws-extras fallback re-emits them so bun-authored workspace
    /// peer data round-trips even when the manifest isn't
    /// authoritative for the importer.
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// Decoded view of one bun.lock package entry.
///
/// bun uses different tuple shapes depending on where the package came
/// from:
///   - Registry: `[ident, resolved_url, { meta }, "sha512-..."]`
///   - Git / github: `[ident, { meta }, "owner-repo-commit"]`
///   - Workspace / link / file: `[ident]` or `[ident, { meta }]`
///
/// We introspect by element type rather than position: the metadata
/// object is the sole `Object` in the array, and an integrity hash is
/// recognized by its `sha…-` prefix.
#[derive(Debug, Default)]
struct BunEntry {
    ident: String,
    meta: RawBunMeta,
    integrity: Option<String>,
}

impl BunEntry {
    fn from_array(key: &str, arr: &[serde_json::Value]) -> Result<Self, String> {
        let ident = arr
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("package '{key}' has no ident string at position 0"))?
            .to_string();

        let mut meta = RawBunMeta::default();
        let mut integrity: Option<String> = None;
        for el in arr.iter().skip(1) {
            match el {
                serde_json::Value::Object(_) => {
                    meta = serde_json::from_value(el.clone()).unwrap_or_default();
                }
                serde_json::Value::String(s) if is_integrity_hash(s) => {
                    integrity = Some(s.clone());
                }
                _ => {}
            }
        }

        Ok(Self {
            ident,
            meta,
            integrity,
        })
    }
}

/// Recognize an SRI-style integrity hash (`<algo>-<base64>`).
///
/// The prefix check alone isn't enough: a github entry's trailing
/// `owner-repo-shortsha` could start with a literal `sha1`/`sha256`/etc.
/// if that's the owner name. A real SRI hash also has a fixed base64
/// body length for each algo, and base64 never uses `-`, so
/// `sha1-myrepo-abc123` fails both the length and charset checks.
fn is_integrity_hash(s: &str) -> bool {
    let Some((algo, body)) = s.split_once('-') else {
        return false;
    };
    // Accept sha1 and md5 at the parser layer so bun lockfiles that
    // reference pre-2017 npm packages (whose `dist.integrity` is only
    // ever sha1) still round-trip without losing the hash. Downgrade
    // enforcement lives at verify time in `aube-store::verify_integrity`,
    // which already refuses anything but sha512 for content verification.
    let expected_len = match algo {
        "sha512" => 88,
        "sha384" => 64,
        "sha256" => 44,
        "sha1" => 28,
        "md5" => 24,
        _ => return false,
    };
    if body.len() != expected_len {
        return false;
    }
    body.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBunMeta {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
    /// bun records peer declarations on the meta block in the same
    /// shape as `dependencies`. Keeping them typed lets the writer
    /// emit them back in bun's native field order; anything we don't
    /// have an explicit slot for drops through to `extra` below.
    #[serde(default)]
    peer_dependencies: BTreeMap<String, String>,
    /// Compact list form of `peerDependenciesMeta[name].optional:
    /// true` — bun's preferred representation on per-entry meta.
    #[serde(default)]
    optional_peers: Vec<String>,
    /// `bin:` map — bun records executables by name on each package's
    /// meta block (`{ "bin": { "semver": "bin/semver.js" } }`). Round-
    /// tripping it is what keeps `aube install --no-frozen-lockfile`
    /// from silently dropping the `bin:` line and drifting against
    /// bun's own output.
    #[serde(default)]
    bin: serde_json::Value,
    /// Platform filters — bun writes arrays of `os` / `cpu` / `libc`
    /// entries on meta blocks for optional platform packages.
    #[serde(default)]
    os: Vec<String>,
    #[serde(default)]
    cpu: Vec<String>,
    #[serde(default)]
    libc: Vec<String>,
    /// Unknown per-entry meta fields preserved for round-trip
    /// (`deprecated`, `hasInstallScript`, anything new bun adds).
    #[serde(flatten)]
    extra: BTreeMap<String, serde_json::Value>,
}

/// Parse a bun.lock file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let raw_content = crate::read_lockfile(path)?;
    let cleaned = strip_jsonc(&raw_content);
    // `strip_jsonc` preserves byte offsets, so a serde_json error on
    // `cleaned` points at the same byte in `raw_content`. Feed the
    // raw file into the `NamedSource` so miette renders the user's
    // actual bun.lock (including comments) under the pointer.
    debug_assert_eq!(raw_content.len(), cleaned.len());

    let raw: RawBunLockfile = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(e) => return Err(Error::parse_json_err(path, raw_content, &e)),
    };

    if raw.lockfile_version != 1 {
        return Err(Error::parse(
            path,
            format!(
                "bun.lock lockfileVersion {} is not supported (expected 1)",
                raw.lockfile_version
            ),
        ));
    }

    // Decode each raw array into a typed BunEntry so later passes don't
    // have to think about bun's per-source-type tuple layouts.
    let mut entries: BTreeMap<String, BunEntry> = BTreeMap::new();
    for (key, value) in &raw.packages {
        let entry = BunEntry::from_array(key, value).map_err(|e| Error::parse(path, e))?;
        entries.insert(key.clone(), entry);
    }

    // First pass: parse (name, version) for each entry. bun.lock keys look
    // like the package name ("foo") for the hoisted version, or a nested
    // path ("parent/foo") when multiple versions exist.
    let mut key_info: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for (key, entry) in &entries {
        let Some((raw_name, raw_version)) = split_ident(&entry.ident) else {
            return Err(Error::parse(
                path,
                format!(
                    "could not parse ident '{}' for package '{}'",
                    entry.ident, key
                ),
            ));
        };

        // Detect non-registry specifiers embedded in bun's ident form
        // (`foo@github:user/repo#sha`, `foo@file:./vendor`,
        // `foo@https://…/pkg.tgz`, `foo@workspace:*`, …). The bun key
        // is always the alias-side name; the ident carries the
        // registry identity when bun wrote an npm-alias entry
        // (`foo@npm:real@1.2.3`). Reconstructing a `LocalSource`
        // here keeps the installer from re-routing every such entry
        // through the default registry and either 404-ing or
        // downloading the wrong tarball.
        let alias_name = bun_key_to_alias_name(key);
        let (name, version, local_source, alias_of) = classify_bun_ident(
            &alias_name,
            &raw_name,
            &raw_version,
            entry.integrity.as_deref(),
            path,
        )?;
        key_info.insert(key.clone(), (name.clone(), version.clone()));

        let dep_path = format!("{name}@{version}");

        // Skip duplicate entries pointing at the same resolved package.
        if packages.contains_key(&dep_path) {
            continue;
        }

        // Collect transitive dep names; resolve to dep_paths in a second pass.
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for n in entry
            .meta
            .dependencies
            .keys()
            .chain(entry.meta.optional_dependencies.keys())
        {
            deps.insert(n.clone(), String::new());
        }
        // Track which of those are optionals so the writer can split
        // them back into `optionalDependencies:` instead of dumping
        // everything under `dependencies:` on re-emit.
        let mut optional_deps: BTreeMap<String, String> = BTreeMap::new();
        for n in entry.meta.optional_dependencies.keys() {
            optional_deps.insert(n.clone(), String::new());
        }
        // Preserve bun's per-entry meta ranges (`"^4.1.0"`) so re-emit
        // doesn't collapse them to the resolved pin.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in entry
            .meta
            .dependencies
            .iter()
            .chain(entry.meta.optional_dependencies.iter())
        {
            declared.insert(k.clone(), v.clone());
        }

        // Normalize bun's `bin` meta into the typed BTreeMap while
        // preserving the raw shape (string vs object) on `extra_meta`
        // so the writer can echo the original representation back.
        let bin_map = bin_value_to_map(&name, &entry.meta.bin);
        let mut extra_meta = entry.meta.extra.clone();
        if !matches!(&entry.meta.bin, serde_json::Value::Null) {
            extra_meta.insert("bin".to_string(), entry.meta.bin.clone());
        }
        if !entry.meta.optional_peers.is_empty() {
            extra_meta.insert(
                "optionalPeers".to_string(),
                serde_json::Value::Array(
                    entry
                        .meta
                        .optional_peers
                        .iter()
                        .map(|s| serde_json::Value::String(s.clone()))
                        .collect(),
                ),
            );
        }

        // Peer declarations survive on their typed slot so drift
        // detection sees them; the meta map round-trip survives
        // through `extra_meta` for anything we don't model.
        let peer_dependencies = entry.meta.peer_dependencies.clone();
        let peer_dependencies_meta: BTreeMap<String, PeerDepMeta> = entry
            .meta
            .optional_peers
            .iter()
            .map(|n| (n.clone(), PeerDepMeta { optional: true }))
            .collect();

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity: entry.integrity.clone().filter(|s| !s.is_empty()),
                dependencies: deps,
                optional_dependencies: optional_deps,
                peer_dependencies,
                peer_dependencies_meta,
                dep_path,
                local_source,
                alias_of,
                os: entry.meta.os.iter().cloned().collect(),
                cpu: entry.meta.cpu.iter().cloned().collect(),
                libc: entry.meta.libc.iter().cloned().collect(),
                declared_dependencies: declared,
                bin: bin_map,
                extra_meta,
                ..Default::default()
            },
        );
    }

    // Second pass: resolve transitive deps by walking the bun nesting
    // hierarchy — for an entry at key "parent/foo", dep "bar" resolves to
    // "parent/foo/bar" → "parent/bar" → "bar".
    let mut resolved_by_dep_path: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (key, entry) in &entries {
        let Some((name, version)) = key_info.get(key) else {
            continue;
        };
        let dep_path = format!("{name}@{version}");
        if resolved_by_dep_path.contains_key(&dep_path) {
            continue;
        }

        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        for dep_name in entry
            .meta
            .dependencies
            .keys()
            .chain(entry.meta.optional_dependencies.keys())
        {
            if let Some(target_key) = resolve_nested_bun(key, dep_name, &key_info)
                && let Some((dname, dver)) = key_info.get(&target_key)
            {
                resolved.insert(dep_name.clone(), format!("{dname}@{dver}"));
            }
        }
        resolved_by_dep_path.insert(dep_path, resolved);
    }
    for (dep_path, deps) in resolved_by_dep_path {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            // Transfer resolved dep_paths onto `dependencies` (the
            // combined map) and onto `optional_dependencies` for the
            // subset the parser flagged on first pass. Matches the
            // pnpm parser's split so every downstream consumer
            // (linker, writer, drift detection) sees the same shape
            // regardless of source format.
            let mut opts = BTreeMap::new();
            for name in pkg
                .optional_dependencies
                .keys()
                .cloned()
                .collect::<Vec<_>>()
            {
                if let Some(resolved) = deps.get(&name) {
                    opts.insert(name.clone(), resolved.clone());
                }
            }
            pkg.dependencies = deps;
            pkg.optional_dependencies = opts;
        }
    }

    // Workspace importers. bun.lock keys workspace paths as `""` for
    // the root and relative paths (`packages/app`, etc.) for each
    // workspace package. Each importer's direct deps resolve first
    // to a workspace-scoped override (`packages/app/foo`) when one
    // exists, falling back to the hoisted entry (`foo`). We don't
    // walk intermediate ancestors like `packages/foo` the way
    // `resolve_nested_bun` does for package-nesting — workspace path
    // segments are directories, not package-nesting scopes, so a
    // partial walk could wrongly match a literal npm package named
    // `packages` that has its own nested `foo` entry.
    let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    let mut workspace_extra_fields: BTreeMap<String, BTreeMap<String, serde_json::Value>> =
        BTreeMap::new();
    for (ws_path, ws_raw) in &raw.workspaces {
        let importer_path = if ws_path.is_empty() {
            ".".to_string()
        } else {
            ws_path.clone()
        };
        let mut direct: Vec<DirectDep> = Vec::new();
        let push_dep =
            |name: &str, specifier: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
                if let Some(target_key) = resolve_workspace_dep(ws_path, name, &key_info)
                    && let Some((dname, dver)) = key_info.get(&target_key)
                {
                    direct.push(DirectDep {
                        name: dname.clone(),
                        dep_path: format!("{dname}@{dver}"),
                        dep_type,
                        specifier: Some(specifier.to_string()),
                    });
                }
            };
        for (n, spec) in &ws_raw.dependencies {
            push_dep(n, spec, DepType::Production, &mut direct);
        }
        for (n, spec) in &ws_raw.dev_dependencies {
            push_dep(n, spec, DepType::Dev, &mut direct);
        }
        for (n, spec) in &ws_raw.optional_dependencies {
            push_dep(n, spec, DepType::Optional, &mut direct);
        }
        importers.insert(importer_path.clone(), direct);
        if !ws_raw.extra.is_empty() {
            workspace_extra_fields.insert(importer_path, ws_raw.extra.clone());
        }
    }
    // The `importers` map always needs a `.` entry even when the
    // lockfile omits the `""` workspace entirely (hand-authored
    // fixtures sometimes do).
    importers.entry(".".to_string()).or_default();

    // Translate bun's unnamed `catalog:` / named `catalogs:` blocks
    // into the shared `LockfileGraph.catalogs` shape — outer key is
    // the catalog name (`default` for the unnamed one), inner key is
    // the package name. We don't have a separate resolved version on
    // bun's side, so the `specifier` and `version` track the same
    // value (the declared range); refreshing the catalog at resolve
    // time rewrites `version` to the picked pin.
    let mut catalogs_map: BTreeMap<String, BTreeMap<String, crate::CatalogEntry>> = BTreeMap::new();
    if !raw.catalog.is_empty() {
        let inner = raw
            .catalog
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::CatalogEntry {
                        specifier: v.clone(),
                        version: v.clone(),
                    },
                )
            })
            .collect();
        catalogs_map.insert("default".to_string(), inner);
    }
    for (catalog_name, entries) in &raw.catalogs {
        let inner = entries
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    crate::CatalogEntry {
                        specifier: v.clone(),
                        version: v.clone(),
                    },
                )
            })
            .collect();
        catalogs_map.insert(catalog_name.clone(), inner);
    }

    Ok(LockfileGraph {
        importers,
        packages,
        bun_config_version: Some(raw.config_version),
        overrides: raw.overrides,
        patched_dependencies: raw.patched_dependencies,
        // Preserve bun's insertion order verbatim — dedupe to guard
        // against a hand-authored lockfile with repeats but never
        // reorder, so a re-emit is byte-identical to bun's own output.
        trusted_dependencies: {
            let mut seen = BTreeSet::new();
            let mut out: Vec<String> = Vec::with_capacity(raw.trusted_dependencies.len());
            for name in raw.trusted_dependencies {
                if seen.insert(name.clone()) {
                    out.push(name);
                }
            }
            out
        },
        catalogs: catalogs_map,
        extra_fields: raw.extra,
        workspace_extra_fields,
        ..Default::default()
    })
}

/// Extract the alias name bun uses as the hoist key. bun's `packages`
/// key is `<alias_name>` hoisted or `<parent>/<alias_name>` nested,
/// where `alias_name` matches `package.json`'s dep key verbatim.
fn bun_key_to_alias_name(key: &str) -> String {
    if let Some(last_slash) = key.rfind('/') {
        // Scoped names like `@scope/name` are a single unit — if the
        // slice before the last slash is itself `@scope`, keep the
        // whole suffix.
        let tail_start = key[..last_slash].rfind('/').map(|i| i + 1).unwrap_or(0);
        if key[tail_start..last_slash].starts_with('@') {
            key[tail_start..].to_string()
        } else {
            key[last_slash + 1..].to_string()
        }
    } else {
        key.to_string()
    }
}

/// Classify a bun ident's version tail as a registry pin, an npm alias
/// target, or a non-registry source (git, file, link, workspace, http
/// tarball). Returns `(name, version, local_source, alias_of)`.
///
/// - `alias_name` is the hoist key (bun's left-hand side).
/// - `raw_name` / `raw_version` come from `split_ident()` on the ident
///   (the right-hand side of the tuple's position 0).
///
/// The alias name wins as `LockedPackage.name` whenever it differs
/// from the ident's name (npm-alias case). `alias_of` records the
/// registry-side name only then.
fn classify_bun_ident(
    alias_name: &str,
    raw_name: &str,
    raw_version: &str,
    integrity: Option<&str>,
    _path: &Path,
) -> Result<(String, String, Option<LocalSource>, Option<String>), Error> {
    // npm-alias tail: bun writes the registry identity into the ident,
    // so the raw name is the real registry name and the alias key is
    // the hoist name.
    let alias_of = if alias_name != raw_name {
        Some(raw_name.to_string())
    } else {
        None
    };
    let name = alias_name.to_string();

    // Non-registry tails.
    if raw_version.starts_with("workspace:") {
        let rel = raw_version.strip_prefix("workspace:").unwrap_or("");
        // `workspace:*` / `workspace:^` / `workspace:~` are version-
        // range selectors, not directory paths — a `PathBuf::from("*")`
        // would silently become `{project_root}/*` under any caller
        // that does `project_root.join(link.path())`. Only treat the
        // tail as a path when it looks like one (leading `.` or `/`);
        // otherwise fall back to `.` so the link points at the
        // workspace root and the caller resolves the actual location
        // from the graph's workspace map.
        let is_path = rel.starts_with('.') || rel.starts_with('/');
        let path_buf = std::path::PathBuf::from(if rel.is_empty() || !is_path { "." } else { rel });
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Link(path_buf)),
            alias_of,
        ));
    }
    if let Some(rest) = raw_version.strip_prefix("github:") {
        let (url, committish) = split_committish(rest);
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Git(GitSource {
                url: format!("https://github.com/{url}.git"),
                committish: committish.clone(),
                resolved: committish.unwrap_or_default(),
            })),
            alias_of,
        ));
    }
    if (raw_version.starts_with("git+")
        || raw_version.starts_with("git://")
        || raw_version.starts_with("git@"))
        && let Some((url, committish)) = crate::parse_git_spec(raw_version)
    {
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Git(GitSource {
                url,
                committish: committish.clone(),
                resolved: committish.unwrap_or_default(),
            })),
            alias_of,
        ));
    }
    if raw_version.starts_with("http://") || raw_version.starts_with("https://") {
        // Mirror the sibling `LockedPackage.integrity` hash onto the
        // `RemoteTarballSource` so consumers of
        // `local_source.specifier()` or integrity-verification paths
        // see the same value — leaving it empty would make the two
        // fields drift apart for the same entry.
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::RemoteTarball(RemoteTarballSource {
                url: raw_version.to_string(),
                integrity: integrity.map(str::to_string).unwrap_or_default(),
            })),
            alias_of,
        ));
    }
    if let Some(rest) = raw_version.strip_prefix("file:") {
        let rel = std::path::PathBuf::from(rest);
        let kind = if LocalSource::path_looks_like_tarball(&rel) {
            LocalSource::Tarball(rel)
        } else {
            LocalSource::Directory(rel)
        };
        return Ok((name, raw_version.to_string(), Some(kind), alias_of));
    }
    if let Some(rest) = raw_version.strip_prefix("link:") {
        return Ok((
            name,
            raw_version.to_string(),
            Some(LocalSource::Link(std::path::PathBuf::from(rest))),
            alias_of,
        ));
    }
    // Plain registry pin.
    Ok((name, raw_version.to_string(), None, alias_of))
}

fn split_committish(spec: &str) -> (String, Option<String>) {
    match spec.rfind('#') {
        Some(i) => (spec[..i].to_string(), Some(spec[i + 1..].to_string())),
        None => (spec.to_string(), None),
    }
}

/// Normalize bun's `bin` meta (either a single-string form or a
/// `{name: path}` object) into the typed BTreeMap LockedPackage uses.
/// String form defaults the bin name to `default_name` (the package
/// name), matching npm's own fallback when `package.json` writes
/// `"bin": "./foo.js"` shorthand.
fn bin_value_to_map(default_name: &str, value: &serde_json::Value) -> BTreeMap<String, String> {
    match value {
        serde_json::Value::String(s) => {
            let mut map = BTreeMap::new();
            map.insert(default_name.to_string(), s.clone());
            map
        }
        serde_json::Value::Object(obj) => obj
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => BTreeMap::new(),
    }
}

impl Clone for RawBunWorkspace {
    fn clone(&self) -> Self {
        Self {
            dependencies: self.dependencies.clone(),
            dev_dependencies: self.dev_dependencies.clone(),
            optional_dependencies: self.optional_dependencies.clone(),
            extra: self.extra.clone(),
        }
    }
}

/// Resolve a transitive dep from the perspective of a bun.lock entry at
/// key `pkg_key`. bun.lock uses slash-delimited keys for nested overrides:
/// an entry at "parent/foo" means "foo" is nested inside "parent" because
/// the hoisted version didn't satisfy parent's range.
///
/// We walk up the key's ancestors, first checking the package's own nested
/// scope then each ancestor's, finally falling back to the hoisted entry
/// at just the bare `dep_name`.
fn resolve_nested_bun(
    pkg_key: &str,
    dep_name: &str,
    key_info: &BTreeMap<String, (String, String)>,
) -> Option<String> {
    let mut base = pkg_key.to_string();
    loop {
        let candidate = if base.is_empty() {
            dep_name.to_string()
        } else {
            format!("{base}/{dep_name}")
        };
        if key_info.contains_key(&candidate) {
            return Some(candidate);
        }
        if base.is_empty() {
            return None;
        }
        // Strip the trailing package segment. For scoped packages we need
        // to strip "@scope/name" as a single unit.
        if let Some(idx) = base.rfind('/') {
            // If the base ends with "@scope/name", we need to check if the
            // segment before the "/" starts with '@' — if so, strip that full
            // "@scope/name" tail. Otherwise strip just the trailing segment.
            let tail_start = base[..idx].rfind('/').map(|i| i + 1).unwrap_or(0);
            if base[tail_start..idx].starts_with('@') {
                base.truncate(tail_start.saturating_sub(1));
            } else {
                base.truncate(idx);
            }
        } else {
            base.clear();
        }
    }
}

/// Resolve a direct dep of a workspace importer at path `ws_path`
/// (e.g. `""` for root, `"packages/app"` for a nested workspace) to
/// its `key_info` key. Checks the workspace-scoped override
/// (`<ws_path>/<dep_name>`) first, then the hoisted bare key
/// (`<dep_name>`). Intentionally does *not* walk intermediate
/// ancestors like `packages/<dep_name>` — those are
/// package-nesting keys that belong to `resolve_nested_bun`, and
/// partial matches there could spuriously resolve to a literal npm
/// package named `packages` that happened to carry its own nested
/// entry.
fn resolve_workspace_dep(
    ws_path: &str,
    dep_name: &str,
    key_info: &BTreeMap<String, (String, String)>,
) -> Option<String> {
    if !ws_path.is_empty() {
        let ws_specific = format!("{ws_path}/{dep_name}");
        if key_info.contains_key(&ws_specific) {
            return Some(ws_specific);
        }
    }
    if key_info.contains_key(dep_name) {
        return Some(dep_name.to_string());
    }
    None
}

/// Split a bun ident like `foo@1.2.3` or `@scope/pkg@1.2.3` into `(name, version)`.
fn split_ident(ident: &str) -> Option<(String, String)> {
    if let Some(rest) = ident.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let name = format!("@{}", &rest[..slash + 1 + at]);
        let version = after_slash[at + 1..].to_string();
        Some((name, version))
    } else {
        let at = ident.find('@')?;
        Some((ident[..at].to_string(), ident[at + 1..].to_string()))
    }
}

/// Strip JSONC features (line comments, block comments, trailing commas)
/// to produce valid JSON. Respects string literals.
///
/// Output length is byte-identical to the input — comment bytes and
/// trailing commas become spaces (newlines inside block comments are
/// preserved). That keeps every byte offset in `cleaned` pointing at
/// the same byte in the original file, so a `serde_json` parse error
/// on the stripped buffer lines up with the user's editor line/column
/// when rendered against the original source via `miette`'s fancy
/// handler.
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < bytes.len() {
        let c = bytes[i];

        if in_string {
            out.push(c as char);
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        // Line comment: replace every byte up to (not including) the
        // newline with a space. The `\n` itself is kept.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }

        // Block comment: replace every byte with a space, but keep
        // embedded newlines so line numbers don't shift.
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            out.push(' ');
            out.push(' ');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i + 1 < bytes.len() {
                // consume the closing `*/`
                out.push(' ');
                out.push(' ');
                i += 2;
            } else {
                // unterminated block comment — mirror every remaining
                // byte to preserve length, keeping newlines intact.
                while i < bytes.len() {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            continue;
        }

        // Trailing comma: replace `,` with a space when the next
        // non-whitespace char is `}` or `]`.
        if c == b',' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                out.push(' ');
                i += 1;
                continue;
            }
        }

        if c == b'"' {
            in_string = true;
        }

        out.push(c as char);
        i += 1;
    }

    out
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → bun.lock (text / JSONC v1)
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a bun v1 text lockfile.
///
/// Shares the hoist + nest algorithm with the npm writer via
/// [`crate::npm::build_hoist_tree`]. The segment list per entry is
/// rendered as bun's slash-delimited key form (`foo` or `parent/foo`),
/// and each entry body is a 4-tuple array
/// `[ident, resolved, metadata, integrity]` matching the parser.
///
/// Non-root workspace importers are emitted under their relative
/// project paths (e.g. `packages/app`) by reading each
/// `{importer}/package.json` from disk. The `packages` section is
/// built from the union of every importer's direct deps so workspace-
/// only transitive deps still get keyed into the hoist tree; workspace
/// packages themselves (identified by a `LocalSource::Link`) are
/// filtered out because bun tracks them separately in `workspaces`.
///
/// Lossy areas (same family as the npm writer):
///   - `resolved` is written as an empty string — we don't persist
///     origin URLs in [`LockedPackage`]. bun reparse is unaffected
///     because its parser explicitly ignores field 1.
///   - Peer-contextualized variants collapse to a single
///     `name@version` entry.
pub fn write(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    use serde_json::{Value, json};

    // Canonicalize to one entry per (name, version). Skip workspace
    // packages (LocalSource::Link) — bun tracks those via the
    // `workspaces` map, not as top-level `packages` entries.
    let mut canonical: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    for pkg in graph.packages.values() {
        if matches!(pkg.local_source, Some(LocalSource::Link(_))) {
            continue;
        }
        canonical.entry(pkg.spec_key()).or_insert(pkg);
    }

    // Build the hoist tree from every importer's direct deps (not just
    // the root's), so transitive deps declared only by a non-root
    // workspace still appear in the `packages` section. Skip
    // workspace-link deps for the same reason as the canonical filter.
    //
    // Dedupe by package name so duplicate direct deps across
    // workspaces don't confuse `build_hoist_tree` — its root-seeding
    // loop silently drops any queue entry whose segs already exist in
    // `placed`, which would mean the second workspace's transitive
    // deps never get walked. `graph.importers` is a BTreeMap, so `.`
    // iterates first and wins conflicts. When two workspaces declare
    // the same dep at different versions we still collapse to a
    // single top-level entry (the first-seen version); a proper fix
    // would emit `<workspace>/<dep>` nested entries per-workspace,
    // which is out of scope here.
    let mut all_roots: Vec<DirectDep> = Vec::new();
    let mut seen_names: BTreeSet<String> = BTreeSet::new();
    for deps in graph.importers.values() {
        for d in deps {
            if matches!(
                graph
                    .packages
                    .get(&d.dep_path)
                    .and_then(|p| p.local_source.as_ref()),
                Some(LocalSource::Link(_))
            ) {
                continue;
            }
            if !seen_names.insert(d.name.clone()) {
                continue;
            }
            all_roots.push(d.clone());
        }
    }
    let tree = crate::npm::build_hoist_tree(&canonical, &all_roots);

    // Non-root workspaces are read fresh from disk because the caller
    // doesn't thread them through — the root manifest is the only one
    // that might carry unsaved edits (from `aube add` / `remove`).
    // Silently falling back to an empty manifest when a read fails
    // keeps the writer best-effort: a missing workspace package.json
    // is odd but not fatal.
    let project_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut workspace_manifests: BTreeMap<String, aube_manifest::PackageJson> = BTreeMap::new();
    for importer_path in graph.importers.keys() {
        if importer_path == "." {
            continue;
        }
        let pj_path = project_dir.join(importer_path).join("package.json");
        let pj = aube_manifest::PackageJson::from_path(&pj_path).unwrap_or_default();
        workspace_manifests.insert(importer_path.clone(), pj);
    }

    // Build the `workspaces[path]` object for each importer.
    //
    // bun's root entry carries only `name` + dep sections (the root's
    // `version`/`bin`/`peerDependenciesMeta` live in the adjacent
    // `package.json`, so duplicating them into the lockfile would
    // produce a gratuitous diff against bun's own output). Non-root
    // entries carry the full picture — `version`, `bin`, dep sections,
    // and `optionalPeers` (bun's compact list form of
    // `peerDependenciesMeta[name].optional`) — because bun treats the
    // lockfile as authoritative for workspace resolution and doesn't
    // re-read every workspace package.json on install.
    //
    // Returns ordered `(key, value)` pairs rather than a `Map` so the
    // hand-written JSONC emitter can render them in bun's field order.
    fn build_workspace_pairs(
        pj: &aube_manifest::PackageJson,
        is_root: bool,
        ws_extras: Option<&BTreeMap<String, Value>>,
    ) -> Vec<(String, Value)> {
        let mut pairs: Vec<(String, Value)> = Vec::new();
        if let Some(name) = &pj.name {
            pairs.push(("name".to_string(), json!(name)));
        }
        if !is_root {
            if let Some(version) = &pj.version {
                pairs.push(("version".to_string(), json!(version)));
            }
            if let Some(bin) = pj.extra.get("bin") {
                pairs.push(("bin".to_string(), bin.clone()));
            }
        }
        if !pj.dependencies.is_empty() {
            pairs.push(("dependencies".to_string(), json!(pj.dependencies)));
        }
        if !pj.dev_dependencies.is_empty() {
            pairs.push(("devDependencies".to_string(), json!(pj.dev_dependencies)));
        }
        if !pj.optional_dependencies.is_empty() {
            pairs.push((
                "optionalDependencies".to_string(),
                json!(pj.optional_dependencies),
            ));
        }
        if !pj.peer_dependencies.is_empty() {
            pairs.push(("peerDependencies".to_string(), json!(pj.peer_dependencies)));
        }
        if !is_root
            && let Some(meta) = pj
                .extra
                .get("peerDependenciesMeta")
                .and_then(Value::as_object)
        {
            // `serde_json::Map` is workspace-configured with
            // `preserve_order`, so `iter()` yields insertion order.
            // bun emits `optionalPeers` alphabetized — sort here to
            // match, otherwise a package.json that declares
            // `peerDependenciesMeta` keys out of order would round-
            // trip to a different byte sequence than bun produces.
            let mut optional_peer_names: Vec<&String> = meta
                .iter()
                .filter(|(_, v)| v.get("optional").and_then(Value::as_bool).unwrap_or(false))
                .map(|(k, _)| k)
                .collect();
            optional_peer_names.sort();
            if !optional_peer_names.is_empty() {
                let optional_peers: Vec<Value> = optional_peer_names
                    .into_iter()
                    .map(|k| Value::String(k.clone()))
                    .collect();
                pairs.push(("optionalPeers".to_string(), Value::Array(optional_peers)));
            }
        }
        // Re-emit unknown workspace fields (anything bun writes that
        // we don't model above) so a bun-side roundtrip preserves
        // them verbatim. Skip keys we've already rendered to avoid
        // duplicating the serde-flatten collision with typed fields.
        if let Some(extras) = ws_extras {
            let already: BTreeSet<String> = pairs.iter().map(|(k, _)| k.clone()).collect();
            for (k, v) in extras {
                if already.contains(k) {
                    continue;
                }
                pairs.push((k.clone(), v.clone()));
            }
        }
        pairs
    }

    let mut workspace_pairs: Vec<(String, Vec<(String, Value)>)> = Vec::new();
    workspace_pairs.push((
        "".to_string(),
        build_workspace_pairs(manifest, true, graph.workspace_extra_fields.get(".")),
    ));
    for (importer_path, pj) in &workspace_manifests {
        let extras = graph.workspace_extra_fields.get(importer_path);
        workspace_pairs.push((
            importer_path.clone(),
            build_workspace_pairs(pj, false, extras),
        ));
    }

    let mut package_entries: Vec<(String, Value)> = Vec::new();
    for (segs, canonical_key) in &tree {
        let Some(pkg) = canonical.get(canonical_key).copied() else {
            continue;
        };

        // Bun's key form: `foo` (hoisted) or `parent/foo` (nested).
        // Scoped names like `@scope/name` already carry their own
        // internal `/` and are joined wholesale — bun's parser
        // recognizes `@`-prefixed segments as a single unit.
        let bun_key = segs.join("/");

        // Metadata object: transitive deps keyed by name → declared
        // range (e.g. `"^4.1.0"`). Fall back to the resolved pin when
        // the declared range is unknown — happens for lockfiles that
        // came through a format without declared ranges (pnpm's
        // `snapshots:` stores pins only). Filter out deps we don't
        // have a canonical entry for (e.g. dropped optional deps).
        //
        // Split the combined `dependencies` map back into
        // `dependencies` + `optionalDependencies` on emission so
        // packages that originally declared optionals round-trip
        // through bun's parser with the same classification.
        let mut deps_obj = serde_json::Map::new();
        let mut opt_deps_obj = serde_json::Map::new();
        for (dep_name, dep_value) in &pkg.dependencies {
            let key = crate::npm::child_canonical_key(dep_name, dep_value);
            if !canonical.contains_key(&key) {
                continue;
            }
            let rendered = pkg
                .declared_dependencies
                .get(dep_name)
                .cloned()
                .unwrap_or_else(|| {
                    crate::npm::dep_value_as_version(dep_name, dep_value).to_string()
                });
            if pkg.optional_dependencies.contains_key(dep_name) {
                opt_deps_obj.insert(dep_name.clone(), Value::String(rendered));
            } else {
                deps_obj.insert(dep_name.clone(), Value::String(rendered));
            }
        }
        let mut meta = serde_json::Map::new();
        if !deps_obj.is_empty() {
            meta.insert("dependencies".to_string(), Value::Object(deps_obj));
        }
        if !opt_deps_obj.is_empty() {
            meta.insert(
                "optionalDependencies".to_string(),
                Value::Object(opt_deps_obj),
            );
        }
        // Peer declarations survive on bun's per-entry meta.
        // Collapsing them into `dependencies` on re-emit is one of
        // the reported parity bugs, so round-trip through the typed
        // slot.
        if !pkg.peer_dependencies.is_empty() {
            let map: serde_json::Map<String, Value> = pkg
                .peer_dependencies
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            meta.insert("peerDependencies".to_string(), Value::Object(map));
        }
        // `optionalPeers` is bun's compact list form — derive from
        // `peer_dependencies_meta` when present, fall back to any
        // original extra_meta["optionalPeers"] array.
        let optional_peer_names: Vec<String> = pkg
            .peer_dependencies_meta
            .iter()
            .filter(|(_, v)| v.optional)
            .map(|(k, _)| k.clone())
            .collect();
        if !optional_peer_names.is_empty() {
            let mut sorted = optional_peer_names.clone();
            sorted.sort();
            let arr: Vec<Value> = sorted.into_iter().map(Value::String).collect();
            meta.insert("optionalPeers".to_string(), Value::Array(arr));
        }
        // Preserve the full `bin:` map — bun's meta block records
        // executables by name so `bun install --frozen-lockfile` can
        // recreate the `.bin` shims without re-reading each tarball's
        // manifest. pnpm collapses this to `hasBin: true`; we keep
        // both representations on `LockedPackage.bin` so either
        // writer can render byte-identical output.
        //
        // Prefer the original shape captured in `extra_meta["bin"]`
        // (string vs object) so a bun-authored lockfile that wrote
        // `"bin": "./foo"` doesn't round-trip to `"bin": {"foo": "./foo"}`.
        // Skip empty-key entries — those are the placeholder bins
        // pnpm's lockfile synthesizes when it knows `hasBin: true`
        // but has no paths.
        if let Some(raw_bin) = pkg.extra_meta.get("bin")
            && !matches!(raw_bin, Value::Null)
        {
            meta.insert("bin".to_string(), raw_bin.clone());
        } else {
            let real_bins: serde_json::Map<String, Value> = pkg
                .bin
                .iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            if !real_bins.is_empty() {
                meta.insert("bin".to_string(), Value::Object(real_bins));
            }
        }
        // Preserve optional-platform packages' filter metadata so
        // bun's platform-aware resolution still has what it needs
        // on the next install.
        if !pkg.os.is_empty() {
            let arr: Vec<Value> = pkg.os.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("os".to_string(), Value::Array(arr));
        }
        if !pkg.cpu.is_empty() {
            let arr: Vec<Value> = pkg.cpu.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("cpu".to_string(), Value::Array(arr));
        }
        if !pkg.libc.is_empty() {
            let arr: Vec<Value> = pkg.libc.iter().map(|s| Value::String(s.clone())).collect();
            meta.insert("libc".to_string(), Value::Array(arr));
        }
        // Extras: anything bun wrote on the meta block that we don't
        // model on `LockedPackage` (e.g. `deprecated`,
        // `hasInstallScript`). Skip keys we've already rendered to
        // avoid duplicate slots — the serde-flatten capture would
        // include them only if the typed slot was missing.
        const MODELED_META_KEYS: &[&str] = &[
            "dependencies",
            "optionalDependencies",
            "peerDependencies",
            "optionalPeers",
            "bin",
            "os",
            "cpu",
            "libc",
        ];
        for (k, v) in &pkg.extra_meta {
            if MODELED_META_KEYS.contains(&k.as_str()) {
                continue;
            }
            meta.insert(k.clone(), v.clone());
        }

        // npm-alias identity: bun writes the *registry* name and
        // resolved version as the ident when the hoist key is an
        // alias (`foo-alias: [bar@1.2.3, ...]`), not the alias name.
        // Aube's earlier writer emitted `{name}@{version}` which
        // collapsed to the alias name and produced a gratuitous diff
        // against bun's own output.
        let ident_name = pkg.alias_of.as_deref().unwrap_or(&pkg.name);
        let ident = format!("{}@{}", ident_name, pkg.version);
        let integrity = pkg.integrity.clone().unwrap_or_default();
        let entry = Value::Array(vec![
            Value::String(ident),
            Value::String(String::new()),
            Value::Object(meta),
            Value::String(integrity),
        ]);
        package_entries.push((bun_key, entry));
    }

    // Echo back the parsed `configVersion` (default 1 for older v1.1
    // lockfiles that predate the field) so a bun-bumped value round-
    // trips instead of silently downgrading on re-emit.
    let config_version = graph.bun_config_version.unwrap_or(1);

    // Collect top-level blocks bun understands natively. Overrides /
    // catalog / catalogs / patchedDependencies / trustedDependencies
    // are all round-tripped from the parsed graph; anything else the
    // lockfile carried drops through `graph.extra_fields`.
    let mut top_level_extras: Vec<(String, Value)> = Vec::new();
    if !graph.overrides.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in &graph.overrides {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        top_level_extras.push(("overrides".to_string(), Value::Object(obj)));
    }
    if !graph.patched_dependencies.is_empty() {
        let mut obj = serde_json::Map::new();
        for (k, v) in &graph.patched_dependencies {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        top_level_extras.push(("patchedDependencies".to_string(), Value::Object(obj)));
    }
    if !graph.trusted_dependencies.is_empty() {
        let arr: Vec<Value> = graph
            .trusted_dependencies
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect();
        top_level_extras.push(("trustedDependencies".to_string(), Value::Array(arr)));
    }
    if let Some(default_catalog) = graph.catalogs.get("default") {
        let mut obj = serde_json::Map::new();
        for (k, v) in default_catalog {
            obj.insert(k.clone(), Value::String(v.specifier.clone()));
        }
        if !obj.is_empty() {
            top_level_extras.push(("catalog".to_string(), Value::Object(obj)));
        }
    }
    let named_catalogs: BTreeMap<&String, &BTreeMap<String, crate::CatalogEntry>> = graph
        .catalogs
        .iter()
        .filter(|(k, _)| k.as_str() != "default")
        .collect();
    if !named_catalogs.is_empty() {
        let mut outer = serde_json::Map::new();
        for (name, entries) in named_catalogs {
            let mut inner = serde_json::Map::new();
            for (k, v) in entries {
                inner.insert(k.clone(), Value::String(v.specifier.clone()));
            }
            outer.insert(name.clone(), Value::Object(inner));
        }
        top_level_extras.push(("catalogs".to_string(), Value::Object(outer)));
    }
    // Finally, anything else the parser stashed in `extra_fields`
    // (future bun bumps or hand-authored blocks we don't model).
    const MODELED_TOP_KEYS: &[&str] = &[
        "lockfileVersion",
        "configVersion",
        "workspaces",
        "packages",
        "overrides",
        "patchedDependencies",
        "trustedDependencies",
        "catalog",
        "catalogs",
    ];
    for (k, v) in &graph.extra_fields {
        if MODELED_TOP_KEYS.contains(&k.as_str()) {
            continue;
        }
        top_level_extras.push((k.clone(), v.clone()));
    }

    let body = format_bun_lockfile(
        &workspace_pairs,
        &package_entries,
        config_version,
        &top_level_extras,
    );
    crate::atomic_write_lockfile(path, body.as_bytes())?;
    Ok(())
}

/// Hand-written JSONC emitter matching bun 1.2's `bun.lock` style.
///
/// bun's output has an idiosyncratic shape — nested object fields use
/// trailing commas (standard JSONC) except `packages:` itself (the
/// last top-level field, where bun omits the trailing comma and leaves
/// the closing brace bare) — and every `packages:` entry is serialized
/// as a single-line array with a blank separator above. serde_json's
/// `to_string_pretty` can't express any of that, so we build the
/// output by hand.
///
/// `workspaces` is the ordered list of `(path, pairs)` where `path` is
/// the workspace key in `workspaces[]` (`""` for the root,
/// `"packages/app"` for non-root) and `pairs` are the ordered
/// key/value entries inside. `package_entries` are the `packages:`
/// map in BTreeMap order — each is rendered as a single-line
/// `[ident, "", {meta}, integrity]` array.
///
/// `config_version` is echoed back into the output as bun itself does —
/// hardcoding would silently downgrade the field when bun bumps it.
fn format_bun_lockfile(
    workspaces: &[(String, Vec<(String, serde_json::Value)>)],
    package_entries: &[(String, serde_json::Value)],
    config_version: u32,
    top_level_extras: &[(String, serde_json::Value)],
) -> String {
    let mut out = String::with_capacity(8192);
    out.push_str("{\n");
    out.push_str("  \"lockfileVersion\": 1,\n");
    out.push_str(&format!("  \"configVersion\": {config_version},\n"));

    // Workspaces block. Emits root (`""`) first, then each non-root
    // workspace in the order the caller supplied.
    out.push_str("  \"workspaces\": {\n");
    for (path, pairs) in workspaces.iter() {
        out.push_str(&format!(
            "    {}: {{\n",
            serde_json::to_string(path).unwrap()
        ));
        // Keys bun renders as multi-line blocks inside a workspace
        // entry. Other object-valued keys (`bin`) stay inline to
        // match bun's `"bin": { "name": "./path" }` form.
        const MULTILINE_KEYS: &[&str] = &[
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ];
        for (k, v) in pairs.iter() {
            let key_str = serde_json::to_string(k).unwrap();
            // bun emits a trailing comma after every workspace-level
            // field, including the last one — `},` closes the block.
            match v {
                serde_json::Value::Object(map)
                    if !map.is_empty() && MULTILINE_KEYS.contains(&k.as_str()) =>
                {
                    out.push_str(&format!("      {key_str}: {{\n"));
                    for (dk, dv) in map {
                        out.push_str(&format!(
                            "        {}: {},\n",
                            serde_json::to_string(dk).unwrap(),
                            inline_json(dv, 0)
                        ));
                    }
                    out.push_str("      },\n");
                }
                _ => {
                    out.push_str(&format!("      {key_str}: {},\n", inline_json(v, 0)));
                }
            }
        }
        // bun emits a trailing comma on every workspace entry,
        // including the last one — the outer `"workspaces"` map's
        // own trailing comma still closes the block below.
        out.push_str("    },\n");
    }
    out.push_str("  },\n");

    // Top-level extras (`overrides`, `catalog`, `catalogs`,
    // `patchedDependencies`, `trustedDependencies`, plus anything
    // the parser captured in `extra_fields`). Emit in the order the
    // caller supplied so a bun-first write preserves bun's own
    // field order on re-read.
    for (k, v) in top_level_extras {
        let key_str = serde_json::to_string(k).unwrap();
        match v {
            serde_json::Value::Object(map) if !map.is_empty() => {
                out.push_str(&format!("  {key_str}: {{\n"));
                for (dk, dv) in map {
                    out.push_str(&format!(
                        "    {}: {},\n",
                        serde_json::to_string(dk).unwrap(),
                        inline_json(dv, 0)
                    ));
                }
                out.push_str("  },\n");
            }
            _ => {
                out.push_str(&format!("  {key_str}: {},\n", inline_json(v, 0)));
            }
        }
    }

    // Packages block. Each entry is its own line; bun separates
    // entries with a blank line (an empty line between every
    // consecutive pair). `packages:` is bun's last top-level field and
    // gets no trailing comma on its closing brace.
    out.push_str("  \"packages\": {\n");
    for (i, (key, entry)) in package_entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "    {}: {},\n",
            serde_json::to_string(key).unwrap(),
            inline_json(entry, 0)
        ));
    }
    out.push_str("  }\n");
    out.push_str("}\n");
    out
}

/// Serialize a JSON value inline in bun's spaced style — objects as
/// `{ "k": v, "k2": v2 }` (with a trailing space before `}` and a
/// trailing comma before the close), arrays as `["a", "b"]` (no
/// trailing comma). Recurses into nested objects/arrays.
///
/// `base_indent` is reserved for a future multi-line fallback when an
/// object gets too wide; bun in 1.2 keeps even the larger metadata
/// objects on one line, so we currently ignore it.
fn inline_json(value: &serde_json::Value, _base_indent: usize) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(_) => serde_json::to_string(value).unwrap(),
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(|v| inline_json(v, 0)).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap(),
                        inline_json(v, 0)
                    )
                })
                .collect();
            // bun writes `{ k: v, k2: v2 }` — spaces inside, no trailing comma.
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_ident() {
        assert_eq!(
            split_ident("foo@1.2.3"),
            Some(("foo".to_string(), "1.2.3".to_string()))
        );
        assert_eq!(
            split_ident("@scope/pkg@1.0.0"),
            Some(("@scope/pkg".to_string(), "1.0.0".to_string()))
        );
    }

    #[test]
    fn test_is_integrity_hash() {
        // Real SRI hashes at their exact base64 lengths.
        assert!(is_integrity_hash(&format!("sha512-{}", "A".repeat(88))));
        assert!(is_integrity_hash(&format!("sha256-{}", "A".repeat(44))));
        assert!(is_integrity_hash(&format!("sha1-{}", "A".repeat(28))));
        // base64 body with +, /, and = padding is still valid.
        let mixed = format!("{}+/==", "A".repeat(84));
        assert_eq!(mixed.len(), 88);
        assert!(is_integrity_hash(&format!("sha512-{mixed}")));

        // Github dir-id whose owner is literally a hash algo name —
        // the extra `-` and the wrong length must disqualify it.
        assert!(!is_integrity_hash("sha1-myrepo-abc123"));
        assert!(!is_integrity_hash("sha256-owner-repo-deadbee"));
        // Unknown algo prefix.
        assert!(!is_integrity_hash("foo-bar"));
        // Correct algo prefix but the wrong body length.
        assert!(!is_integrity_hash("sha512-tooshort"));
        // Right length but contains a forbidden `-` (base64 has no `-`).
        let with_dash = format!("sha512-{}-{}", "A".repeat(43), "A".repeat(44));
        assert_eq!(with_dash.len(), "sha512-".len() + 88);
        assert!(!is_integrity_hash(&with_dash));
        // No dash at all.
        assert!(!is_integrity_hash("opaquestring"));
    }

    #[test]
    fn test_strip_jsonc_trailing_comma() {
        let input = r#"{ "a": 1, "b": 2, }"#;
        let out = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn test_strip_jsonc_line_comment() {
        let input = "{ // comment\n  \"a\": 1 }";
        let out = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn test_strip_jsonc_respects_strings() {
        // Make sure we don't strip things that look like comments inside strings
        let input = r#"{ "url": "http://example.com/path" }"#;
        let out = strip_jsonc(input);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "http://example.com/path");
    }

    /// `strip_jsonc` must preserve byte offsets so a `serde_json` error
    /// on the stripped buffer maps 1:1 onto the original file — that's
    /// the only reason `parse()` can hand `raw_content` to miette's
    /// `NamedSource` and trust the span.
    #[test]
    fn test_strip_jsonc_preserves_byte_offsets() {
        let cases = [
            "{ \"a\": 1 }",                    // no-op
            "{ // line\n  \"a\": 1 }",         // line comment
            "{ /* block */ \"a\": 1 }",        // block comment
            "{ /* multi\nline */ \"a\": 1 }",  // block spans newline
            "{ \"a\": 1, \"b\": 2, }",         // trailing comma
            "{ \"a\": \"// not a comment\" }", // comment inside string
            "{ \"a\": 1 /* trailing",          // unterminated block
        ];
        for input in cases {
            let out = strip_jsonc(input);
            assert_eq!(
                out.len(),
                input.len(),
                "length mismatch stripping {input:?} -> {out:?}"
            );
            // Every `\n` must land at the same byte offset so line
            // numbers stay stable between the raw and cleaned buffers.
            let raw_nls: Vec<usize> = input.match_indices('\n').map(|(i, _)| i).collect();
            let out_nls: Vec<usize> = out.match_indices('\n').map(|(i, _)| i).collect();
            assert_eq!(raw_nls, out_nls, "newline drift stripping {input:?}");
        }
    }

    /// Build a placeholder SRI hash of the right shape (88-char base64
    /// body for sha512). Tests need real SRI lengths now that
    /// `is_integrity_hash` validates them — bogus stand-ins like
    /// `sha512-aaa` would be rejected and integrity dropped.
    fn fake_sri(tag: char) -> String {
        format!("sha512-{}", tag.to_string().repeat(88))
    }

    #[test]
    fn test_parse_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri_foo = fake_sri('a');
        let sri_nested = fake_sri('b');
        let sri_bar = fake_sri('c');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "test",
      "dependencies": {
        "foo": "^1.0.0",
      },
      "devDependencies": {
        "bar": "^2.0.0",
      },
    },
  },
  "packages": {
    "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "SRI_FOO"],
    "nested": ["nested@3.1.0", "", {}, "SRI_NESTED"],
    "bar": ["bar@2.5.0", "", {}, "SRI_BAR"],
  }
}"#
        .replace("SRI_FOO", &sri_foo)
        .replace("SRI_NESTED", &sri_nested)
        .replace("SRI_BAR", &sri_bar);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        assert_eq!(graph.packages.len(), 3);
        assert!(graph.packages.contains_key("foo@1.2.3"));
        assert!(graph.packages.contains_key("nested@3.1.0"));
        assert!(graph.packages.contains_key("bar@2.5.0"));

        let foo = &graph.packages["foo@1.2.3"];
        assert_eq!(foo.integrity.as_deref(), Some(sri_foo.as_str()));
        assert_eq!(
            foo.dependencies.get("nested").map(String::as_str),
            Some("nested@3.1.0")
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
    fn test_parse_multi_version_nested() {
        // bun keys nested packages using "parent/child" paths.
        // Here `bar` exists hoisted at 2.0.0 and nested under `foo` at 1.0.0.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "sha512-top-bar"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "sha512-foo"],
    "foo/bar": ["bar@1.0.0", "", {}, "sha512-nested-bar"]
  }
}"#;
        std::fs::write(tmp.path(), content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        assert!(graph.packages.contains_key("bar@2.0.0"));
        assert!(graph.packages.contains_key("bar@1.0.0"));
        assert!(graph.packages.contains_key("foo@1.0.0"));

        // foo's transitive must be the nested bar@1.0.0
        let foo = &graph.packages["foo@1.0.0"];
        assert_eq!(
            foo.dependencies.get("bar").map(String::as_str),
            Some("bar@1.0.0")
        );

        // Root direct bar is the hoisted 2.0.0
        let root = graph.importers.get(".").unwrap();
        let bar = root.iter().find(|d| d.name == "bar").unwrap();
        assert_eq!(bar.dep_path, "bar@2.0.0");
    }

    #[test]
    fn test_parse_scoped() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "@scope/pkg": "^1.0.0" }
    }
  },
  "packages": {
    "@scope/pkg": ["@scope/pkg@1.0.0", "", {}, "sha512-zzz"]
  }
}"#;
        std::fs::write(tmp.path(), content).unwrap();
        let graph = parse(tmp.path()).unwrap();
        assert!(graph.packages.contains_key("@scope/pkg@1.0.0"));
        let root = graph.importers.get(".").unwrap();
        assert_eq!(root[0].name, "@scope/pkg");
    }

    /// bun.lock uses a 3-tuple `[ident, { meta }, "owner-repo-commit"]`
    /// for GitHub / git deps (no `resolved` slot and no integrity). A
    /// naive positional parse would mistake the trailing commit-id
    /// string for the metadata object — make sure we recognize the
    /// object by type rather than position.
    #[test]
    fn test_parse_github_dep() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri_dep = fake_sri('d');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "vfs": "github:collinstevens/vfs#0b6ea53" }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", { "dependencies": { "dep": "^1.0.0" } }, "collinstevens-vfs-0b6ea53"],
    "dep": ["dep@1.0.0", "", {}, "SRI_DEP"]
  }
}"#
        .replace("SRI_DEP", &sri_dep);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        // The vfs package parsed with its github: version and picked up
        // the transitive dep declared in the metadata slot.
        let vfs_key = "vfs@github:collinstevens/vfs#0b6ea53abcdef";
        assert!(graph.packages.contains_key(vfs_key));
        let vfs = &graph.packages[vfs_key];
        assert_eq!(
            vfs.dependencies.get("dep").map(String::as_str),
            Some("dep@1.0.0")
        );
        // No SRI-shaped hash on the github entry → integrity stays None.
        assert!(vfs.integrity.is_none());

        // The adjacent registry dep's integrity must still round-trip —
        // proves the type-based introspection doesn't break the normal
        // 4-tuple path when mixed with a 3-tuple github entry.
        let dep = &graph.packages["dep@1.0.0"];
        assert_eq!(dep.integrity.as_deref(), Some(sri_dep.as_str()));

        let root = graph.importers.get(".").unwrap();
        assert!(root.iter().any(|d| d.name == "vfs"));
    }

    /// Round-trip the same multi-version shape the npm writer test
    /// uses: two versions of `bar`, one hoisted, one nested under
    /// `foo`. The writer's bun-key form (`foo/bar` instead of
    /// `node_modules/foo/node_modules/bar`) must round-trip through
    /// the bun parser without losing the nested version.
    #[test]
    fn test_write_roundtrip_multi_version() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri_top = fake_sri('t');
        let sri_foo = fake_sri('f');
        let sri_nested = fake_sri('n');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "foo": "^1.0.0", "bar": "^2.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@2.0.0", "", {}, "SRI_TOP"],
    "foo": ["foo@1.0.0", "", { "dependencies": { "bar": "^1.0.0" } }, "SRI_FOO"],
    "foo/bar": ["bar@1.0.0", "", {}, "SRI_NESTED"]
  }
}"#
        .replace("SRI_TOP", &sri_top)
        .replace("SRI_FOO", &sri_foo)
        .replace("SRI_NESTED", &sri_nested);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        let manifest = aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: [
                ("foo".to_string(), "^1.0.0".to_string()),
                ("bar".to_string(), "^2.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let reparsed = parse(out.path()).unwrap();

        assert!(reparsed.packages.contains_key("bar@2.0.0"));
        assert!(reparsed.packages.contains_key("bar@1.0.0"));
        assert!(reparsed.packages.contains_key("foo@1.0.0"));
        assert_eq!(
            reparsed.packages["bar@2.0.0"].integrity.as_deref(),
            Some(sri_top.as_str())
        );
        assert_eq!(
            reparsed.packages["bar@1.0.0"].integrity.as_deref(),
            Some(sri_nested.as_str())
        );
        // foo's nested bar dep still resolves to 1.0.0 (nested)
        // rather than snapping to the hoisted 2.0.0.
        assert_eq!(
            reparsed.packages["foo@1.0.0"]
                .dependencies
                .get("bar")
                .map(String::as_str),
            Some("bar@1.0.0")
        );
    }

    /// Byte-parity with a real `bun install`-generated lockfile — the
    /// fixture at `tests/fixtures/bun-native.lock` was produced by
    /// bun 1.3 against a `{ chalk, picocolors, semver }` manifest. A
    /// parse → write round-trip must reproduce the exact bytes;
    /// anything less means `aube install --no-frozen-lockfile` churns
    /// someone's bun.lock in git when nothing in the graph moved.
    /// Covers the format fixes (`configVersion`, no workspace
    /// `version`, trailing commas, single-line package arrays) plus
    /// the data-model fixes that ride with them (declared-range
    /// preservation in `declared_dependencies`, `bin:` map
    /// round-trip).
    #[test]
    fn test_write_byte_identical_to_native_bun() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bun-native.lock");
        // Normalize line endings — Windows' `core.autocrlf=true` can
        // rewrite the checked-out fixture to CRLF even with
        // `.gitattributes eol=lf`; compare against LF form explicitly.
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
                "bun writer drifted from native bun output.\n\n--- expected ---\n{original}\n--- got ---\n{written}"
            );
        }
    }

    /// `configVersion` must echo back whatever was parsed, not a
    /// hardcoded `1`. Regression guard for a future bun release that
    /// bumps the field — without this, aube would silently downgrade
    /// every re-emit and drift against bun's own output.
    #[test]
    fn test_write_roundtrips_config_version() {
        let project = tempfile::TempDir::new().unwrap();
        let pj = project.path().join("package.json");
        std::fs::write(&pj, r#"{"name":"root","dependencies":{}}"#).unwrap();
        let lock_path = project.path().join("bun.lock");
        std::fs::write(
            &lock_path,
            r#"{
  "lockfileVersion": 1,
  "configVersion": 42,
  "workspaces": {
    "": { "name": "root" }
  },
  "packages": {}
}"#,
        )
        .unwrap();

        let graph = parse(&lock_path).unwrap();
        assert_eq!(graph.bun_config_version, Some(42));

        let manifest = aube_manifest::PackageJson::from_path(&pj).unwrap();
        write(&lock_path, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&lock_path).unwrap();
        assert!(
            written.contains("\"configVersion\": 42,"),
            "configVersion must round-trip verbatim, got:\n{written}"
        );
    }

    /// Hand-authored bun.lock with two workspace entries (root and
    /// `packages/app`) round-trips through the parser with both
    /// importers populated, and the writer regenerates both
    /// workspace entries from the on-disk manifests.
    #[test]
    fn test_parse_and_write_multi_workspace() {
        use tempfile::TempDir;
        let sri_foo = fake_sri('a');
        let sri_bar = fake_sri('b');

        let project = TempDir::new().unwrap();
        let project_dir = project.path();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"root","version":"1.0.0","dependencies":{"foo":"^1.0.0"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
        std::fs::write(
            project_dir.join("packages/app/package.json"),
            r#"{"name":"app","version":"2.0.0","dependencies":{"bar":"^3.0.0"}}"#,
        )
        .unwrap();

        let lock_path = project_dir.join("bun.lock");
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "name": "root",
      "version": "1.0.0",
      "dependencies": { "foo": "^1.0.0" }
    },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "dependencies": { "bar": "^3.0.0" }
    }
  },
  "packages": {
    "foo": ["foo@1.2.3", "", {}, "SRI_FOO"],
    "bar": ["bar@3.1.0", "", {}, "SRI_BAR"]
  }
}"#
        .replace("SRI_FOO", &sri_foo)
        .replace("SRI_BAR", &sri_bar);
        std::fs::write(&lock_path, content).unwrap();

        let graph = parse(&lock_path).unwrap();

        // Both importers are populated with their own direct deps.
        let root = graph.importers.get(".").expect("root importer");
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "foo");
        assert_eq!(root[0].dep_path, "foo@1.2.3");

        let app = graph
            .importers
            .get("packages/app")
            .expect("packages/app importer");
        assert_eq!(app.len(), 1);
        assert_eq!(app[0].name, "bar");
        assert_eq!(app[0].dep_path, "bar@3.1.0");

        // Now write the graph back out and re-parse. The non-root
        // workspace entry must survive the round-trip. Write into the
        // same project dir so the writer can find
        // `packages/app/package.json` alongside the lockfile.
        let manifest =
            aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
        std::fs::remove_file(&lock_path).unwrap();
        write(&lock_path, &graph, &manifest).unwrap();

        let reparsed = parse(&lock_path).unwrap();
        assert!(reparsed.importers.contains_key("."));
        assert!(reparsed.importers.contains_key("packages/app"));
        let app = &reparsed.importers["packages/app"];
        assert_eq!(app.len(), 1);
        assert_eq!(app[0].name, "bar");
        assert_eq!(app[0].dep_path, "bar@3.1.0");
        // And the raw text keeps the workspace block by key.
        let raw = std::fs::read_to_string(&lock_path).unwrap();
        assert!(raw.contains("\"packages/app\""));
        assert!(raw.contains("\"name\": \"app\""));
    }

    /// Non-root workspace entries must carry `version`, `bin`, and
    /// `optionalPeers` (bun's compact form of
    /// `peerDependenciesMeta[name].optional`). Root stays minimal —
    /// bun's own output omits those three on the root entry because
    /// the adjacent project `package.json` is authoritative.
    #[test]
    fn test_write_workspace_entry_carries_version_bin_and_optional_peers() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let project_dir = project.path();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"root","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/drifti")).unwrap();
        std::fs::write(
            project_dir.join("packages/drifti/package.json"),
            r#"{
  "name": "@redact/drifti",
  "version": "0.0.1",
  "bin": { "drifti": "./dist/cli/bin.mjs" },
  "peerDependencies": {
    "@electric-sql/pglite": "*",
    "kysely": "*"
  },
  "peerDependenciesMeta": {
    "kysely": { "optional": true },
    "@electric-sql/pglite": { "optional": true },
    "not-optional": { "optional": false }
  }
}"#,
        )
        .unwrap();

        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![]);
        importers.insert("packages/drifti".to_string(), vec![]);
        let graph = LockfileGraph {
            importers,
            ..Default::default()
        };

        let manifest =
            aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
        let lock_path = project_dir.join("bun.lock");
        write(&lock_path, &graph, &manifest).unwrap();

        let raw = std::fs::read_to_string(&lock_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
        let drifti = &v["workspaces"]["packages/drifti"];
        assert_eq!(drifti["name"], "@redact/drifti");
        assert_eq!(drifti["version"], "0.0.1");
        assert_eq!(drifti["bin"]["drifti"], "./dist/cli/bin.mjs");
        // Sorted alphabetically even though package.json lists keys
        // out of order, and the `optional: false` entry is excluded.
        let optional_peers: Vec<&str> = drifti["optionalPeers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(optional_peers, vec!["@electric-sql/pglite", "kysely"]);

        // `bin` must render inline — bun's own output puts it on one
        // line (`"bin": { "drifti": "./dist/cli/bin.mjs" }`). A
        // multi-line render here would produce the exact diff the
        // writer is trying to avoid.
        assert!(
            raw.contains(r#""bin": { "drifti": "./dist/cli/bin.mjs" },"#),
            "bin rendered multi-line or unexpected shape:\n{raw}"
        );

        // Root entry stays minimal: no version/bin/optionalPeers.
        let root = &v["workspaces"][""];
        assert!(
            root.get("version").is_none(),
            "root carried version: {root}"
        );
        assert!(root.get("bin").is_none(), "root carried bin: {root}");
        assert!(
            root.get("optionalPeers").is_none(),
            "root carried optionalPeers: {root}"
        );
    }

    /// A workspace-link package (`my-app` in this graph) must not leak
    /// into the bun.lock `packages` section — bun tracks workspaces
    /// through the `workspaces` map, and a leftover `packages["my-app"]`
    /// entry would confuse bun's own parser on round-trip.
    #[test]
    fn test_write_skips_workspace_link_packages() {
        use crate::LocalSource;
        use std::path::PathBuf;

        let tmp_dir = tempfile::TempDir::new().unwrap();
        let project_dir = tmp_dir.path();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"root","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
        std::fs::write(
            project_dir.join("packages/app/package.json"),
            r#"{"name":"my-app","version":"0.1.0"}"#,
        )
        .unwrap();

        let mut packages = BTreeMap::new();
        packages.insert(
            "my-app@0.1.0".to_string(),
            LockedPackage {
                name: "my-app".to_string(),
                version: "0.1.0".to_string(),
                dep_path: "my-app@0.1.0".to_string(),
                local_source: Some(LocalSource::Link(PathBuf::from("packages/app"))),
                ..Default::default()
            },
        );
        let mut importers = BTreeMap::new();
        importers.insert(".".to_string(), vec![]);
        importers.insert("packages/app".to_string(), vec![]);
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let manifest =
            aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
        let lock_path = project_dir.join("bun.lock");
        write(&lock_path, &graph, &manifest).unwrap();

        let raw = std::fs::read_to_string(&lock_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&strip_jsonc(&raw)).unwrap();
        let pkgs = v["packages"].as_object().unwrap();
        assert!(
            !pkgs.contains_key("my-app"),
            "workspace-link package leaked into `packages`: {pkgs:?}"
        );
        let ws = v["workspaces"].as_object().unwrap();
        assert!(ws.contains_key("packages/app"));
    }

    /// When the root and a non-root workspace declare the same dep
    /// name at *different* versions, the writer must emit a
    /// consistent top-level `packages` entry and still walk the
    /// chosen version's transitive deps. Regression test for a
    /// corruption in `build_hoist_tree`'s root-seeding loop: without
    /// name-dedupe, the second version would overwrite the first in
    /// `placed` but never get queued, so neither version's
    /// transitive deps were walked correctly and the top-level entry
    /// pointed at a package whose deps were never expanded.
    #[test]
    fn test_write_dedupes_duplicate_direct_deps_across_workspaces() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let project_dir = project.path();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"root","dependencies":{"foo":"^1.0.0"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
        std::fs::write(
            project_dir.join("packages/app/package.json"),
            r#"{"name":"app","dependencies":{"foo":"^2.0.0"}}"#,
        )
        .unwrap();

        let mut packages = BTreeMap::new();
        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dependencies: [("bar".to_string(), "bar@2.0.0".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        packages.insert(
            "foo@2.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "2.0.0".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                ..Default::default()
            },
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            LockedPackage {
                name: "bar".to_string(),
                version: "2.0.0".to_string(),
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
                specifier: None,
            }],
        );
        importers.insert(
            "packages/app".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );
        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let manifest =
            aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
        let lock_path = project_dir.join("bun.lock");
        write(&lock_path, &graph, &manifest).unwrap();

        let reparsed = parse(&lock_path).unwrap();
        // The root's version wins the hoisted `foo` slot (BTreeMap
        // iteration puts `.` before `packages/app`), and `bar` — only
        // reachable by walking root-foo's transitive deps — must be
        // present. Before the fix, `foo@2.0.0` would overwrite
        // `foo@1.0.0` in `placed` but never get queued, and neither
        // version's transitive deps (including `bar`) would make it
        // into the output.
        let foo = reparsed.packages.get("foo@1.0.0").expect("foo@1.0.0");
        assert_eq!(foo.version, "1.0.0");
        assert!(
            reparsed.packages.contains_key("bar@2.0.0"),
            "root foo's transitive `bar` was dropped: {:?}",
            reparsed.packages.keys().collect::<Vec<_>>()
        );
    }

    /// When a workspace directory path (e.g. `packages/app`) happens
    /// to share its first segment with a literal npm package name,
    /// the parser must not wrongly resolve a workspace dep to that
    /// package's nested entry. Here there's an npm package literally
    /// named `packages` with a nested `bar@9.9.9`, and the workspace
    /// `packages/app` depends on `bar`. The workspace's `bar` must
    /// resolve to the hoisted `bar@1.0.0`, not to `packages/bar`'s
    /// `9.9.9`.
    #[test]
    fn test_parse_workspace_path_does_not_alias_npm_package() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri = fake_sri('a');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "packages": "^1.0.0" } },
    "packages/app": {
      "name": "app",
      "dependencies": { "bar": "^1.0.0" }
    }
  },
  "packages": {
    "bar": ["bar@1.0.0", "", {}, "SRI"],
    "packages": ["packages@1.0.0", "", { "dependencies": { "bar": "^9.0.0" } }, "SRI"],
    "packages/bar": ["bar@9.9.9", "", {}, "SRI"]
  }
}"#
        .replace("SRI", &sri);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        let app = graph
            .importers
            .get("packages/app")
            .expect("packages/app importer");
        let bar = app.iter().find(|d| d.name == "bar").expect("bar dep");
        assert_eq!(
            bar.dep_path, "bar@1.0.0",
            "workspace `bar` must resolve to hoisted 1.0.0, not packages/bar@9.9.9"
        );
    }

    /// Top-level `overrides` / `patchedDependencies` / `trustedDependencies`
    /// and the unnamed `catalog` / named `catalogs` blocks must round-trip
    /// verbatim — bun preserves all five on re-emit, so aube dropping any
    /// of them is a real-repo churn source on every install. Keep this
    /// test format-agnostic (no SRI hashes, no packages) so it only
    /// exercises the metadata-preservation path.
    #[test]
    fn test_roundtrip_top_level_metadata() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" }
  },
  "overrides": {
    "lodash": "^4.17.21",
    "lodash>debug": "^4.0.0"
  },
  "patchedDependencies": {
    "lodash@4.17.21": "patches/lodash@4.17.21.patch"
  },
  "trustedDependencies": ["sharp", "esbuild"],
  "catalog": {
    "react": "^18.2.0"
  },
  "catalogs": {
    "evens": { "date-fns": "^2.30.0" }
  },
  "packages": {}
}"#;
        std::fs::write(tmp.path(), content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        assert_eq!(
            graph.overrides.get("lodash").map(String::as_str),
            Some("^4.17.21")
        );
        assert_eq!(
            graph.overrides.get("lodash>debug").map(String::as_str),
            Some("^4.0.0")
        );
        assert_eq!(
            graph
                .patched_dependencies
                .get("lodash@4.17.21")
                .map(String::as_str),
            Some("patches/lodash@4.17.21.patch")
        );
        assert_eq!(
            graph.trusted_dependencies,
            vec!["sharp".to_string(), "esbuild".to_string()],
            "trustedDependencies must preserve bun's original order on parse"
        );
        assert_eq!(graph.catalogs["default"]["react"].specifier, "^18.2.0");
        assert_eq!(graph.catalogs["evens"]["date-fns"].specifier, "^2.30.0");

        let manifest = aube_manifest::PackageJson {
            name: Some("root".to_string()),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(out.path()).unwrap();

        // Every round-tripped block must appear in the re-emitted
        // lockfile — the exact rendering is implementation-defined
        // but a substring check is enough to catch regression.
        assert!(
            written.contains("\"overrides\""),
            "overrides dropped:\n{written}"
        );
        assert!(
            written.contains("\"patchedDependencies\""),
            "patchedDependencies dropped:\n{written}"
        );
        assert!(
            written.contains("\"trustedDependencies\""),
            "trustedDependencies dropped:\n{written}"
        );
        // trustedDependencies must round-trip in insertion order
        // (bun writes [sharp, esbuild] — alphabetized emit would
        // produce a gratuitous diff against bun's own output).
        let sharp_at = written
            .find("\"sharp\"")
            .expect("sharp in trustedDependencies");
        let esbuild_at = written
            .find("\"esbuild\"")
            .expect("esbuild in trustedDependencies");
        assert!(
            sharp_at < esbuild_at,
            "trustedDependencies reordered on write — expected sharp before esbuild:\n{written}"
        );
        assert!(
            written.contains("\"catalog\""),
            "catalog dropped:\n{written}"
        );
        assert!(
            written.contains("\"catalogs\""),
            "catalogs dropped:\n{written}"
        );

        let reparsed = parse(out.path()).unwrap();
        assert_eq!(reparsed.overrides, graph.overrides);
        assert_eq!(reparsed.patched_dependencies, graph.patched_dependencies);
        assert_eq!(reparsed.trusted_dependencies, graph.trusted_dependencies);
        assert_eq!(reparsed.catalogs["default"]["react"].specifier, "^18.2.0");
    }

    /// Non-registry specifier classes (github:, file:, link:, https:,
    /// workspace:) must parse into `LocalSource` rather than fall
    /// through as registry pins. The installer routes by
    /// `LocalSource`, so mis-classification here sends the package
    /// through the default registry and either 404s or downloads the
    /// wrong tarball — bug class #1 in the parity report.
    #[test]
    fn test_parse_routes_non_registry_specs_to_localsource() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": {
        "vfs": "github:collinstevens/vfs#0b6ea53",
        "localdir": "file:./vendor/localdir",
        "localtgz": "file:./vendor/thing.tgz",
        "sibling": "link:../sibling",
        "remote": "https://example.com/thing.tgz"
      }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", {}, "collinstevens-vfs-0b6ea53abcdef"],
    "localdir": ["localdir@file:./vendor/localdir", {}],
    "localtgz": ["localtgz@file:./vendor/thing.tgz", {}],
    "sibling": ["sibling@link:../sibling", {}],
    "remote": ["remote@https://example.com/thing.tgz", {}]
  }
}"#;
        std::fs::write(tmp.path(), content).unwrap();
        let graph = parse(tmp.path()).unwrap();

        let vfs = graph
            .packages
            .values()
            .find(|p| p.name == "vfs")
            .expect("vfs package");
        assert!(
            matches!(vfs.local_source, Some(LocalSource::Git(_))),
            "github dep must be LocalSource::Git, got {:?}",
            vfs.local_source
        );

        let localdir = graph
            .packages
            .values()
            .find(|p| p.name == "localdir")
            .expect("localdir package");
        assert!(
            matches!(localdir.local_source, Some(LocalSource::Directory(_))),
            "file:./dir must be LocalSource::Directory, got {:?}",
            localdir.local_source
        );

        let localtgz = graph
            .packages
            .values()
            .find(|p| p.name == "localtgz")
            .expect("localtgz package");
        assert!(
            matches!(localtgz.local_source, Some(LocalSource::Tarball(_))),
            "file:./*.tgz must be LocalSource::Tarball, got {:?}",
            localtgz.local_source
        );

        let sibling = graph
            .packages
            .values()
            .find(|p| p.name == "sibling")
            .expect("sibling package");
        assert!(
            matches!(sibling.local_source, Some(LocalSource::Link(_))),
            "link: must be LocalSource::Link, got {:?}",
            sibling.local_source
        );

        let remote = graph
            .packages
            .values()
            .find(|p| p.name == "remote")
            .expect("remote package");
        assert!(
            matches!(remote.local_source, Some(LocalSource::RemoteTarball(_))),
            "https://*.tgz must be LocalSource::RemoteTarball, got {:?}",
            remote.local_source
        );
    }

    /// npm-alias ident: bun writes `<real>@<version>` as the ident
    /// string while using the alias name as the `packages[]` hoist
    /// key. Aube's earlier writer emitted `<alias>@<version>` and
    /// produced a gratuitous diff against bun's own output. Cover
    /// both parse (populates `alias_of`) and write (emits real name
    /// in ident).
    #[test]
    fn test_parse_and_write_npm_alias() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri = fake_sri('a');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "dependencies": { "h3-v2": "npm:h3@2.0.1" } }
  },
  "packages": {
    "h3-v2": ["h3@2.0.1", "", {}, "SRI"]
  }
}"#
        .replace("SRI", &sri);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();
        let h3 = graph
            .packages
            .values()
            .find(|p| p.name == "h3-v2")
            .expect("h3-v2 package");
        assert_eq!(h3.alias_of.as_deref(), Some("h3"));
        assert_eq!(h3.version, "2.0.1");

        let manifest = aube_manifest::PackageJson {
            name: Some("root".to_string()),
            dependencies: [("h3-v2".to_string(), "npm:h3@2.0.1".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(out.path()).unwrap();

        // Ident reads `h3@2.0.1` (registry identity), not `h3-v2@...`.
        assert!(
            written.contains("\"h3@2.0.1\""),
            "expected ident `h3@2.0.1`, got:\n{written}"
        );
        assert!(
            !written.contains("\"h3-v2@2.0.1\""),
            "alias-name ident leaked into packages entry:\n{written}"
        );
    }

    /// Per-entry meta blocks bun preserves that aube historically
    /// dropped: `peerDependencies`, `optionalPeers`, `os`, `cpu`,
    /// `libc`. Round-trip through a single package entry and confirm
    /// every field survives re-parse.
    #[test]
    fn test_roundtrip_peer_and_platform_metadata() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let sri = fake_sri('a');
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "dependencies": { "foo": "^1.0.0" } } },
  "packages": {
    "foo": ["foo@1.0.0", "", {
      "peerDependencies": { "react": "^18.0.0" },
      "optionalPeers": ["react"],
      "os": ["darwin", "linux"],
      "cpu": ["arm64", "x64"],
      "libc": ["glibc"]
    }, "SRI"]
  }
}"#
        .replace("SRI", &sri);
        std::fs::write(tmp.path(), &content).unwrap();
        let graph = parse(tmp.path()).unwrap();
        let foo = &graph.packages["foo@1.0.0"];
        assert_eq!(
            foo.peer_dependencies.get("react").map(String::as_str),
            Some("^18.0.0")
        );
        assert!(
            foo.peer_dependencies_meta
                .get("react")
                .is_some_and(|m| m.optional)
        );
        assert_eq!(
            foo.os.as_slice(),
            &["darwin".to_string(), "linux".to_string()]
        );
        assert_eq!(
            foo.cpu.as_slice(),
            &["arm64".to_string(), "x64".to_string()]
        );
        assert_eq!(foo.libc.as_slice(), &["glibc".to_string()]);

        let manifest = aube_manifest::PackageJson {
            name: Some("root".to_string()),
            dependencies: [("foo".to_string(), "^1.0.0".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        write(out.path(), &graph, &manifest).unwrap();
        let reparsed = parse(out.path()).unwrap();
        let foo2 = &reparsed.packages["foo@1.0.0"];
        assert_eq!(foo2.peer_dependencies, foo.peer_dependencies);
        assert_eq!(foo2.os, foo.os);
        assert_eq!(foo2.cpu, foo.cpu);
        assert_eq!(foo2.libc, foo.libc);
    }

    /// Workspace-level `peerDependencies` must survive round-trip
    /// through the serde-flatten `extra` map even though aube's
    /// typed workspace model doesn't claim the field directly. The
    /// prior revision had a typed slot that silently drained bun's
    /// peer block without plumbing it anywhere — regression guard.
    #[test]
    fn test_roundtrip_workspace_peer_dependencies() {
        use tempfile::TempDir;

        let project = TempDir::new().unwrap();
        let project_dir = project.path();
        std::fs::write(
            project_dir.join("package.json"),
            r#"{"name":"root","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project_dir.join("packages/app")).unwrap();
        // Non-root workspace's package.json deliberately omits
        // peerDependencies; the lockfile is the only place they live.
        std::fs::write(
            project_dir.join("packages/app/package.json"),
            r#"{"name":"app","version":"2.0.0"}"#,
        )
        .unwrap();

        let lock_path = project_dir.join("bun.lock");
        std::fs::write(
            &lock_path,
            r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "version": "2.0.0",
      "peerDependencies": { "react": "^18.0.0" }
    }
  },
  "packages": {}
}"#,
        )
        .unwrap();

        let graph = parse(&lock_path).unwrap();
        let app_extras = graph
            .workspace_extra_fields
            .get("packages/app")
            .expect("packages/app workspace_extra_fields entry");
        let peers = app_extras
            .get("peerDependencies")
            .and_then(serde_json::Value::as_object)
            .expect("peerDependencies captured in extras");
        assert_eq!(peers.get("react").and_then(|v| v.as_str()), Some("^18.0.0"));

        let manifest =
            aube_manifest::PackageJson::from_path(&project_dir.join("package.json")).unwrap();
        write(&lock_path, &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(&lock_path).unwrap();
        assert!(
            written.contains("\"peerDependencies\""),
            "workspace peerDependencies dropped on re-emit:\n{written}"
        );
        assert!(
            written.contains("\"react\""),
            "workspace peerDependencies.react dropped on re-emit:\n{written}"
        );
    }
}
