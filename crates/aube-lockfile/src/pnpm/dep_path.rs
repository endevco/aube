use crate::LockedPackage;
use std::collections::BTreeMap;

pub(super) fn version_to_dep_path(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

pub(super) fn dep_path_tail<'a>(dep_path: &'a str, name: &str) -> &'a str {
    dep_path
        .strip_prefix(&format!("{name}@"))
        .unwrap_or(dep_path)
}

pub(super) fn peerless_dep_path(name: &str, value: &str) -> String {
    version_to_dep_path(name, value.split('(').next().unwrap_or(value))
}

pub(super) fn peerless_alias_target<'a>(
    packages: &'a BTreeMap<String, LockedPackage>,
    real_dep_path: &str,
) -> Option<&'a LockedPackage> {
    let (real_name, real_version) = parse_dep_path(real_dep_path)?;
    packages.get(&version_to_dep_path(&real_name, &real_version))
}

/// Parse a dep path like "@scope/name@1.0.0" or "name@1.0.0" into (name, version).
pub(super) fn parse_dep_path(dep_path: &str) -> Option<(String, String)> {
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
pub(super) fn rewrite_snapshot_alias_deps(
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
