//! Parser for yarn.lock, covering both classic (v1) and berry (v2+).
//!
//! ## Classic (v1)
//!
//! Line-based, similar to YAML but not quite:
//!
//! ```text
//! # comment
//! "@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
//!   version "1.2.3"
//!   resolved "https://..."
//!   integrity sha512-...
//!   dependencies:
//!     other-pkg "^2.0.0"
//! ```
//!
//! Top-level blocks are keyed by one or more comma-separated specifiers
//! (`name@range`). The body is indented 2 spaces. Nested sections like
//! `dependencies:` add another 2 spaces of indentation.
//!
//! ## Berry (v2+)
//!
//! Proper YAML with a `__metadata:` header and per-block
//! `resolution:` / `checksum:` / `languageName` / `linkType` fields:
//!
//! ```yaml
//! __metadata:
//!   version: 8
//!   cacheKey: 10c0
//!
//! "@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
//!   version: 1.1.0
//!   resolution: "@scope/pkg@npm:1.1.0"
//!   dependencies:
//!     foo: "npm:^2.0.0"
//!   checksum: 10c0/aabbcc...
//!   languageName: node
//!   linkType: hard
//! ```
//!
//! Multi-spec headers are serialized as a single YAML string containing
//! `", "`-separated specifiers. Values carry a protocol prefix: `npm:`
//! for registry packages (the common case), `workspace:` for monorepo
//! refs, `file:` / `link:` / `portal:` for local paths, `patch:` for
//! patched packages, and full URLs for `git:` / `http(s):` sources.
//!
//! yarn.lock does not distinguish direct deps from transitive ones, so we
//! cross-reference specifiers against the project's package.json to populate
//! `importers["."]`.

use crate::{
    DepType, DirectDep, Error, GitSource, LocalSource, LockedPackage, LockfileGraph,
    RemoteTarballSource,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parse a yarn.lock file into a LockfileGraph, dispatching between
/// classic v1 and berry v2+ based on content.
///
/// The manifest is needed to identify direct dependencies (yarn.lock has
/// no notion of direct vs transitive).
pub fn parse(path: &Path, manifest: &aube_manifest::PackageJson) -> Result<LockfileGraph, Error> {
    let content = std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    if is_berry(&content) {
        parse_berry_str(path, &content, manifest)
    } else {
        parse_classic_str(path, &content, manifest)
    }
}

/// True when `content` looks like a yarn berry (v2+) lockfile.
///
/// Detection is content-based because both classic and berry live in the
/// same `yarn.lock` filename. Berry always emits a top-level
/// `__metadata:` mapping (it's what yarn's own cache-key bookkeeping
/// reads), so its presence is a reliable marker.
pub fn is_berry(content: &str) -> bool {
    content
        .lines()
        .any(|l| l.trim_start().starts_with("__metadata:"))
}

/// Like [`is_berry`], but reads from disk. Returns `false` on IO
/// errors (including "file doesn't exist") so callers that branch on
/// the result can fall through to the classic path or skip the file
/// entirely without an extra error branch.
///
/// Reads only a 4 KiB prefix rather than the full file. Berry's
/// `__metadata:` header always appears in the first couple of lines
/// (yarn emits the two-line comment banner then the mapping
/// directly), so scanning more than that wastes I/O — `parse_one`
/// calls `yarn::parse` immediately after, which reads the file
/// fully, so keeping the detect cheap avoids doubling the cost for
/// monorepo-scale lockfiles.
///
/// Byte-level scan: `__metadata:` is pure ASCII so matching raw
/// bytes is safe even if the 4 KiB window happens to cut a
/// multi-byte UTF-8 sequence mid-character (a non-concern for yarn's
/// own output, but cheap insurance against future format tweaks).
pub fn is_berry_path(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf).unwrap_or(0);
    let needle = b"__metadata:";
    // Must appear at the start of a line: either the file head or
    // directly after a newline. A preceding `#` comment line is fine
    // because the newline before `__metadata` is what matters.
    buf[..n]
        .windows(needle.len())
        .enumerate()
        .any(|(i, w)| w == needle && (i == 0 || buf[i - 1] == b'\n'))
}

/// Parse a yarn classic (v1) lockfile's pre-read contents.
fn parse_classic_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let blocks = tokenize_blocks(content).map_err(|e| Error::Parse(path.to_path_buf(), e))?;

    // spec_to_dep_path maps each specifier (e.g. "is-odd@^3.0.0") to its
    // resolved dep_path ("is-odd@3.0.1"). Used for resolving direct deps
    // from package.json ranges and transitive dep references.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for block in &blocks {
        let version = block
            .fields
            .get("version")
            .ok_or_else(|| {
                Error::Parse(
                    path.to_path_buf(),
                    format!("yarn.lock block {:?} has no version", block.specs),
                )
            })?
            .clone();

        // All specs in the key map to the same resolved package.
        // Extract the package name from the first spec.
        let name = parse_spec_name(&block.specs[0]).ok_or_else(|| {
            Error::Parse(
                path.to_path_buf(),
                format!(
                    "could not parse package name from yarn.lock spec '{}'",
                    block.specs[0]
                ),
            )
        })?;

        let dep_path = format!("{name}@{version}");

        for spec in &block.specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Only insert the first occurrence; dedup is fine because yarn.lock
        // already guarantees unique (name, version) entries.
        if !packages.contains_key(&dep_path) {
            // Yarn records the declared ranges on each block's
            // `dependencies:` subsection exactly as they appear in the
            // package's own manifest — preserve them so re-emit keeps
            // the original specifiers.
            let declared: BTreeMap<String, String> = block
                .dependencies
                .iter()
                .map(|(n, r)| (n.clone(), r.clone()))
                .collect();
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: name.clone(),
                    version: version.clone(),
                    integrity: block.fields.get("integrity").cloned(),
                    // Store raw "name@range" pairs for now; resolve below.
                    dependencies: block
                        .dependencies
                        .iter()
                        .map(|(n, r)| (n.clone(), format!("{n}@{r}")))
                        .collect(),
                    dep_path,
                    declared_dependencies: declared,
                    ..Default::default()
                },
            );
        }
    }

    // Second pass: resolve transitive dep references to dep_paths.
    let mut resolved: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let mut deps: BTreeMap<String, String> = BTreeMap::new();
        for (name, raw_spec) in &pkg.dependencies {
            if let Some(resolved_path) = spec_to_dep_path.get(raw_spec) {
                deps.insert(name.clone(), resolved_path.clone());
            }
        }
        resolved.insert(dep_path.clone(), deps);
    }
    for (dep_path, deps) in resolved {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }

    // Build direct deps from the manifest, cross-referencing against spec_to_dep_path.
    let mut direct: Vec<DirectDep> = Vec::new();
    let push_direct = |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        let spec = format!("{name}@{range}");
        if let Some(dep_path) = spec_to_dep_path.get(&spec) {
            direct.push(DirectDep {
                name: name.to_string(),
                dep_path: dep_path.clone(),
                dep_type,
                specifier: None,
            });
        }
    };
    for (name, range) in &manifest.dependencies {
        push_direct(name, range, DepType::Production, &mut direct);
    }
    for (name, range) in &manifest.dev_dependencies {
        push_direct(name, range, DepType::Dev, &mut direct);
    }
    for (name, range) in &manifest.optional_dependencies {
        push_direct(name, range, DepType::Optional, &mut direct);
    }

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), direct);

    Ok(LockfileGraph {
        importers,
        packages,
        ..Default::default()
    })
}

#[derive(Debug)]
struct Block {
    /// Specifier keys: each is a "name@range" string.
    specs: Vec<String>,
    /// Flat scalar fields (version, resolved, integrity, etc.)
    fields: BTreeMap<String, String>,
    /// Nested dependencies section: name -> range
    dependencies: BTreeMap<String, String>,
}

