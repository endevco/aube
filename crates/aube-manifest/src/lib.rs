pub mod workspace;

pub use workspace::WorkspaceConfig;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_dependencies: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_config: Option<UpdateConfig>,
    /// `bundledDependencies` (or the alias `bundleDependencies`) from
    /// package.json. Names listed here are shipped *inside* the package
    /// tarball itself, under the package's own `node_modules/`. The
    /// resolver must not recurse into them, and Node's directory walk
    /// serves them straight out of the extracted tree.
    #[serde(
        default,
        alias = "bundleDependencies",
        skip_serializing_if = "Option::is_none"
    )]
    pub bundled_dependencies: Option<BundledDependencies>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scripts: BTreeMap<String, String>,
    /// `engines` field — declared runtime version constraints, e.g.
    /// `{"node": ">=18.0.0"}`. Checked against the current runtime during
    /// `aube install`; a mismatch warns by default and fails under
    /// `engine-strict`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub engines: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspaces: Option<Workspaces>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// `bundledDependencies` shape from package.json. npm/pnpm accept
/// either an array of dep names or a boolean (`true` meaning "bundle
/// everything in `dependencies`"). We preserve both so the resolver
/// can compute the exact name set.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum BundledDependencies {
    List(Vec<String>),
    All(bool),
}

