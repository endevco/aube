use crate::{DepType, DirectDep, Error, LockedPackage, LockfileGraph};
use std::collections::BTreeMap;
use std::path::Path;

/// Parse a yarn classic (v1) lockfile's pre-read contents.
pub(super) fn parse_classic_str(
    path: &Path,
    content: &str,
    manifest: &aube_manifest::PackageJson,
) -> Result<LockfileGraph, Error> {
    let blocks = tokenize_blocks(content).map_err(|e| Error::parse(path, e))?;

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
                Error::parse(
                    path,
                    format!("yarn.lock block {:?} has no version", block.specs),
                )
            })?
            .clone();

        // All specs in the key map to the same resolved package.
        // Extract the package name from the first spec.
        let name = parse_spec_name(&block.specs[0]).ok_or_else(|| {
            Error::parse(
                path,
                format!(
                    "could not parse package name from yarn.lock spec '{}'",
                    block.specs[0]
                ),
            )
        })?;
        // npm-protocol alias: `<alias>@npm:<real-name>@<version>`. `name`
        // stays the alias (matches the npm parser's convention — it keys
        // node_modules/<alias>/ and is what consumers refer to); the real
        // registry name lives in `alias_of` so registry_name() returns it.
        // Scan every spec — our writer emits the canonical `name@version`
        // first and the npm-alias spec alongside it, so checking only
        // specs[0] would miss the alias on round-trips.
        let alias_of = block
            .specs
            .iter()
            .find_map(|s| parse_npm_alias_real_name(s))
            .filter(|real| real.as_str() != name);

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
                    alias_of: alias_of.clone(),
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
pub(super) fn parse_spec_name(spec: &str) -> Option<String> {
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

/// Detect a yarn npm-protocol alias spec like
/// `<alias>@npm:<real-name>@<version-or-range>` and return the real
/// registry name. Returns `None` for non-aliased specs (the common case).
///
/// Yarn lets a consumer rename a dep on import — `react-loadable: "npm:@docusaurus/react-loadable@5.5.2"`
/// installs `@docusaurus/react-loadable` under `node_modules/react-loadable/`.
/// The lockfile records the alias as the spec key; without surfacing the
/// real name into [`LockedPackage::alias_of`], every registry/store call
/// site would hit the alias-qualified URL and 404.
pub(super) fn parse_npm_alias_real_name(spec: &str) -> Option<String> {
    let after_alias = if let Some(rest) = spec.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        &after_slash[at + 1..]
    } else {
        let at = spec.find('@')?;
        &spec[at + 1..]
    };
    let after_protocol = after_alias.strip_prefix("npm:")?;
    if let Some(rest) = after_protocol.strip_prefix('@') {
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        Some(format!("@{}", &rest[..slash + 1 + at]))
    } else {
        let at = after_protocol.find('@')?;
        Some(after_protocol[..at].to_string())
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

    let mut out = String::with_capacity(canonical.len().saturating_mul(256).max(4096));
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