/// Tokenize the yarn.lock body into blocks. This is a line-based parser that
/// recognizes:
/// - Comments (`# …`) and blank lines
/// - Header lines ending in `:` (block keys)
/// - Fields indented with 2 spaces
/// - A special nested `dependencies:` section indented with 4 spaces
fn tokenize_blocks(content: &str) -> Result<Vec<Block>, String> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current: Option<Block> = None;
    let mut in_deps = false;

    for (lineno, raw_line) in content.lines().enumerate() {
        let line_num = lineno + 1;

        // Strip trailing whitespace but preserve leading indentation
        let line = raw_line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }

        let indent = line.len() - line.trim_start().len();

        // Top-level: block header (one or more comma-separated specs ending in `:`)
        if indent == 0 {
            if let Some(b) = current.take() {
                blocks.push(b);
            }
            in_deps = false;

            let header = line.trim_end_matches(':').trim();
            if !line.ends_with(':') {
                return Err(format!(
                    "line {line_num}: expected block header ending in ':', got '{line}'"
                ));
            }

            let specs = parse_header_specs(header).map_err(|e| format!("line {line_num}: {e}"))?;
            current = Some(Block {
                specs,
                fields: BTreeMap::new(),
                dependencies: BTreeMap::new(),
            });
            continue;
        }

        let block = current.as_mut().ok_or_else(|| {
            format!("line {line_num}: unexpected indented content before any block header")
        })?;

        if indent == 2 {
            in_deps = false;
            let body = line.trim_start();

            // Check for nested section markers (e.g. `dependencies:`)
            if body.ends_with(':') {
                let section = body.trim_end_matches(':').trim();
                if section == "dependencies"
                    || section == "optionalDependencies"
                    || section == "peerDependencies"
                {
                    // Only track `dependencies:` for our resolution graph; ignore others.
                    in_deps = section == "dependencies";
                    continue;
                }
                // Unknown 2-space section header — ignore.
                continue;
            }

            let (key, value) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse '{body}'"))?;
            block.fields.insert(key, value);
        } else if indent >= 4 && in_deps {
            let body = line.trim_start();
            let (name, range) = split_key_value(body)
                .ok_or_else(|| format!("line {line_num}: could not parse dep '{body}'"))?;
            block.dependencies.insert(name, range);
        }
        // Deeper indents outside `dependencies:` are ignored.
    }

    if let Some(b) = current.take() {
        blocks.push(b);
    }

    Ok(blocks)
}

/// Parse a header like `"foo@^1.0.0", "foo@^1.1.0"` or `foo@^1.0.0` into specs.
fn parse_header_specs(header: &str) -> Result<Vec<String>, String> {
    let mut specs = Vec::new();
    for raw in header.split(',') {
        let s = raw.trim();
        let unquoted = if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
            || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
        {
            &s[1..s.len() - 1]
        } else {
            s
        };
        if unquoted.is_empty() {
            return Err(format!("empty spec in header '{header}'"));
        }
        specs.push(unquoted.to_string());
    }
    if specs.is_empty() {
        return Err(format!("no specs parsed from header '{header}'"));
    }
    Ok(specs)
}

/// Split a body line like `version "1.2.3"` or `foo "^1.0.0"` into (key, value).
/// Values may be quoted or unquoted.
fn split_key_value(line: &str) -> Option<(String, String)> {
    let (key, rest) = line.split_once(char::is_whitespace)?;
    let value = rest.trim();
    let unquoted = if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
    {
        &value[1..value.len() - 1]
    } else {
        value
    };
    Some((key.to_string(), unquoted.to_string()))
}