impl BundledDependencies {
    /// The set of dep names that should be treated as bundled, given
    /// the package's own `dependencies` map (needed for the `true`
    /// form, which means "bundle every production dep").
    pub fn names<'a>(&'a self, dependencies: &'a BTreeMap<String, String>) -> Vec<&'a str> {
        match self {
            BundledDependencies::List(v) => v.iter().map(String::as_str).collect(),
            BundledDependencies::All(true) => dependencies.keys().map(String::as_str).collect(),
            BundledDependencies::All(false) => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Workspaces {
    Array(Vec<String>),
    Object {
        // `packages` stays required (no `#[serde(default)]`) so that a
        // typo like `"pacakges"` fails deserialization instead of
        // silently producing an empty vec. Bun's object form always
        // includes `packages`, so this doesn't lock out the catalog use
        // case.
        packages: Vec<String>,
        #[serde(default)]
        nohoist: Vec<String>,
        /// Bun-style default catalog nested under `workspaces.catalog`.
        /// Aube reads it in addition to `pnpm-workspace.yaml`'s `catalog:`
        /// so bun projects that migrated config into package.json keep
        /// working.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        catalog: BTreeMap<String, String>,
        /// Bun-style named catalogs nested under `workspaces.catalogs`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        catalogs: BTreeMap<String, BTreeMap<String, String>>,
    },
}

impl Workspaces {
    pub fn patterns(&self) -> &[String] {
        match self {
            Workspaces::Array(v) => v,
            Workspaces::Object { packages, .. } => packages,
        }
    }

    /// Bun-style default catalog (`workspaces.catalog`). Empty when the
    /// `workspaces` field is an array or the object form has no catalog.
    pub fn catalog(&self) -> &BTreeMap<String, String> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, String>> = std::sync::OnceLock::new();
        match self {
            Workspaces::Array(_) => EMPTY.get_or_init(BTreeMap::new),
            Workspaces::Object { catalog, .. } => catalog,
        }
    }

    /// Bun-style named catalogs (`workspaces.catalogs`).
    pub fn catalogs(&self) -> &BTreeMap<String, BTreeMap<String, String>> {
        static EMPTY: std::sync::OnceLock<BTreeMap<String, BTreeMap<String, String>>> =
            std::sync::OnceLock::new();
        match self {
            Workspaces::Array(_) => EMPTY.get_or_init(BTreeMap::new),
            Workspaces::Object { catalogs, .. } => catalogs,
        }
    }
}

impl PackageJson {
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let content =
            std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_path_buf(), e))?;
        serde_json::from_str(&content).map_err(|e| Error::Parse(path.to_path_buf(), e))
    }

    /// Extract the `pnpm.allowBuilds` object from the raw `package.json`
    /// payload, if present. Returns a map keyed by the raw pattern
    /// string (e.g. `"esbuild"`, `"@swc/core@1.3.0"`) with `bool`
    /// values preserved as `bool` and any other shape captured
    /// verbatim so the caller can warn about it.
    ///
    /// The key is held in `extra` rather than as a named field because
    /// it's pnpm-specific and nested under a `pnpm` object.
    pub fn pnpm_allow_builds(&self) -> BTreeMap<String, AllowBuildRaw> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        let Some(map) = pnpm.get("allowBuilds").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        map.iter()
            .map(|(k, v)| (k.clone(), AllowBuildRaw::from_json(v)))
            .collect()
    }

    /// Extract `pnpm.onlyBuiltDependencies` as a flat list of package
    /// names allowed to run lifecycle scripts. This is pnpm's canonical
    /// allowlist key (used by nearly every real-world pnpm project) and
    /// coexists with `pnpm.allowBuilds` — both sources merge into the
    /// same `BuildPolicy`. Non-string entries are dropped silently to
    /// match pnpm's tolerance for malformed configs.
    pub fn pnpm_only_built_dependencies(&self) -> Vec<String> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        let Some(arr) = pnpm.get("onlyBuiltDependencies").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    }

    /// Extract `pnpm.neverBuiltDependencies` — pnpm's canonical denylist
    /// for lifecycle scripts. Entries override any allowlist match in
    /// `onlyBuiltDependencies` / `allowBuilds` since explicit denies
    /// always win in `BuildPolicy::decide`.
    pub fn pnpm_never_built_dependencies(&self) -> Vec<String> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return Vec::new();
        };
        let Some(arr) = pnpm
            .get("neverBuiltDependencies")
            .and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    }

    /// Extract `pnpm.catalog` — a default catalog defined inline in
    /// package.json under the `pnpm` object. pnpm itself reads catalogs
    /// only from `pnpm-workspace.yaml`, but aube also honors this
    /// location so single-package projects can declare catalogs without
    /// maintaining a separate workspace file.
    pub fn pnpm_catalog(&self) -> BTreeMap<String, String> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        let Some(map) = pnpm.get("catalog").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    }

    /// Extract `pnpm.catalogs` — named catalogs nested under the `pnpm`
    /// object. Pairs with [`pnpm_catalog`] for a fully-package.json-local
    /// catalog declaration.
    pub fn pnpm_catalogs(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        let Some(outer) = pnpm.get("catalogs").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        outer
            .iter()
            .filter_map(|(name, inner)| {
                let inner = inner.as_object()?;
                let entries: BTreeMap<String, String> = inner
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect();
                Some((name.clone(), entries))
            })
            .collect()
    }

    /// Extract `pnpm.ignoredOptionalDependencies` — a list of dep names
    /// that should be stripped from every manifest's `optionalDependencies`
    /// before resolution. Mirrors pnpm's read-package hook at
    /// `@pnpm/hooks.read-package-hook::createOptionalDependenciesRemover`.
    /// Non-string entries are ignored.
    pub fn pnpm_ignored_optional_dependencies(&self) -> BTreeSet<String> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return BTreeSet::new();
        };
        let Some(arr) = pnpm
            .get("ignoredOptionalDependencies")
            .and_then(|v| v.as_array())
        else {
            return BTreeSet::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    }

    /// Extract `pnpm.patchedDependencies` as a map of `name@version` ->
    /// patch file path (relative to the project root). Empty when the
    /// field is missing or malformed.
    pub fn pnpm_patched_dependencies(&self) -> BTreeMap<String, String> {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        let Some(map) = pnpm.get("patchedDependencies").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    }

    /// Return the set of dependency names marked
    /// `dependenciesMeta.<name>.injected = true`. When present, pnpm
    /// installs a hard copy of the resolved package (typically a
    /// workspace sibling) instead of a symlink, so the consumer sees
    /// the packed form — peer deps resolve against the consumer's
    /// tree rather than the source package's devDependencies. Aube's
    /// injection step reads this set after linking and rewrites each
    /// top-level symlink to point at a freshly materialized copy
    /// under `.aube/<name>@<version>+inject_<hash>/node_modules/<name>`.
    pub fn dependencies_meta_injected(&self) -> BTreeSet<String> {
        let Some(meta) = self
            .extra
            .get("dependenciesMeta")
            .and_then(|v| v.as_object())
        else {
            return BTreeSet::new();
        };
        meta.iter()
            .filter_map(|(k, v)| {
                let injected = v.get("injected").and_then(|b| b.as_bool()).unwrap_or(false);
                injected.then(|| k.clone())
            })
            .collect()
    }

    /// Return `pnpm.supportedArchitectures.{os,cpu,libc}` as three
    /// string arrays. Missing fields become empty vecs. Used by the
    /// resolver to widen the set of platforms considered installable
    /// for optional dependencies — e.g. resolving a lockfile for a
    /// different target than the host running `aube install`.
    pub fn pnpm_supported_architectures(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let Some(pnpm) = self.extra.get("pnpm").and_then(|v| v.as_object()) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let Some(sa) = pnpm
            .get("supportedArchitectures")
            .and_then(|v| v.as_object())
        else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let read = |k: &str| -> Vec<String> {
            sa.get(k)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        (read("os"), read("cpu"), read("libc"))
    }

    /// Collect dependency overrides from every supported source on the
    /// root manifest, merged in pnpm's precedence order: top-level
    /// `overrides` (npm/pnpm) wins, then `pnpm.overrides`, then yarn-style
    /// `resolutions`. Keys round-trip as their raw selector strings:
    /// bare name (`foo`), parent-chain (`parent>foo`), version-suffixed
    /// (`foo@<2`, `parent@1>foo`), and yarn wildcards (`**/foo`,
    /// `parent/foo`). Structural validation lives in
    /// `aube_resolver::override_rule`; this layer just filters out
    /// malformed keys and non-string values. Workspace-level overrides
    /// from `pnpm-workspace.yaml` are merged on top of this map by the
    /// caller.
    pub fn overrides_map(&self) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();

        // yarn `resolutions` (lowest priority)
        if let Some(obj) = self.extra.get("resolutions").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str()
                    && is_valid_selector_key(k)
                {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }

        // `pnpm.overrides`
        if let Some(obj) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("overrides"))
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                if let Some(s) = v.as_str()
                    && is_valid_selector_key(k)
                {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }

        // Top-level `overrides` (npm / pnpm) — highest priority of the three
        if let Some(obj) = self.extra.get("overrides").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(s) = v.as_str()
                    && is_valid_selector_key(k)
                {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }

        out
    }

    /// Extract `packageExtensions` from root package.json. Supports both
    /// top-level `packageExtensions` and `pnpm.packageExtensions`, with the
    /// top-level value taking precedence for duplicate selectors.
    pub fn package_extensions(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = BTreeMap::new();
        if let Some(obj) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("packageExtensions"))
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                out.insert(k.clone(), v.clone());
            }
        }
        if let Some(obj) = self
            .extra
            .get("packageExtensions")
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Extract package deprecation mute ranges. Supports top-level
    /// `allowedDeprecatedVersions` and `pnpm.allowedDeprecatedVersions`;
    /// non-string values are ignored.
    pub fn allowed_deprecated_versions(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(obj) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("allowedDeprecatedVersions"))
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        if let Some(obj) = self
            .extra
            .get("allowedDeprecatedVersions")
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }

    /// Extract `pnpm.peerDependencyRules.ignoreMissing` as a flat list of
    /// glob patterns. Non-string entries are dropped. Mirrors pnpm's
    /// `peerDependencyRules` escape hatch — patterns silence "missing
    /// required peer dependency" warnings when the peer name matches.
    pub fn pnpm_peer_dependency_rules_ignore_missing(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("ignoreMissing")
    }

    /// Extract `pnpm.peerDependencyRules.allowAny` as a flat list of
    /// glob patterns. Peers whose name matches a pattern have their
    /// semver check bypassed — any resolved version is accepted.
    pub fn pnpm_peer_dependency_rules_allow_any(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("allowAny")
    }

    /// Extract `pnpm.peerDependencyRules.allowedVersions` as a map of
    /// selector -> additional semver range. Selectors are either a bare
    /// peer name (e.g. `react`) meaning "applies to every consumer of
    /// this peer", or `parent>peer` (e.g. `styled-components>react`)
    /// meaning "only when declared by this parent". Values widen the
    /// declared peer range: a peer resolving inside *either* the
    /// declared range or this override is treated as satisfied.
    /// Non-string entries are ignored.
    pub fn pnpm_peer_dependency_rules_allowed_versions(&self) -> BTreeMap<String, String> {
        let Some(rules) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("peerDependencyRules"))
            .and_then(|v| v.as_object())
        else {
            return BTreeMap::new();
        };
        let Some(obj) = rules.get("allowedVersions").and_then(|v| v.as_object()) else {
            return BTreeMap::new();
        };
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    }

    fn pnpm_peer_dependency_rules_string_list(&self, field: &str) -> Vec<String> {
        let Some(rules) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("peerDependencyRules"))
            .and_then(|v| v.as_object())
        else {
            return Vec::new();
        };
        let Some(arr) = rules.get(field).and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    }

    /// Extract `updateConfig.ignoreDependencies` from package.json and
    /// `pnpm.updateConfig.ignoreDependencies`, merged with top-level
    /// `updateConfig` taking precedence by appending last.
    pub fn update_ignore_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(arr) = self
            .extra
            .get("pnpm")
            .and_then(|v| v.as_object())
            .and_then(|p| p.get("updateConfig"))
            .and_then(|v| v.as_object())
            .and_then(|u| u.get("ignoreDependencies"))
            .and_then(|v| v.as_array())
        {
            out.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
        }
        if let Some(update_config) = &self.update_config {
            out.extend(update_config.ignore_dependencies.iter().cloned());
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn all_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .chain(self.dev_dependencies.iter())
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn production_dependencies(&self) -> impl Iterator<Item = (&str, &str)> {
        self.dependencies
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Raw value shape for a single `allowBuilds` entry, preserved as-is
/// from the source JSON/YAML. Interpretation (allow / deny / warn
/// about unsupported shape) lives in `aube-scripts::policy`, keeping
/// this crate purely about parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowBuildRaw {
    Bool(bool),
    Other(String),
}

impl AllowBuildRaw {
    fn from_json(v: &serde_json::Value) -> Self {
        match v {
            serde_json::Value::Bool(b) => Self::Bool(*b),
            other => Self::Other(other.to_string()),
        }
    }
}

/// Surface-level structural check on an override key. We accept any
/// non-empty key that isn't obviously a JSON typo — the resolver's
/// `override_rule` parser does the real work and silently drops keys
/// it can't interpret. Keeping the manifest filter loose means a pnpm
/// user with an unfamiliar-but-valid selector (e.g. `a@1>b@<2`)
/// reaches the resolver unchanged.
fn is_valid_selector_key(k: &str) -> bool {
    !k.is_empty()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    Parse(std::path::PathBuf, serde_json::Error),
    #[error("failed to parse {0}: {1}")]
    YamlParse(std::path::PathBuf, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> PackageJson {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn selector_key_filter_accepts_valid_forms() {
        assert!(is_valid_selector_key("lodash"));
        assert!(is_valid_selector_key("@babel/core"));
        assert!(is_valid_selector_key("foo>bar"));
        assert!(is_valid_selector_key("**/foo"));
        assert!(is_valid_selector_key("lodash@<4.17.21"));
        assert!(is_valid_selector_key("a@1>b@<2"));
    }

    #[test]
    fn selector_key_filter_rejects_empty() {
        assert!(!is_valid_selector_key(""));
    }

    #[test]
    fn overrides_map_collects_top_level() {
        let p = parse(r#"{"overrides": {"lodash": "4.17.21"}}"#);
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "4.17.21");
    }

    #[test]
    fn overrides_map_top_level_wins_over_pnpm_and_resolutions() {
        let p = parse(
            r#"{
                "resolutions": {"lodash": "1.0.0"},
                "pnpm": {"overrides": {"lodash": "2.0.0"}},
                "overrides": {"lodash": "3.0.0"}
            }"#,
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "3.0.0");
    }

    #[test]
    fn overrides_map_merges_disjoint_keys() {
        let p = parse(
            r#"{
                "resolutions": {"a": "1"},
                "pnpm": {"overrides": {"b": "2"}},
                "overrides": {"c": "3"}
            }"#,
        );
        let m = p.overrides_map();
        assert_eq!(m.get("a").unwrap(), "1");
        assert_eq!(m.get("b").unwrap(), "2");
        assert_eq!(m.get("c").unwrap(), "3");
    }

    #[test]
    fn overrides_map_preserves_advanced_selector_keys() {
        // Advanced selectors round-trip as raw keys; the resolver
        // parses them later.
        let p = parse(
            r#"{
                "overrides": {
                    "lodash": "4.17.21",
                    "foo>bar": "1.0.0",
                    "**/baz": "1.0.0",
                    "qux@<2": "1.0.0"
                }
            }"#,
        );
        let m = p.overrides_map();
        assert_eq!(m.len(), 4);
        assert!(m.contains_key("lodash"));
        assert!(m.contains_key("foo>bar"));
        assert!(m.contains_key("**/baz"));
        assert!(m.contains_key("qux@<2"));
    }

    #[test]
    fn overrides_map_supports_npm_alias_value() {
        let p = parse(r#"{"overrides": {"foo": "npm:bar@^2"}}"#);
        assert_eq!(p.overrides_map().get("foo").unwrap(), "npm:bar@^2");
    }

    #[test]
    fn package_extensions_top_level_wins_over_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"packageExtensions": {"foo": {"dependencies": {"a": "1"}}}},
                "packageExtensions": {"foo": {"dependencies": {"a": "2"}}}
            }"#,
        );
        assert_eq!(
            p.package_extensions()
                .get("foo")
                .and_then(|v| v.pointer("/dependencies/a"))
                .and_then(|v| v.as_str()),
            Some("2")
        );
    }

    #[test]
    fn update_ignore_dependencies_merges_top_level_and_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"updateConfig": {"ignoreDependencies": ["a"]}},
                "updateConfig": {"ignoreDependencies": ["b"]}
            }"#,
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["a", "b"]);
    }

    #[test]
    fn overrides_map_skips_object_values() {
        // npm allows nested override objects; we don't support those yet,
        // so they should be silently dropped rather than panicking.
        let p = parse(r#"{"overrides": {"foo": {"bar": "1.0.0"}}}"#);
        assert!(p.overrides_map().is_empty());
    }

    #[test]
    fn parses_bundled_dependencies_list() {
        let p = parse(r#"{"name":"x","bundledDependencies":["foo","bar"]}"#);
        let deps = BTreeMap::new();
        let names = p.bundled_dependencies.as_ref().unwrap().names(&deps);
        assert_eq!(names, vec!["foo", "bar"]);
    }

    #[test]
    fn accepts_legacy_bundle_dependencies_alias() {
        let p = parse(r#"{"name":"x","bundleDependencies":["foo"]}"#);
        assert!(matches!(
            p.bundled_dependencies,
            Some(BundledDependencies::List(_))
        ));
    }

    #[test]
    fn bundle_true_means_all_production_deps() {
        let p =
            parse(r#"{"name":"x","dependencies":{"a":"1","b":"2"},"bundledDependencies":true}"#);
        let names = p
            .bundled_dependencies
            .as_ref()
            .unwrap()
            .names(&p.dependencies);
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn peer_dependency_rules_accessors_read_nested_pnpm_block() {
        let p = parse(
            r#"{
                "name":"x",
                "pnpm": {
                    "peerDependencyRules": {
                        "ignoreMissing": ["react", "react-dom"],
                        "allowAny": ["@types/*"],
                        "allowedVersions": {
                            "react": "^18.0.0",
                            "styled-components>react": "^17.0.0",
                            "ignored": 42
                        }
                    }
                }
            }"#,
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-dom".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/*".to_string()],
        );
        let allowed = p.pnpm_peer_dependency_rules_allowed_versions();
        assert_eq!(allowed.get("react").map(String::as_str), Some("^18.0.0"));
        assert_eq!(
            allowed.get("styled-components>react").map(String::as_str),
            Some("^17.0.0"),
        );
        assert!(!allowed.contains_key("ignored"));
    }

    #[test]
    fn peer_dependency_rules_accessors_empty_when_missing() {
        let p = parse(r#"{"name":"x"}"#);
        assert!(p.pnpm_peer_dependency_rules_ignore_missing().is_empty());
        assert!(p.pnpm_peer_dependency_rules_allow_any().is_empty());
        assert!(p.pnpm_peer_dependency_rules_allowed_versions().is_empty());
    }
}
