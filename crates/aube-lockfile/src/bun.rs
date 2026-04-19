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

use crate::{DepType, DirectDep, Error, LockedPackage, LockfileGraph};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct RawBunLockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u32,
    #[serde(default)]
    workspaces: BTreeMap<String, RawBunWorkspace>,
    #[serde(default)]
    packages: BTreeMap<String, Vec<serde_json::Value>>,
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

fn is_integrity_hash(s: &str) -> bool {
    matches!(
        s.split_once('-').map(|(algo, _)| algo),
        Some("sha512" | "sha384" | "sha256" | "sha1" | "md5")
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBunMeta {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

/// Parse a bun.lock file into a LockfileGraph.
pub fn parse(path: &Path) -> Result<LockfileGraph, Error> {
    let raw_content =
        std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    let cleaned = strip_jsonc(&raw_content);

    let raw: RawBunLockfile = serde_json::from_str(&cleaned)
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;

    if raw.lockfile_version != 1 {
        return Err(Error::Parse(
            path.to_path_buf(),
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
        let entry =
            BunEntry::from_array(key, value).map_err(|e| Error::Parse(path.to_path_buf(), e))?;
        entries.insert(key.clone(), entry);
    }

    // First pass: parse (name, version) for each entry. bun.lock keys look
    // like the package name ("foo") for the hoisted version, or a nested
    // path ("parent/foo") when multiple versions exist.
    let mut key_info: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut packages: BTreeMap<String, LockedPackage> = BTreeMap::new();

    for (key, entry) in &entries {
        let Some((name, version)) = split_ident(&entry.ident) else {
            return Err(Error::Parse(
                path.to_path_buf(),
                format!(
                    "could not parse ident '{}' for package '{}'",
                    entry.ident, key
                ),
            ));
        };
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

        packages.insert(
            dep_path.clone(),
            LockedPackage {
                name,
                version,
                integrity: entry.integrity.clone().filter(|s| !s.is_empty()),
                dependencies: deps,
                dep_path,
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
            pkg.dependencies = deps;
        }
    }

    // Root importer from the "" workspace entry.
    let root = raw
        .workspaces
        .get("")
        .cloned()
        .unwrap_or(RawBunWorkspace::default());

    // Root importer: deps always map to top-level entries keyed by bare package name.
    let mut direct: Vec<DirectDep> = Vec::new();
    let push = |name: &str, dep_type: DepType, direct: &mut Vec<DirectDep>| {
        if let Some((dname, dver)) = key_info.get(name) {
            direct.push(DirectDep {
                name: dname.clone(),
                dep_path: format!("{dname}@{dver}"),
                dep_type,
                specifier: None,
            });
        }
    };
    for n in root.dependencies.keys() {
        push(n, DepType::Production, &mut direct);
    }
    for n in root.dev_dependencies.keys() {
        push(n, DepType::Dev, &mut direct);
    }
    for n in root.optional_dependencies.keys() {
        push(n, DepType::Optional, &mut direct);
    }

    let mut importers = BTreeMap::new();
    importers.insert(".".to_string(), direct);

    Ok(LockfileGraph {
        importers,
        packages,
        ..Default::default()
    })
}

impl Clone for RawBunWorkspace {
    fn clone(&self) -> Self {
        Self {
            dependencies: self.dependencies.clone(),
            dev_dependencies: self.dev_dependencies.clone(),
            optional_dependencies: self.optional_dependencies.clone(),
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

        // Line comment
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        // Block comment
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2.min(bytes.len() - i.min(bytes.len()));
            continue;
        }

        // Trailing comma: drop `,` if the next non-whitespace char is `}` or `]`
        if c == b',' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
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
/// Lossy areas (same family as the npm writer):
///   - `resolved` is written as an empty string — we don't persist
///     origin URLs in [`LockedPackage`]. bun reparse is unaffected
///     because its parser explicitly ignores field 1.
///   - Peer-contextualized variants collapse to a single
///     `name@version` entry.
///   - Workspace importers beyond the root aren't walked.
pub fn write(
    path: &Path,
    graph: &LockfileGraph,
    manifest: &aube_manifest::PackageJson,
) -> Result<(), Error> {
    use serde_json::{Value, json};

    // Canonicalize to one entry per (name, version).
    let mut canonical: BTreeMap<String, &LockedPackage> = BTreeMap::new();
    for pkg in graph.packages.values() {
        canonical
            .entry(format!("{}@{}", pkg.name, pkg.version))
            .or_insert(pkg);
    }

    let roots = graph.importers.get(".").cloned().unwrap_or_default();
    let tree = crate::npm::build_hoist_tree(&canonical, &roots);

    // Root workspace entry (`""` in bun.lock): mirror the manifest's
    // direct-dep sections so `bun install --frozen-lockfile` can
    // match against package.json without our having to carry bun's
    // own specifier fields through the graph.
    let mut root_obj = serde_json::Map::new();
    if let Some(name) = &manifest.name {
        root_obj.insert("name".to_string(), json!(name));
    }
    if let Some(version) = &manifest.version {
        root_obj.insert("version".to_string(), json!(version));
    }
    if !manifest.dependencies.is_empty() {
        root_obj.insert("dependencies".to_string(), json!(manifest.dependencies));
    }
    if !manifest.dev_dependencies.is_empty() {
        root_obj.insert(
            "devDependencies".to_string(),
            json!(manifest.dev_dependencies),
        );
    }
    if !manifest.optional_dependencies.is_empty() {
        root_obj.insert(
            "optionalDependencies".to_string(),
            json!(manifest.optional_dependencies),
        );
    }
    if !manifest.peer_dependencies.is_empty() {
        root_obj.insert(
            "peerDependencies".to_string(),
            json!(manifest.peer_dependencies),
        );
    }

    let mut packages_obj = serde_json::Map::new();
    for (segs, canonical_key) in &tree {
        let Some(pkg) = canonical.get(canonical_key).copied() else {
            continue;
        };

        // Bun's key form: `foo` (hoisted) or `parent/foo` (nested).
        // Scoped names like `@scope/name` already carry their own
        // internal `/` and are joined wholesale — bun's parser
        // recognizes `@`-prefixed segments as a single unit.
        let bun_key = segs.join("/");

        // Metadata object: transitive deps keyed by name → version.
        // Filter out deps we don't have a canonical entry for (e.g.
        // dropped optional deps).
        let mut deps_obj = serde_json::Map::new();
        for (dep_name, dep_value) in &pkg.dependencies {
            let key = crate::npm::child_canonical_key(dep_name, dep_value);
            if !canonical.contains_key(&key) {
                continue;
            }
            let version = crate::npm::dep_value_as_version(dep_name, dep_value);
            deps_obj.insert(dep_name.clone(), Value::String(version.to_string()));
        }
        let mut meta = serde_json::Map::new();
        if !deps_obj.is_empty() {
            meta.insert("dependencies".to_string(), Value::Object(deps_obj));
        }

        let ident = format!("{}@{}", pkg.name, pkg.version);
        let integrity = pkg.integrity.clone().unwrap_or_default();
        let entry = Value::Array(vec![
            Value::String(ident),
            Value::String(String::new()),
            Value::Object(meta),
            Value::String(integrity),
        ]);
        packages_obj.insert(bun_key, entry);
    }

    let mut root_workspace = serde_json::Map::new();
    root_workspace.insert("".to_string(), Value::Object(root_obj));

    let mut doc = serde_json::Map::new();
    doc.insert("lockfileVersion".to_string(), json!(1));
    doc.insert("workspaces".to_string(), Value::Object(root_workspace));
    doc.insert("packages".to_string(), Value::Object(packages_obj));

    let mut body = serde_json::to_string_pretty(&Value::Object(doc))
        .map_err(|e| Error::Parse(path.to_path_buf(), e.to_string()))?;
    body.push('\n');
    std::fs::write(path, body).map_err(|e| Error::Io(path.to_path_buf(), e))?;
    Ok(())
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

    #[test]
    fn test_parse_simple() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
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
    "foo": ["foo@1.2.3", "", { "dependencies": { "nested": "^3.0.0" } }, "sha512-aaa"],
    "nested": ["nested@3.1.0", "", {}, "sha512-bbb"],
    "bar": ["bar@2.5.0", "", {}, "sha512-ccc"],
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
        let content = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": {
      "dependencies": { "vfs": "github:collinstevens/vfs#0b6ea53" }
    }
  },
  "packages": {
    "vfs": ["vfs@github:collinstevens/vfs#0b6ea53abcdef", { "dependencies": { "dep": "^1.0.0" } }, "collinstevens-vfs-0b6ea53"],
    "dep": ["dep@1.0.0", "", {}, "sha512-depintegrity"]
  }
}"#;
        std::fs::write(tmp.path(), content).unwrap();
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
        // No sha-*-style hash on the github entry → integrity stays None.
        assert!(vfs.integrity.is_none());

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
            Some("sha512-top-bar")
        );
        assert_eq!(
            reparsed.packages["bar@1.0.0"].integrity.as_deref(),
            Some("sha512-nested-bar")
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
}