/// Extract the package name from a spec like `foo@^1.0.0` or `@scope/pkg@^1.0.0`.
fn parse_spec_name(spec: &str) -> Option<String> {
    if let Some(rest) = spec.strip_prefix('@') {
        // Scoped package: find the '@' that comes after the '/'
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = spec.find('@')?;
        Some(spec[..at].to_string())
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn.lock v1
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn v1 lockfile.
///
/// yarn v1 is flat — unlike npm or bun, there's no nested install
/// path. Every `(name, version)` pair gets exactly one block whose
/// header is a comma-separated list of every spec that resolves to
/// it. We always emit the exact `"name@version"` spec (so transitive
/// deps emitted as `bar "2.5.0"` round-trip), and for direct root
/// deps we *also* emit the manifest range spec (e.g. `"bar@^2.0.0"`)
/// so `yarn install` and `aube install` — both of which look up
/// manifest ranges against the block headers — find the entry.
///
/// Transitive deps that arrive through a semver *range* (e.g. `foo`
/// depends on `bar "^2.0.0"`) are still technically lossy: the
/// original range isn't preserved, so if the parent's resolved
/// `bar` version differs from what the lockfile records, reparse
/// will miss. In practice the writer only runs on a graph the
/// resolver just produced, so the resolved versions match the
/// transitive dep keys exactly and reparse finds them.
///
/// Peer-contextualized variants collapse to a single `name@version`
/// entry (yarn v1's data model has no peer context). `resolved` URLs
/// are omitted for the same reason as the npm writer: we don't
/// persist the origin URL. yarn tolerates missing `resolved`.
pub fn write_classic(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants: one entry per canonical (name, version).
    let canonical = crate::build_canonical_map(graph);

    // Collect every spec that points at a canonical `(name, version)` —
    // both root-manifest ranges *and* transitive declared ranges from
    // every other package's `declared_dependencies`. Yarn groups all
    // specs resolving to the same (name, version) under one block
    // header (`is-number@^6.0.0, is-number@~6.0.1:`), and reparse of
    // a transitive `bar "^2.0.0"` needs `bar@^2.0.0` to appear in
    // some block's header to find the right canonical entry.
    //
    // Keyed by canonical key; values are the extra range-form spec
    // strings to emit alongside the exact `"name@version"` one.
    // Deduped per canonical so identical ranges coming from multiple
    // consumers collapse.
    let mut extra_specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let root_importer_specs = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter());
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        // Look up the range the manifest currently uses for this dep.
        let range = root_importer_specs
            .clone()
            .find(|(n, _)| n.as_str() == dep.name.as_str())
            .map(|(_, r)| r.clone());
        if let Some(range) = range {
            let spec = format!("{}@{range}", dep.name);
            if spec != canonical_key {
                extra_specs.entry(canonical_key).or_default().insert(spec);
            }
        }
    }
    // Harvest transitive declared ranges. Each package's
    // `declared_dependencies[name] = range` is the range its own
    // manifest uses; the canonical the range resolves to is whatever
    // the resolver already placed under `pkg.dependencies[name]`.
    for pkg in canonical.values() {
        for (dep_name, range) in &pkg.declared_dependencies {
            let Some(resolved_value) = pkg.dependencies.get(dep_name) else {
                continue;
            };
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            if !canonical.contains_key(&target) {
                continue;
            }
            let spec = format!("{dep_name}@{range}");
            if spec != target {
                extra_specs.entry(target).or_default().insert(spec);
            }
        }
    }

    let mut out = String::new();
    out.push_str("# THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.\n");
    out.push_str("# yarn lockfile v1\n\n\n");

    for (canonical_key, pkg) in &canonical {
        // Header: `"name@version"[, "name@range"]*:` — always start
        // with the exact spec so transitive reparse works, then
        // append any manifest range specs pointing at this entry.
        out.push('"');
        out.push_str(canonical_key);
        out.push('"');
        if let Some(extras) = extra_specs.get(canonical_key) {
            for spec in extras {
                out.push_str(", \"");
                out.push_str(spec);
                out.push('"');
            }
        }
        out.push_str(":\n");

        // `  version "..."`
        out.push_str("  version \"");
        out.push_str(&pkg.version);
        out.push_str("\"\n");

        if let Some(integ) = &pkg.integrity {
            out.push_str("  integrity ");
            out.push_str(integ);
            out.push('\n');
        }

        // `  dependencies:` block — prefer the declared range from the
        // package's own manifest (what yarn itself writes) over the
        // resolved pin. Falls back to the pin when the source
        // lockfile didn't carry declared ranges (e.g. pnpm → yarn).
        let nonempty_deps: BTreeMap<&str, String> = pkg
            .dependencies
            .iter()
            .filter_map(|(n, v)| {
                let key = crate::npm::child_canonical_key(n, v);
                if !canonical.contains_key(&key) {
                    return None;
                }
                let rendered = pkg
                    .declared_dependencies
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| crate::npm::dep_value_as_version(n, v).to_string());
                Some((n.as_str(), rendered))
            })
            .collect();
        if !nonempty_deps.is_empty() {
            out.push_str("  dependencies:\n");
            for (dep_name, dep_version) in &nonempty_deps {
                out.push_str("    ");
                out.push_str(dep_name);
                out.push_str(" \"");
                out.push_str(dep_version);
                out.push_str("\"\n");
            }
        }

        out.push('\n');
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Berry (v2+) parser
// ---------------------------------------------------------------------------

/// Parse a yarn berry (v2+) lockfile's pre-read contents.
///
/// Public entry point is [`parse`]; this function is split out so we
/// don't re-read the file just to peek for the `__metadata:` marker.
fn parse_berry_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let doc: serde_yaml::Value = serde_yaml::from_str(content)
        .map_err(|e| Error::parse_yaml_err(path, content.to_string(), &e))?;
    let map = doc.as_mapping().ok_or_else(|| {
        Error::Parse(
            path.to_path_buf(),
            "yarn berry lockfile root must be a mapping".to_string(),
        )
    })?;

    // Validate `__metadata.version` — berry has been at major
    // version 3 since yarn 2, 6 from yarn 3.x, and 8 from yarn 4.x.
    // We accept any value >= 3; the shape we care about (block
    // headers, `resolution:` / `checksum:`) hasn't changed across
    // those versions.
    let meta_version = map
        .get("__metadata")
        .and_then(|m| m.as_mapping())
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if meta_version < 3 {
        return Err(Error::Parse(
            path.to_path_buf(),
            format!(
                "yarn berry lockfile has unexpected __metadata.version: {meta_version} (expected >= 3)"
            ),
        ));
    }

    // First pass: walk every top-level block, turning each into a
    // `LockedPackage` keyed by canonical `name@version` and recording
    // every header-spec → dep_path mapping for the second pass.
    let mut spec_to_dep_path: BTreeMap<String, String> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    // Transitive dep values written into each `LockedPackage` in this
    // pass are the raw header specs (e.g. `"foo@npm:^2.0.0"`); the
    // second pass collapses them down to dep_paths.

    for (key, value) in map {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        if key_str.starts_with("__") {
            continue;
        }
        let block = value.as_mapping().ok_or_else(|| {
            Error::Parse(
                path.to_path_buf(),
                format!("yarn berry block '{key_str}' is not a mapping"),
            )
        })?;

        let specs = split_berry_header(key_str);
        if specs.is_empty() {
            continue;
        }

        // Berry writes versions unquoted (`version: 1.0.0`), so
        // YAML 1.2 core-schema resolution kicks in: three-component
        // semver parses as a plain string, but a bare integer
        // (`version: 5`) or two-component value (`version: 1.0`)
        // parses as number. Coerce both back to a string rather than
        // reporting "has no version" against a spec that obviously
        // does.
        let version = block
            .get("version")
            .and_then(yaml_scalar_as_string)
            .ok_or_else(|| {
                Error::Parse(
                    path.to_path_buf(),
                    format!("yarn berry block '{key_str}' has no version"),
                )
            })?;

        let resolution = block
            .get("resolution")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                Error::Parse(
                    path.to_path_buf(),
                    format!("yarn berry block '{key_str}' has no resolution"),
                )
            })?;
        let (res_name, res_protocol, res_body) = parse_berry_spec(resolution).ok_or_else(|| {
            Error::Parse(
                path.to_path_buf(),
                format!("yarn berry block '{key_str}' has malformed resolution '{resolution}'"),
            )
        })?;

        // Map the resolution protocol onto aube's data model. `npm:` is
        // the default and the only thing with a conventional
        // `name@version` dep_path; everything else is a non-registry
        // source the linker needs to materialize differently.
        let local_source = match res_protocol {
            "npm" => None,
            "workspace" => {
                // The root workspace block lives here (`name@workspace:.`)
                // and is driven by `package.json`, not the lockfile. We
                // rely on the manifest pass below to populate
                // `importers["."]`, and skip emitting workspace blocks
                // as `LockedPackage` entries.
                for spec in &specs {
                    spec_to_dep_path.insert(spec.clone(), format!("{res_name}@{version}"));
                }
                continue;
            }
            "patch" | "portal" | "exec" => {
                tracing::warn!(
                    "yarn berry '{}' protocol in block '{}' is not supported — entry skipped",
                    res_protocol,
                    key_str,
                );
                continue;
            }
            "file" => Some(file_protocol_source(res_body)),
            "link" => Some(LocalSource::Link(PathBuf::from(strip_hash_fragment(
                res_body,
            )))),
            // Plain HTTP(S) tarball or git-over-HTTPS: berry records
            // the full URL in the spec, which `parse_berry_spec` split
            // into `res_protocol = "https"` and `res_body =
            // "//host/path..."`. Glue them back together with
            // `<protocol>:<body>` to get the original URL, then let
            // `classify_remote` pick tarball vs git based on `.git`
            // in the URL.
            "http" | "https" => {
                let url = format!("{res_protocol}:{res_body}");
                classify_remote(&url, block)
            }
            // Git via a non-HTTP transport. Covers `git:`, `ssh:`, and
            // the compound `git+ssh:` / `git+https:` / `git+file:`
            // variants berry emits for git deps whose commit is pinned
            // after `#`. Rejoin `<protocol>:<body>` into the full URL
            // the linker will hand to `git clone`.
            p if p == "git" || p == "ssh" || p.starts_with("git+") || p.starts_with("ssh+") => {
                let url = format!("{res_protocol}:{res_body}");
                Some(LocalSource::Git(GitSource {
                    url: strip_commit_hash(&url),
                    committish: None,
                    resolved: extract_commit_hash(&url).unwrap_or_default(),
                }))
            }
            _ => {
                tracing::warn!(
                    "yarn berry unrecognized protocol '{}' in block '{}' — entry skipped",
                    res_protocol,
                    key_str,
                );
                continue;
            }
        };

        // Canonical dep_path: `name@version` for registry packages,
        // whatever `LocalSource::dep_path` returns for non-registry ones.
        // Berry always pairs registry and local deps by `name@version`
        // at the graph layer, so duplicate names with the same version
        // but different protocols collapse — same as the classic writer.
        let dep_path = match &local_source {
            Some(src) => src.dep_path(res_name),
            None => format!("{res_name}@{version}"),
        };

        for spec in &specs {
            spec_to_dep_path.insert(spec.clone(), dep_path.clone());
        }

        // Transitive deps: `name: "protocol:range"`. We store the raw
        // header-style spec (`name@protocol:range`) and rewrite to a
        // dep_path in pass two.
        let raw_deps = collect_dep_map(block, "dependencies");
        let peer_deps = collect_dep_map(block, "peerDependencies");
        let peer_deps_meta = collect_peer_meta(block);
        let optional_deps = collect_dep_map(block, "optionalDependencies");

        // Declared ranges — same source as `raw_deps` / `optional_deps`
        // but kept as the bare range string (no `name@` prefix) so
        // writers can slot them straight back into the output.
        let mut declared: BTreeMap<String, String> = BTreeMap::new();
        for (n, v) in raw_deps.iter().chain(optional_deps.iter()) {
            declared.insert(n.clone(), v.clone());
        }

        let raw_deps_specs: BTreeMap<String, String> = raw_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();
        let optional_deps_specs: BTreeMap<String, String> = optional_deps
            .into_iter()
            .map(|(n, v)| (n.clone(), format!("{n}@{v}")))
            .collect();

        let checksum = block
            .get("checksum")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if !packages.contains_key(&dep_path) {
            packages.insert(
                dep_path.clone(),
                LockedPackage {
                    name: res_name.to_string(),
                    version: version.clone(),
                    integrity: None,
                    yarn_checksum: checksum,
                    dependencies: raw_deps_specs,
                    optional_dependencies: optional_deps_specs,
                    peer_dependencies: peer_deps,
                    peer_dependencies_meta: peer_deps_meta,
                    dep_path: dep_path.clone(),
                    local_source,
                    declared_dependencies: declared,
                    ..Default::default()
                },
            );
        }
    }

    // Second pass: resolve raw header specs on each package's
    // `dependencies` / `optional_dependencies` map to canonical dep_paths.
    let mut resolved_deps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut resolved_opts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for (dep_path, pkg) in &packages {
        let resolve = |raw: &BTreeMap<String, String>| {
            let mut out = BTreeMap::new();
            for (name, raw_spec) in raw {
                if let Some(target) = spec_to_dep_path.get(raw_spec) {
                    out.insert(name.clone(), target.clone());
                }
            }
            out
        };
        resolved_deps.insert(dep_path.clone(), resolve(&pkg.dependencies));
        resolved_opts.insert(dep_path.clone(), resolve(&pkg.optional_dependencies));
    }
    for (dep_path, deps) in resolved_deps {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.dependencies = deps;
        }
    }
    for (dep_path, deps) in resolved_opts {
        if let Some(pkg) = packages.get_mut(&dep_path) {
            pkg.optional_dependencies = deps;
        }
    }

    // Build direct deps from the manifest, using the yarn berry
    // convention that a range `"^1.0.0"` corresponds to the spec
    // `"name@npm:^1.0.0"`. If the manifest range already carries a
    // protocol prefix (`workspace:*`, `file:./pkgs/foo`, ...), it's
    // already a valid spec suffix and we try it verbatim first.
    let mut direct: Vec<DirectDep> = Vec::new();
    let push_direct = |name: &str, range: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        let candidates = berry_spec_candidates(name, range);
        for candidate in candidates {
            if let Some(dep_path) = spec_to_dep_path.get(&candidate) {
                direct.push(DirectDep {
                    name: name.to_string(),
                    dep_path: dep_path.clone(),
                    dep_type,
                    specifier: None,
                });
                return;
            }
        }
    };
    for (name, range) in &manifest.dependencies {
        push_direct(name, range, DepType::Production, &mut direct);
    }
    for (name, range) in &manifest.dev_dependencies {
        push_direct(name, range, DepType::Dev, &mut direct);
    }
    for (name, range) in &manifest.optional_dependencies {
        push_direct(name, range, DepType::Optional, &mut direct);
    }

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), direct);

    Ok(LockfileGraph {
        importers,
        packages,
        ..Default::default()
    })
}

/// Split a berry block header like `"foo@npm:^1.0.0, foo@npm:^2.0.0"`
/// into individual specs. The YAML layer already unquoted the key;
/// berry separates multiple specs inside the single key string with
/// `", "` (space required). Leading/trailing whitespace tolerated.
fn split_berry_header(header: &str) -> Vec<String> {
    header
        .split(", ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a berry spec `<name>@<protocol>:<rest>` into its three parts.
/// Scoped names (`@scope/pkg`) are handled by skipping the leading `@`.
/// Returns `None` if the spec is malformed.
fn parse_berry_spec(spec: &str) -> Option<(&str, &str, &str)> {
    let (name, after_at) = if let Some(rest) = spec.strip_prefix('@') {
        // Scoped: `@scope/pkg@<protocol>:<rest>` — find the second `@`.
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let full_name_len = 1 + slash + 1 + at;
        (&spec[..full_name_len], &spec[full_name_len + 1..])
    } else {
        let at = spec.find('@')?;
        (&spec[..at], &spec[at + 1..])
    };
    let colon = after_at.find(':')?;
    let protocol = &after_at[..colon];
    let body = &after_at[colon + 1..];
    Some((name, protocol, body))
}

/// Build the ordered list of berry spec strings to try when matching
/// a manifest entry against the lockfile's spec-to-dep-path index.
/// First we try the raw `name@range` (covers cases where the manifest
/// already carries a protocol prefix like `workspace:*`); failing
/// that, fall back to `name@npm:range` which is the default berry
/// adds when the user writes an un-prefixed semver range.
fn berry_spec_candidates(name: &str, range: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(2);
    out.push(format!("{name}@{range}"));
    if !range_has_protocol(range) {
        out.push(format!("{name}@npm:{range}"));
    }
    out
}

/// True when a manifest range carries a berry protocol prefix like
/// `npm:...`, `workspace:...`, `file:...`, `link:...`, `portal:...`,
/// `patch:...`, `exec:...`, `git:...`, `git+ssh:...`, `http:`,
/// `https:`. Used to decide whether to prepend `npm:` when building
/// the spec candidate.
fn range_has_protocol(range: &str) -> bool {
    let Some(colon) = range.find(':') else {
        return false;
    };
    let head = &range[..colon];
    // Yarn berry's protocol heads are alphabetic with optional `+`
    // separators for compound transports (`git+ssh`, `git+https`,
    // `git+file`). Digits and other punctuation are not valid
    // protocol chars, which also rules out Windows drive letters
    // (single-letter heads are technically still valid protocols,
    // but yarn itself doesn't emit any and the `file:` spelling
    // handles those deps).
    !head.is_empty() && head.chars().all(|c| c.is_ascii_alphabetic() || c == '+')
}

/// Render a scalar YAML value as its source-text-equivalent string.
///
/// Berry emits `version: 1.0.0` unquoted. Under YAML 1.2 core-schema
/// resolution (what `serde_yaml` 0.9 uses), that bare token parses
/// as a string *only because* it has two dots — a bare integer
/// (`version: 5`) comes out as `Value::Number(5)`, a two-component
/// value (`version: 1.0`) as a float. Returning those back as
/// strings matches what a quote-everything serializer would have
/// produced, so rare packages with one- or two-component versions
/// don't break parsing against a lockfile yarn itself wrote.
///
/// Booleans would behave the same way (`version: yes`), but no real
/// version string collides with YAML 1.2's bool tokens (`true` /
/// `false`), so we don't bother unfolding them.
fn yaml_scalar_as_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Extract a `BTreeMap<name, value>` from a sub-mapping like
/// `dependencies:` or `peerDependencies:`. Missing section or
/// non-mapping values return empty. Values go through
/// `yaml_scalar_as_string` for the same reason `version` does — a
/// bare `dep: 5` would otherwise silently drop the edge instead of
/// recording `"5"` as the range.
fn collect_dep_map(block: &serde_yaml::Mapping, key: &str) -> BTreeMap<String, String> {
    block
        .get(key)
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.as_str()?.to_string(), yaml_scalar_as_string(v)?)))
                .collect()
        })
        .unwrap_or_default()
}

/// Pull `peerDependenciesMeta` into our structured form. Only the
/// `optional` flag round-trips through aube's model; other keys in
/// the meta block (if any) are ignored.
fn collect_peer_meta(block: &serde_yaml::Mapping) -> BTreeMap<String, crate::PeerDepMeta> {
    block
        .get("peerDependenciesMeta")
        .and_then(|v| v.as_mapping())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    let name = k.as_str()?.to_string();
                    let meta = v.as_mapping()?;
                    let optional = meta
                        .get("optional")
                        .and_then(|o| o.as_bool())
                        .unwrap_or(false);
                    Some((name, crate::PeerDepMeta { optional }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a `file:`-protocol body like `./vendor/foo` or
/// `./vendor/foo-1.0.0.tgz#hash` to a [`LocalSource`]. The fragment
/// (`#...`) that berry appends to pin the imported checksum is
/// stripped — aube's `LocalSource` records the path only, and the
/// checksum round-trips via `yarn_checksum`.
fn file_protocol_source(body: &str) -> LocalSource {
    let path = PathBuf::from(strip_hash_fragment(body));
    let is_tarball = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("tgz") || e.eq_ignore_ascii_case("gz"));
    if is_tarball {
        LocalSource::Tarball(path)
    } else {
        LocalSource::Directory(path)
    }
}

fn strip_hash_fragment(s: &str) -> &str {
    s.split_once('#').map(|(a, _)| a).unwrap_or(s)
}

/// If the URL has a `#<commit>` suffix, return `<commit>`. Used for
/// git-over-http berry specs that pin the resolved commit after `#`.
fn extract_commit_hash(url: &str) -> Option<String> {
    url.split_once('#')
        .and_then(|(_, b)| crate::normalize_git_fragment(b))
}

fn strip_commit_hash(url: &str) -> String {
    strip_hash_fragment(url).to_string()
}

/// Classify a remote URL in berry's resolution field as either a git
/// repo (if it has a commit hash suffix or a `.git` path) or a plain
/// tarball download. Checksum / integrity lives on the `checksum:`
/// field and round-trips through `yarn_checksum`.
fn classify_remote(url: &str, _block: &serde_yaml::Mapping) -> Option<LocalSource> {
    if url.ends_with(".git") || url.contains(".git#") {
        Some(LocalSource::Git(GitSource {
            url: strip_commit_hash(url),
            committish: None,
            resolved: extract_commit_hash(url).unwrap_or_default(),
        }))
    } else {
        Some(LocalSource::RemoteTarball(RemoteTarballSource {
            url: strip_commit_hash(url),
            integrity: String::new(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Writer: flat LockfileGraph → yarn berry lockfile
// ---------------------------------------------------------------------------

/// Serialize a [`LockfileGraph`] as a yarn berry (v2+) lockfile.
///
/// Output targets yarn 4's `__metadata.version: 8` / `cacheKey: 10c0`
/// (accepted by yarn 3.x too; yarn 2.x is functionally extinct). The
/// block shape — one entry per canonical `(name, version)` with a
/// comma-separated header of all specifiers that resolve to it — is
/// identical to the classic writer's, just reformatted as YAML with
/// `resolution:` / `checksum:` / `languageName` / `linkType` fields.
///
/// ## What round-trips
///
/// `yarn_checksum` (parsed from `checksum:`), peer-dep metadata, and
/// all resolved transitive edges make it through parse → write →
/// parse unchanged.
///
/// ## What doesn't
///
/// - Peer-contextualized variants collapse onto a single `name@version`
///   block; berry's native encoding uses `virtual:` keys to keep them
///   distinct but aube's graph model doesn't, matching our pnpm/npm
///   writers.
/// - Packages the resolver produced fresh (no berry parse to source
///   `yarn_checksum` from) are written without a `checksum:` field.
///   Yarn's default `checksumBehavior: throw` populates missing
///   checksums on the next install against its own cache.
/// - `patch:` / `portal:` / `exec:` protocols aren't represented in
///   aube's graph and never round-trip.
pub fn write_berry(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    // Collapse peer-context variants to one entry per canonical
    // `(name, version)` — same as the classic writer. The canonical
    // key is `name@version`; we need it to look up a package's extra
    // specifiers in the same map.
    let canonical = crate::build_canonical_map(graph);

    // `extra_specs[canonical_key]` is the set of range-form
    // specifiers (e.g. `"foo@npm:^1.0.0"`) that should appear in the
    // block header alongside the exact `foo@npm:1.2.3` one. Collecting
    // them keeps the header compatible with what yarn itself would
    // produce: every spec that resolves to the same (name, version)
    // gets folded into one block, so transitive lookups of a declared
    // range find a matching header.
    let mut extra_specs: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let manifest_ranges: Vec<(String, String)> = manifest
        .dependencies
        .iter()
        .chain(manifest.dev_dependencies.iter())
        .chain(manifest.optional_dependencies.iter())
        .chain(manifest.peer_dependencies.iter())
        .map(|(n, r)| (n.clone(), r.clone()))
        .collect();
    let format_berry_spec = |dep_name: &str, range: &str| {
        if range_has_protocol(range) {
            format!("{dep_name}@{range}")
        } else {
            format!("{dep_name}@npm:{range}")
        }
    };
    for dep in graph.importers.get(".").into_iter().flatten() {
        let canonical_key = crate::npm::canonical_key_from_dep_path(&dep.dep_path);
        if !canonical.contains_key(&canonical_key) {
            continue;
        }
        let Some((_, range)) = manifest_ranges.iter().find(|(n, _)| n == &dep.name) else {
            continue;
        };
        let manifest_spec = format_berry_spec(&dep.name, range);
        let exact_spec = berry_exact_spec(canonical.get(&canonical_key).copied().unwrap());
        if manifest_spec != exact_spec {
            extra_specs
                .entry(canonical_key)
                .or_default()
                .insert(manifest_spec);
        }
    }
    // Harvest transitive declared ranges, same shape as the classic
    // writer. Berry specs always carry a protocol (`npm:`, `workspace:`,
    // `patch:` …); bare ranges get the default `npm:` prefix.
    for pkg in canonical.values() {
        for (dep_name, range) in &pkg.declared_dependencies {
            let Some(resolved_value) = pkg.dependencies.get(dep_name) else {
                continue;
            };
            let target = crate::npm::child_canonical_key(dep_name, resolved_value);
            let Some(target_pkg) = canonical.get(&target) else {
                continue;
            };
            let manifest_spec = format_berry_spec(dep_name, range);
            let exact_spec = berry_exact_spec(target_pkg);
            if manifest_spec != exact_spec {
                extra_specs.entry(target).or_default().insert(manifest_spec);
            }
        }
    }

    let mut out = String::new();
    out.push_str("# This file is generated by running \"yarn install\" inside your project.\n");
    out.push_str("# Manual changes might be lost - proceed with caution!\n\n");
    out.push_str("__metadata:\n  version: 8\n  cacheKey: 10c0\n\n");

    for (canonical_key, pkg) in &canonical {
        // Every block starts with the exact `name@protocol:version`
        // specifier so transitive lookups (which parse emits as
        // `"name@npm:version"`) find a header match, then appends the
        // manifest range specs collected above.
        let exact_spec = berry_exact_spec(pkg);
        let mut header_specs: Vec<String> = vec![exact_spec.clone()];
        if let Some(extras) = extra_specs.get(canonical_key) {
            for s in extras {
                if !header_specs.contains(s) {
                    header_specs.push(s.clone());
                }
            }
        }
        // Header and `resolution:` both carry spec strings that may
        // contain `:`, `/`, `@`, `#`, or — for `patch:` / file-path
        // sources — backslashes and quotes. Route both through
        // `quote_yaml_scalar` so escaping matches the rest of the
        // writer. For the multi-spec header we quote the joined
        // `", "`-separated list as one string, same as berry.
        let header_inner = header_specs.join(", ");
        out.push_str(&quote_yaml_scalar(&header_inner));
        out.push_str(":\n");

        // Scalar fields: version, resolution.
        out.push_str("  version: ");
        out.push_str(&quote_yaml_scalar(&pkg.version));
        out.push('\n');
        out.push_str("  resolution: ");
        out.push_str(&quote_yaml_scalar(&exact_spec));
        out.push('\n');

        // Dependencies / peerDependencies / peerDependenciesMeta /
        // optionalDependencies: nested YAML mappings with
        // `name: "npm:<range-or-version>"` values. Resolved
        // transitive deps collapse to the exact version of the target
        // block (the resolver produced the graph, so the key always
        // exists in `canonical`).
        write_berry_dep_map(
            &mut out,
            "dependencies",
            &pkg.dependencies,
            &pkg.declared_dependencies,
            &canonical,
        );
        write_berry_dep_map(
            &mut out,
            "optionalDependencies",
            &pkg.optional_dependencies,
            &pkg.declared_dependencies,
            &canonical,
        );
        write_berry_peer_deps(&mut out, &pkg.peer_dependencies);
        write_berry_peer_meta(&mut out, &pkg.peer_dependencies_meta);

        if let Some(checksum) = &pkg.yarn_checksum {
            out.push_str("  checksum: ");
            out.push_str(&quote_yaml_scalar(checksum));
            out.push('\n');
        }
        out.push_str("  languageName: node\n");
        // `linkType: soft` means "just symlink, don't materialize into
        // the virtual store" — what berry uses for `link:` (and
        // `workspace:`) entries. `hard` is the default for registry
        // packages and everything that does get materialized. Picking
        // `hard` for a `link:` block would send yarn's own linker
        // down the tarball-import path the next time it reads our
        // output, which breaks `link:` projects that round-trip
        // through aube.
        let link_type = match &pkg.local_source {
            Some(LocalSource::Link(_)) => "soft",
            _ => "hard",
        };
        out.push_str("  linkType: ");
        out.push_str(link_type);
        out.push_str("\n\n");
    }

    crate::atomic_write_lockfile(path, out.as_bytes())?;
    Ok(())
}

/// The canonical header-spec used for a berry block's first
/// specifier and its `resolution:` field. Registry packages take the
/// form `name@npm:version`; non-registry sources use the protocol
/// recorded in `local_source`.
fn berry_exact_spec(pkg: &LockedPackage) -> String {
    match &pkg.local_source {
        None => format!("{}@npm:{}", pkg.name, pkg.version),
        Some(src) => format!("{}@{}", pkg.name, src.specifier()),
    }
}

fn write_berry_dep_map(
    out: &mut String,
    section: &str,
    deps: &BTreeMap<String, String>,
    declared: &BTreeMap<String, String>,
    canonical: &BTreeMap<String, &LockedPackage>,
) {
    // Only emit edges whose target survives in `canonical`; the
    // graph-level filter layer (e.g. `--prod` prune) may have dropped
    // packages that dev-only edges still reference.
    //
    // Prefer the declared range from the package's own manifest (what
    // berry itself writes — `chalk: "npm:^4.1.0"`) over the resolved
    // pin. Falls back to `npm:<version>` when the declared range is
    // unknown (e.g. a pnpm-sourced graph being re-emitted as yarn).
    let resolved: Vec<(&str, String)> = deps
        .iter()
        .filter_map(|(n, v)| {
            let key = crate::npm::child_canonical_key(n, v);
            let target = canonical.get(&key)?;
            let spec_body = match &target.local_source {
                None => {
                    let body = declared
                        .get(n)
                        .cloned()
                        .unwrap_or_else(|| crate::npm::dep_value_as_version(n, v).to_string());
                    // Declared ranges may already carry a protocol
                    // (`npm:^4`, `workspace:*`, `patch:…`) — don't
                    // double-prefix those. Bare ranges like `^4.1.0`
                    // get the default `npm:` protocol.
                    if body.contains(':') {
                        body
                    } else {
                        format!("npm:{body}")
                    }
                }
                Some(src) => src.specifier(),
            };
            Some((n.as_str(), spec_body))
        })
        .collect();
    if resolved.is_empty() {
        return;
    }
    out.push_str("  ");
    out.push_str(section);
    out.push_str(":\n");
    for (name, body) in resolved {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_yaml_scalar(&body));
        out.push('\n');
    }
}

fn write_berry_peer_deps(out: &mut String, peer: &BTreeMap<String, String>) {
    if peer.is_empty() {
        return;
    }
    out.push_str("  peerDependencies:\n");
    for (name, range) in peer {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(": ");
        out.push_str(&quote_yaml_scalar(range));
        out.push('\n');
    }
}

fn write_berry_peer_meta(out: &mut String, meta: &BTreeMap<String, crate::PeerDepMeta>) {
    if meta.is_empty() {
        return;
    }
    out.push_str("  peerDependenciesMeta:\n");
    for (name, m) in meta {
        out.push_str("    ");
        out.push_str(&quote_yaml_key(name));
        out.push_str(":\n");
        out.push_str("      optional: ");
        out.push_str(if m.optional { "true" } else { "false" });
        out.push('\n');
    }
}

/// Quote a YAML scalar so it round-trips through a standard YAML
/// parser regardless of punctuation in the content (`:`, `^`, `*`,
/// `@`, protocol prefixes, leading digits). Berry itself quotes
/// liberally; we do too. Double quotes are backslash-escaped.
fn quote_yaml_scalar(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Scoped package names start with `@`, which YAML interprets as a
/// reserved indicator character. Quoting them is required; bare
/// un-scoped names round-trip without quotes, but we quote them too
/// for consistency with berry's own output.
fn quote_yaml_key(s: &str) -> String {
    quote_yaml_scalar(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest(deps: &[(&str, &str)], dev: &[(&str, &str)]) -> aube_manifest::PackageJson {
        aube_manifest::PackageJson {
            name: Some("test".to_string()),
            version: Some("1.0.0".to_string()),
            dependencies: deps
                .iter()
                .map(|(n, r)| (n.to_string(), r.to_string()))
                .collect(),
            dev_dependencies: dev
                .iter()
                .map(|(n, r)| (n.to_string(), r.to_string()))
                .collect(),
            peer_dependencies: Default::default(),
            optional_dependencies: Default::default(),
            update_config: None,
            scripts: Default::default(),
            engines: Default::default(),
            workspaces: None,
            bundled_dependencies: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn test_parse_spec_name() {
        assert_eq!(parse_spec_name("foo@^1.0.0"), Some("foo".to_string()));
        assert_eq!(parse_spec_name("foo@1.2.3"), Some("foo".to_string()));
        assert_eq!(
            parse_spec_name("@scope/pkg@^1.0.0"),
            Some("@scope/pkg".to_string())
        );
        assert_eq!(parse_spec_name("foo"), None);
    }

    #[test]
    fn test_parse_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  resolved "https://example.com/foo-1.2.3.tgz"
  integrity sha512-aaa
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  resolved "https://example.com/bar-2.5.0.tgz"
  integrity sha512-bbb
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        assert_eq!(graph.packages.len(), 2);
        assert!(graph.packages.contains_key("foo@1.2.3"));
        assert!(graph.packages.contains_key("bar@2.5.0"));

        let foo = &graph.packages["foo@1.2.3"];
        assert_eq!(foo.integrity.as_deref(), Some("sha512-aaa"));
        assert_eq!(
            foo.dependencies.get("bar").map(String::as_str),
            Some("bar@2.5.0")
        );

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "foo");
        assert_eq!(root[0].dep_path, "foo@1.2.3");
    }

    #[test]
    fn test_parse_scoped_and_multi_spec() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"# yarn lockfile v1

"@scope/pkg@^1.0.0", "@scope/pkg@^1.1.0":
  version "1.1.0"
  integrity sha512-zzz
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
        let root = graph.importers.get(".").unwrap();
        assert_eq!(root[0].name, "@scope/pkg");
        assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
    }

    #[test]
    fn test_detect_berry_vs_classic() {
        // The `__metadata:` marker is what distinguishes berry from
        // classic; `is_berry` is the primary dispatcher signal so we
        // assert it fires on every version berry has emitted
        // (`__metadata.version` 3 through 8 across yarn 2–4).
        assert!(is_berry("__metadata:\n  version: 6\n"));
        assert!(is_berry("# comment\n__metadata:\n  version: 8\n"));
        assert!(!is_berry(
            "# yarn lockfile v1\n\nfoo@^1.0.0:\n  version \"1.0.0\"\n"
        ));
    }

    /// Parse → write → parse should preserve package set,
    /// versions, integrity, and the resolved transitive graph. If
    /// the writer emits malformed block headers or forgets to
    /// requote, round-trip breaks here.
    #[test]
    fn test_write_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"# yarn lockfile v1

foo@^1.0.0:
  version "1.2.3"
  integrity sha512-foo
  dependencies:
    bar "^2.0.0"

bar@^2.0.0:
  version "2.5.0"
  integrity sha512-bar
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        let out = tempfile::NamedTempFile::new().unwrap();
        write_classic(out.path(), &graph, &manifest).unwrap();

        // Re-parse the output. The manifest is the same — direct-dep
        // resolution requires a spec key of `foo@^1.0.0`, but the
        // writer emits `"foo@1.2.3"`. So direct-dep lookup will
        // miss; we only assert the packages/transitives round-trip.
        let reparsed_manifest = make_manifest(&[], &[]);
        let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

        assert!(reparsed.packages.contains_key("foo@1.2.3"));
        assert!(reparsed.packages.contains_key("bar@2.5.0"));
        assert_eq!(
            reparsed.packages["foo@1.2.3"].integrity.as_deref(),
            Some("sha512-foo")
        );
        // foo's transitive dep on bar must still resolve: the writer
        // emits `bar "2.5.0"` under foo's dependencies, and reparse
        // finds the block keyed `"bar@2.5.0"` via spec_to_dep_path.
        assert_eq!(
            reparsed.packages["foo@1.2.3"]
                .dependencies
                .get("bar")
                .map(String::as_str),
            Some("bar@2.5.0")
        );
    }

    #[test]
    fn test_dev_dep_classification() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"foo@^1.0.0:
  version "1.0.0"
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[], &[("foo", "^1.0.0")]);
        let graph = parse(tmp.path(), &manifest).unwrap();
        let root = graph.importers.get(".").unwrap();
        assert_eq!(root[0].dep_type, DepType::Dev);
    }

    // ---- berry (v2+) ---------------------------------------------------

    #[test]
    fn test_parse_berry_spec() {
        assert_eq!(
            parse_berry_spec("lodash@npm:^4.17.0"),
            Some(("lodash", "npm", "^4.17.0"))
        );
        assert_eq!(
            parse_berry_spec("@types/node@npm:20.1.0"),
            Some(("@types/node", "npm", "20.1.0"))
        );
        assert_eq!(
            parse_berry_spec("my-pkg@workspace:."),
            Some(("my-pkg", "workspace", "."))
        );
        // Missing protocol colon: malformed.
        assert_eq!(parse_berry_spec("no-protocol"), None);
    }

    #[test]
    fn test_split_berry_header() {
        let specs = split_berry_header("lodash@npm:^4.17.0, lodash@npm:^4.18.0");
        assert_eq!(
            specs,
            vec![
                "lodash@npm:^4.17.0".to_string(),
                "lodash@npm:^4.18.0".to_string()
            ]
        );
        let single = split_berry_header("foo@npm:1.0.0");
        assert_eq!(single, vec!["foo@npm:1.0.0".to_string()]);
    }

    #[test]
    fn test_range_has_protocol() {
        assert!(range_has_protocol("npm:^1.0.0"));
        assert!(range_has_protocol("workspace:*"));
        assert!(range_has_protocol("file:./pkgs/foo"));
        assert!(range_has_protocol("patch:react@^18.0.0#./mypatch.patch"));
        // Compound transports: berry emits these for git-over-ssh /
        // git-over-https, and the writer must not re-prefix them with
        // `npm:` when building header specs from the manifest range.
        assert!(range_has_protocol("git+ssh://git@github.com/u/r.git"));
        assert!(range_has_protocol("git+https://github.com/u/r.git"));
        assert!(range_has_protocol("git+file:./vendored.git"));
        // Bare semver ranges never have a protocol.
        assert!(!range_has_protocol("^1.0.0"));
        assert!(!range_has_protocol("1.2.3"));
        assert!(!range_has_protocol(">=1.0 <2.0"));
    }

    /// Realistic yarn 4 lockfile with `npm:` deps — the overwhelming
    /// majority real-world case. Exercises `__metadata` parsing,
    /// multi-spec block headers, nested `dependencies:`, and the
    /// direct-dep pass that prepends `npm:` to manifest ranges.
    #[test]
    fn test_parse_berry_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"# This file is generated by running "yarn install" inside your project.
# Manual changes might be lost - proceed with caution!

__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/abcdef
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/123456
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        assert_eq!(graph.packages.len(), 2);
        let foo = &graph.packages["foo@1.2.3"];
        assert_eq!(foo.version, "1.2.3");
        assert_eq!(foo.yarn_checksum.as_deref(), Some("10c0/abcdef"));
        assert_eq!(
            foo.dependencies.get("bar").map(String::as_str),
            Some("bar@2.5.0")
        );

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "foo");
        assert_eq!(root[0].dep_path, "foo@1.2.3");
    }

    /// Scoped package names (`@types/node`) and the `, `-joined
    /// multi-spec header format berry uses when two package.json
    /// ranges resolve to the same version.
    #[test]
    fn test_parse_berry_scoped_and_multi_spec() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"@scope/pkg@npm:^1.0.0, @scope/pkg@npm:^1.1.0":
  version: 1.1.0
  resolution: "@scope/pkg@npm:1.1.0"
  checksum: 10c0/zzz
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("@scope/pkg", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        assert!(graph.packages.contains_key("@scope/pkg@1.1.0"));
        let root = graph.importers.get(".").unwrap();
        assert_eq!(root[0].name, "@scope/pkg");
        assert_eq!(root[0].dep_path, "@scope/pkg@1.1.0");
    }

    /// Blocks for the project's own workspace entry shouldn't become
    /// `LockedPackage`s — they're the root importer, not a
    /// resolved dep. Skipping them keeps the graph shape identical to
    /// what parsing the `package.json` alone would produce.
    #[test]
    fn test_parse_berry_skips_workspace_root() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"my-project@workspace:.":
  version: 0.0.0-use.local
  resolution: "my-project@workspace:."
  dependencies:
    foo: "npm:^1.0.0"
  languageName: unknown
  linkType: soft

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        // Workspace block is skipped; only the real resolved dep survives.
        assert_eq!(graph.packages.len(), 1);
        assert!(graph.packages.contains_key("foo@1.0.0"));
        assert!(!graph.packages.contains_key("my-project@0.0.0-use.local"));
    }

    /// Berry emits `version:` unquoted, and under YAML 1.2 core-schema
    /// resolution a bare integer (`version: 5`) comes out as a
    /// number, not a string. Our parser must unfold those back to
    /// strings instead of failing with "has no version" — real
    /// packages with fewer-than-three-component versions do exist
    /// (even if rare).
    #[test]
    fn test_parse_berry_unquoted_numeric_version() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"int-version@npm:5":
  version: 5
  resolution: "int-version@npm:5"
  languageName: node
  linkType: hard

"two-part@npm:1.0":
  version: 1.0
  resolution: "two-part@npm:1.0"
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        assert!(graph.packages.contains_key("int-version@5"));
        assert!(graph.packages.contains_key("two-part@1.0"));
        assert_eq!(graph.packages["int-version@5"].version, "5");
        assert_eq!(graph.packages["two-part@1.0"].version, "1.0");
    }

    /// Same numeric-scalar hazard applies to dependency values:
    /// `peerDependencies: { foo: 5 }` writes a YAML number, and
    /// `as_str()` would silently drop the edge. The fix routes dep
    /// values through `yaml_scalar_as_string`; this test exercises
    /// that path end-to-end so a future regression would show up as
    /// a missing peer edge rather than a parse error.
    #[test]
    fn test_parse_berry_numeric_dep_value() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.0.0
  resolution: "foo@npm:1.0.0"
  peerDependencies:
    numeric-peer: 5
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();
        let foo = &graph.packages["foo@1.0.0"];
        assert_eq!(
            foo.peer_dependencies
                .get("numeric-peer")
                .map(String::as_str),
            Some("5")
        );
    }

    /// Berry's `https:` tarball protocol and `git+ssh:` / `git:`
    /// transports both survive parsing with a populated
    /// `LocalSource`, rather than falling through to the "unknown
    /// protocol" skip path.
    ///
    /// The hazard this guards against: `parse_berry_spec` splits
    /// `"foo@https://host/path"` into `res_protocol = "https"` /
    /// `res_body = "//host/path"` — the body never starts with
    /// `https://`, so a URL-body check would always miss. Parsing the
    /// file and verifying the package lands in the graph with the
    /// right `LocalSource` catches any future regression of the
    /// dispatch match arms.
    #[test]
    fn test_parse_berry_http_and_git_protocols() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"tarball-pkg@https://example.com/pkg-1.0.0.tgz":
  version: 1.0.0
  resolution: "tarball-pkg@https://example.com/pkg-1.0.0.tgz"
  languageName: node
  linkType: hard

"git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01":
  version: 2.0.0
  resolution: "git-pkg@https://github.com/user/repo.git#commit=abcdef0123456789abcdef0123456789abcdef01"
  languageName: node
  linkType: hard

"ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef":
  version: 3.0.0
  resolution: "ssh-git-pkg@git+ssh://git@github.com/user/other.git#deadbeef"
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        // All three packages should be present — none silently
        // skipped as "unrecognized protocol". The `.values()` scan
        // below asserts the `LocalSource` shape for each.
        assert_eq!(graph.packages.len(), 3);
        let by_name: BTreeMap<&str, &LockedPackage> = graph
            .packages
            .values()
            .map(|p| (p.name.as_str(), p))
            .collect();

        // `.tgz` on https → remote tarball.
        let tar = by_name["tarball-pkg"];
        assert!(matches!(
            &tar.local_source,
            Some(LocalSource::RemoteTarball(_))
        ));

        // `.git` on https → git source, not tarball.
        let git = by_name["git-pkg"];
        let Some(LocalSource::Git(git)) = &git.local_source else {
            panic!("expected git LocalSource");
        };
        assert_eq!(git.url, "https://github.com/user/repo.git");
        assert_eq!(git.resolved, "abcdef0123456789abcdef0123456789abcdef01");

        // `git+ssh:` prefix → git source.
        let ssh = by_name["ssh-git-pkg"];
        assert!(matches!(&ssh.local_source, Some(LocalSource::Git(_))));
    }

    /// Round-trip: parse berry → write berry → parse berry should
    /// preserve packages, versions, checksum (via `yarn_checksum`),
    /// and transitive edges. This is the core round-trip contract.
    #[test]
    fn test_write_berry_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"__metadata:
  version: 8
  cacheKey: 10c0

"foo@npm:^1.0.0":
  version: 1.2.3
  resolution: "foo@npm:1.2.3"
  dependencies:
    bar: "npm:^2.0.0"
  checksum: 10c0/foohash
  languageName: node
  linkType: hard

"bar@npm:^2.0.0":
  version: 2.5.0
  resolution: "bar@npm:2.5.0"
  checksum: 10c0/barhash
  languageName: node
  linkType: hard
"#;
        std::fs::write(tmp.path(), content).unwrap();
        let manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let graph = parse(tmp.path(), &manifest).unwrap();

        let out = tempfile::NamedTempFile::new().unwrap();
        write_berry(out.path(), &graph, &manifest).unwrap();

        // Confirm the output is berry-shaped so dispatcher picks the
        // right parser on reparse.
        let written = std::fs::read_to_string(out.path()).unwrap();
        assert!(is_berry(&written));

        let reparsed_manifest = make_manifest(&[("foo", "^1.0.0")], &[]);
        let reparsed = parse(out.path(), &reparsed_manifest).unwrap();

        assert!(reparsed.packages.contains_key("foo@1.2.3"));
        assert!(reparsed.packages.contains_key("bar@2.5.0"));
        assert_eq!(
            reparsed.packages["foo@1.2.3"].yarn_checksum.as_deref(),
            Some("10c0/foohash")
        );
        assert_eq!(
            reparsed.packages["foo@1.2.3"]
                .dependencies
                .get("bar")
                .map(String::as_str),
            Some("bar@2.5.0")
        );
        // The manifest spec `foo@^1.0.0` appears verbatim (with `npm:`
        // prepended) in the block header, so direct-dep lookup
        // succeeds on reparse — which it did NOT for classic, so this
        // is a stronger round-trip guarantee.
        let root = reparsed.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].dep_path, "foo@1.2.3");
    }

    /// `link:` deps are pure symlinks in berry's model, which means
    /// the block must carry `linkType: soft` — writing `hard` makes
    /// yarn's own linker try to copy/hardlink the target into the
    /// virtual store on the next install. Registry packages (no
    /// `local_source`) stay `hard`, the default.
    #[test]
    fn test_write_berry_link_type_soft_for_link_deps() {
        let mut packages = BTreeMap::new();
        packages.insert(
            "linked-pkg@1.0.0".to_string(),
            LockedPackage {
                name: "linked-pkg".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "linked-pkg@1.0.0".to_string(),
                local_source: Some(LocalSource::Link(PathBuf::from("./vendor/linked-pkg"))),
                ..Default::default()
            },
        );
        packages.insert(
            "regular-pkg@2.0.0".to_string(),
            LockedPackage {
                name: "regular-pkg".to_string(),
                version: "2.0.0".to_string(),
                dep_path: "regular-pkg@2.0.0".to_string(),
                ..Default::default()
            },
        );
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(".".to_string(), vec![]);
                m
            },
            packages,
            ..Default::default()
        };
        let manifest = make_manifest(&[], &[]);

        let out = tempfile::NamedTempFile::new().unwrap();
        write_berry(out.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(out.path()).unwrap();

        // The `link:` block gets `soft`; the registry block stays `hard`.
        // Block order is sorted by canonical key, so `linked-pkg`
        // comes before `regular-pkg` and each block's `linkType`
        // appears after its `languageName` line.
        let linked_idx = written.find("linked-pkg@").unwrap();
        let regular_idx = written.find("regular-pkg@").unwrap();
        let linked_block = &written[linked_idx..regular_idx];
        let regular_block = &written[regular_idx..];
        assert!(
            linked_block.contains("linkType: soft"),
            "link: block should be soft-linked:\n{linked_block}"
        );
        assert!(
            regular_block.contains("linkType: hard"),
            "registry block should be hard-linked:\n{regular_block}"
        );
    }

    /// Header and `resolution:` both carry spec strings that may
    /// contain backslashes (Windows-style `file:` paths) or embedded
    /// quotes (patched-package descriptors). The writer must route
    /// them through `quote_yaml_scalar` so the emitted YAML is
    /// well-formed. We can't easily drive backslashes into the model
    /// from a parsed berry file (berry itself doesn't emit them on
    /// macOS/Linux), so we construct a package with a `file:` source
    /// that contains a backslash directly and assert the output
    /// escapes it and round-trips through `serde_yaml::from_str`.
    #[test]
    fn test_write_berry_escapes_resolution_and_header() {
        let mut packages = BTreeMap::new();
        packages.insert(
            "weird-pkg@1.0.0".to_string(),
            LockedPackage {
                name: "weird-pkg".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "weird-pkg@1.0.0".to_string(),
                // A file: source whose path has a backslash. The
                // header and resolution both become
                // `weird-pkg@file:./a\b/c`; without escaping, the
                // raw backslash in the YAML string would be a
                // malformed escape.
                local_source: Some(LocalSource::Directory(PathBuf::from("./a\\b/c"))),
                ..Default::default()
            },
        );
        let graph = LockfileGraph {
            importers: {
                let mut m = BTreeMap::new();
                m.insert(".".to_string(), vec![]);
                m
            },
            packages,
            ..Default::default()
        };
        let manifest = make_manifest(&[], &[]);

        let out = tempfile::NamedTempFile::new().unwrap();
        write_berry(out.path(), &graph, &manifest).unwrap();
        let written = std::fs::read_to_string(out.path()).unwrap();

        // The emitted file must parse as YAML — any missing escape
        // blows up here instead of corrupting a real install.
        let _doc: serde_yaml::Value = serde_yaml::from_str(&written)
            .unwrap_or_else(|e| panic!("berry writer produced malformed YAML: {e}\n{written}"));
    }
}
