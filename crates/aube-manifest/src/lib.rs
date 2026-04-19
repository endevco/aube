pub mod workspace;

pub use workspace::WorkspaceConfig;

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Deserialize `engines` tolerant to the pre-npm-2.x legacy array
/// form, e.g. `extsprintf@1.4.1` ships `"engines": ["node >=0.6.0"]`.
/// Modern npm ignores that shape (engine-strict only consults the map
/// form), so normalize to an empty map rather than failing the whole
/// manifest — a hard error there takes down every install that touches
/// one of these ancient packages, even when the user's target engine
/// wouldn't have matched any constraint anyway.
///
/// An explicit `null` is also tolerated (same as "field absent"),
/// matching the tolerance our other dep-map parsers apply.
fn engines_tolerant<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value: Option<serde_json::Value> = Option::deserialize(de)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::Array(_)) => BTreeMap::new(),
        Some(serde_json::Value::Object(m)) => m
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "engines: expected a map, got {}",
                match other {
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "boolean",
                    _ => "non-object",
                }
            )));
        }
    })
}

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
    /// `engine-strict`. See `engines_tolerant` for the legacy-shape
    /// handling.
    #[serde(
        default,
        deserialize_with = "engines_tolerant",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
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

    /// Iterate over the `pnpm` and `aube` config objects in
    /// `package.json`, yielding whichever are present in precedence
    /// order (pnpm first, aube last). Callers that merge into a map
    /// with later-wins semantics get `aube.*` overriding `pnpm.*` on
    /// key conflict; callers that union lists get both sources
    /// included. Aube mirrors every `pnpm.*` config key under an
    /// `aube.*` alias so projects can declare aube-native config
    /// without piggy-backing on the pnpm namespace.
    fn pnpm_aube_objects(
        &self,
    ) -> impl Iterator<Item = &serde_json::Map<String, serde_json::Value>> {
        ["pnpm", "aube"]
            .into_iter()
            .filter_map(|k| self.extra.get(k).and_then(|v| v.as_object()))
    }

    /// Extract the `pnpm.allowBuilds` / `aube.allowBuilds` object from
    /// the raw `package.json` payload, if present. Returns a map keyed
    /// by the raw pattern string (e.g. `"esbuild"`,
    /// `"@swc/core@1.3.0"`) with `bool` values preserved as `bool` and
    /// any other shape captured verbatim so the caller can warn about
    /// it. `aube.*` wins over `pnpm.*` on key conflict.
    ///
    /// The key is held in `extra` rather than as a named field because
    /// it's nested under a `pnpm`/`aube` object.
    pub fn pnpm_allow_builds(&self) -> BTreeMap<String, AllowBuildRaw> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("allowBuilds").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    out.insert(k.clone(), AllowBuildRaw::from_json(v));
                }
            }
        }
        out
    }

    /// Extract `pnpm.onlyBuiltDependencies` / `aube.onlyBuiltDependencies`
    /// as a flat list of package names allowed to run lifecycle
    /// scripts. This is pnpm's canonical allowlist key (used by nearly
    /// every real-world pnpm project) and coexists with `allowBuilds`
    /// — all sources merge into the same `BuildPolicy`. Non-string
    /// entries are dropped silently to match pnpm's tolerance for
    /// malformed configs. Entries from `aube.*` are appended after
    /// `pnpm.*` and deduped while preserving insertion order.
    pub fn pnpm_only_built_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns.get("onlyBuiltDependencies").and_then(|v| v.as_array()) {
                push_unique_strs(&mut out, arr);
            }
        }
        out
    }

    /// Extract `pnpm.neverBuiltDependencies` /
    /// `aube.neverBuiltDependencies` — the canonical denylist for
    /// lifecycle scripts. Entries override any allowlist match in
    /// `onlyBuiltDependencies` / `allowBuilds` since explicit denies
    /// always win in `BuildPolicy::decide`. Entries union across both
    /// namespaces with insertion order preserved.
    pub fn pnpm_never_built_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns.get("neverBuiltDependencies").and_then(|v| v.as_array()) {
                push_unique_strs(&mut out, arr);
            }
        }
        out
    }

    /// Extract `pnpm.catalog` / `aube.catalog` — a default catalog
    /// defined inline in package.json under the `pnpm`/`aube` object.
    /// pnpm itself reads catalogs only from `pnpm-workspace.yaml`, but
    /// aube also honors this location so single-package projects can
    /// declare catalogs without maintaining a separate workspace
    /// file. `aube.catalog` wins over `pnpm.catalog` on key conflict.
    pub fn pnpm_catalog(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("catalog").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        out
    }

    /// Extract `pnpm.catalogs` / `aube.catalogs` — named catalogs
    /// nested under the `pnpm`/`aube` object. Pairs with
    /// [`pnpm_catalog`] for a fully-package.json-local catalog
    /// declaration. Named catalogs merge per-key across namespaces
    /// (same rule as `pnpm_catalog`): `aube.catalogs.<name>.<pkg>`
    /// wins over `pnpm.catalogs.<name>.<pkg>`, while entries declared
    /// only on one side are preserved.
    pub fn pnpm_catalogs(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(outer) = ns.get("catalogs").and_then(|v| v.as_object()) {
                for (name, inner) in outer {
                    let Some(inner) = inner.as_object() else {
                        continue;
                    };
                    let catalog = out.entry(name.clone()).or_default();
                    for (k, v) in inner {
                        if let Some(s) = v.as_str() {
                            catalog.insert(k.clone(), s.to_string());
                        }
                    }
                }
            }
        }
        out
    }

    /// Extract `pnpm.ignoredOptionalDependencies` /
    /// `aube.ignoredOptionalDependencies` — a list of dep names that
    /// should be stripped from every manifest's `optionalDependencies`
    /// before resolution. Mirrors pnpm's read-package hook at
    /// `@pnpm/hooks.read-package-hook::createOptionalDependenciesRemover`.
    /// Non-string entries are ignored. Entries from both namespaces
    /// union into the returned set.
    pub fn pnpm_ignored_optional_dependencies(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns
                .get("ignoredOptionalDependencies")
                .and_then(|v| v.as_array())
            {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
        }
        out
    }

    /// Extract `pnpm.patchedDependencies` / `aube.patchedDependencies`
    /// as a map of `name@version` -> patch file path (relative to the
    /// project root). Empty when the field is missing or malformed.
    /// `aube.*` wins over `pnpm.*` on key conflict.
    pub fn pnpm_patched_dependencies(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(map) = ns.get("patchedDependencies").and_then(|v| v.as_object()) {
                for (k, v) in map {
                    if let Some(s) = v.as_str() {
                        out.insert(k.clone(), s.to_string());
                    }
                }
            }
        }
        out
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

    /// Return `{pnpm,aube}.supportedArchitectures.{os,cpu,libc}` as
    /// three string arrays. Missing fields become empty vecs. Used by
    /// the resolver to widen the set of platforms considered
    /// installable for optional dependencies — e.g. resolving a
    /// lockfile for a different target than the host running `aube
    /// install`. Entries from `aube.*` are appended after `pnpm.*` and
    /// deduped while preserving insertion order.
    pub fn pnpm_supported_architectures(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let mut os = Vec::new();
        let mut cpu = Vec::new();
        let mut libc = Vec::new();
        for ns in self.pnpm_aube_objects() {
            let Some(sa) = ns.get("supportedArchitectures").and_then(|v| v.as_object()) else {
                continue;
            };
            if let Some(arr) = sa.get("os").and_then(|v| v.as_array()) {
                push_unique_strs(&mut os, arr);
            }
            if let Some(arr) = sa.get("cpu").and_then(|v| v.as_array()) {
                push_unique_strs(&mut cpu, arr);
            }
            if let Some(arr) = sa.get("libc").and_then(|v| v.as_array()) {
                push_unique_strs(&mut libc, arr);
            }
        }
        (os, cpu, libc)
    }

    /// Collect dependency overrides from every supported source on the
    /// root manifest, merged in precedence order: yarn-style
    /// `resolutions` (lowest), then `pnpm.overrides`, then
    /// `aube.overrides`, then top-level `overrides` (highest). Keys
    /// round-trip as their raw selector strings: bare name (`foo`),
    /// parent-chain (`parent>foo`), version-suffixed (`foo@<2`,
    /// `parent@1>foo`), and yarn wildcards (`**/foo`, `parent/foo`).
    /// Structural validation lives in `aube_resolver::override_rule`;
    /// this layer just filters out malformed keys and non-string
    /// values. Workspace-level overrides from `pnpm-workspace.yaml`
    /// are merged on top of this map by the caller.
    pub fn overrides_map(&self) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        let insert = |out: &mut BTreeMap<String, String>,
                      obj: &serde_json::Map<String, serde_json::Value>| {
            for (k, v) in obj {
                if let Some(s) = v.as_str()
                    && is_valid_selector_key(k)
                {
                    out.insert(k.clone(), s.to_string());
                }
            }
        };

        // yarn `resolutions` (lowest priority)
        if let Some(obj) = self.extra.get("resolutions").and_then(|v| v.as_object()) {
            insert(&mut out, obj);
        }

        // `pnpm.overrides` then `aube.overrides` (later wins)
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns.get("overrides").and_then(|v| v.as_object()) {
                insert(&mut out, obj);
            }
        }

        // Top-level `overrides` (npm / pnpm) — highest priority
        if let Some(obj) = self.extra.get("overrides").and_then(|v| v.as_object()) {
            insert(&mut out, obj);
        }

        out
    }

    /// Extract `packageExtensions` from root package.json. Supports
    /// top-level `packageExtensions`, `pnpm.packageExtensions`, and
    /// `aube.packageExtensions`. Precedence (low → high):
    /// `pnpm.packageExtensions`, `aube.packageExtensions`, top-level
    /// `packageExtensions` — later writes win for duplicate selectors.
    pub fn package_extensions(&self) -> BTreeMap<String, serde_json::Value> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns.get("packageExtensions").and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    out.insert(k.clone(), v.clone());
                }
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
    /// `allowedDeprecatedVersions`, `pnpm.allowedDeprecatedVersions`,
    /// and `aube.allowedDeprecatedVersions`; later sources win for
    /// duplicate keys. Non-string values are ignored.
    pub fn allowed_deprecated_versions(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        let insert = |out: &mut BTreeMap<String, String>,
                      obj: &serde_json::Map<String, serde_json::Value>| {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        };
        for ns in self.pnpm_aube_objects() {
            if let Some(obj) = ns
                .get("allowedDeprecatedVersions")
                .and_then(|v| v.as_object())
            {
                insert(&mut out, obj);
            }
        }
        if let Some(obj) = self
            .extra
            .get("allowedDeprecatedVersions")
            .and_then(|v| v.as_object())
        {
            insert(&mut out, obj);
        }
        out
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.ignoreMissing` as a
    /// flat list of glob patterns. Non-string entries are dropped.
    /// Mirrors pnpm's `peerDependencyRules` escape hatch — patterns
    /// silence "missing required peer dependency" warnings when the
    /// peer name matches. Entries from both namespaces union in the
    /// returned list.
    pub fn pnpm_peer_dependency_rules_ignore_missing(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("ignoreMissing")
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.allowAny` as a flat
    /// list of glob patterns. Peers whose name matches a pattern have
    /// their semver check bypassed — any resolved version is accepted.
    pub fn pnpm_peer_dependency_rules_allow_any(&self) -> Vec<String> {
        self.pnpm_peer_dependency_rules_string_list("allowAny")
    }

    /// Extract `{pnpm,aube}.peerDependencyRules.allowedVersions` as a
    /// map of selector -> additional semver range. Selectors are
    /// either a bare peer name (e.g. `react`) meaning "applies to
    /// every consumer of this peer", or `parent>peer` (e.g.
    /// `styled-components>react`) meaning "only when declared by this
    /// parent". Values widen the declared peer range: a peer resolving
    /// inside *either* the declared range or this override is treated
    /// as satisfied. Non-string entries are ignored. `aube.*` wins
    /// over `pnpm.*` on key conflict.
    pub fn pnpm_peer_dependency_rules_allowed_versions(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ns in self.pnpm_aube_objects() {
            let Some(rules) = ns.get("peerDependencyRules").and_then(|v| v.as_object()) else {
                continue;
            };
            let Some(obj) = rules.get("allowedVersions").and_then(|v| v.as_object()) else {
                continue;
            };
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    out.insert(k.clone(), s.to_string());
                }
            }
        }
        out
    }

    fn pnpm_peer_dependency_rules_string_list(&self, field: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            let Some(rules) = ns.get("peerDependencyRules").and_then(|v| v.as_object()) else {
                continue;
            };
            let Some(arr) = rules.get(field).and_then(|v| v.as_array()) else {
                continue;
            };
            push_unique_strs(&mut out, arr);
        }
        out
    }

    /// Extract `updateConfig.ignoreDependencies` from package.json
    /// across all supported locations: top-level `updateConfig`,
    /// `pnpm.updateConfig.ignoreDependencies`, and
    /// `aube.updateConfig.ignoreDependencies`. All entries are merged
    /// and deduped.
    pub fn update_ignore_dependencies(&self) -> Vec<String> {
        let mut out = Vec::new();
        for ns in self.pnpm_aube_objects() {
            if let Some(arr) = ns
                .get("updateConfig")
                .and_then(|v| v.as_object())
                .and_then(|u| u.get("ignoreDependencies"))
                .and_then(|v| v.as_array())
            {
                out.extend(arr.iter().filter_map(|v| v.as_str().map(String::from)));
            }
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

/// Append the string entries of `arr` to `dst`, skipping duplicates
/// already present and dropping non-string values. Preserves the
/// insertion order of first appearance — callers rely on this to keep
/// `pnpm.*` entries ahead of `aube.*` entries when both namespaces
/// contribute to the same list.
fn push_unique_strs(dst: &mut Vec<String>, arr: &[serde_json::Value]) {
    for v in arr {
        if let Some(s) = v.as_str()
            && !dst.iter().any(|existing| existing == s)
        {
            dst.push(s.to_string());
        }
    }
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

    /// Pre-npm-2.x publishes (e.g. `extsprintf@1.4.1`, `coffee-script@1.3.3`)
    /// ship `"engines": ["node >=0.6.0"]` as an array rather than a map.
    /// Modern npm ignores the legacy shape; we do the same rather than
    /// fail the whole manifest and take down every install that touches
    /// one of these ancient packages.
    #[test]
    fn engines_legacy_array_form_parses_as_empty_map() {
        let p = parse(r#"{"name":"x","engines":["node >=0.6.0"]}"#);
        assert!(p.engines.is_empty());
    }

    #[test]
    fn engines_null_is_treated_as_empty() {
        let p = parse(r#"{"name":"x","engines":null}"#);
        assert!(p.engines.is_empty());
    }

    #[test]
    fn engines_modern_map_form_still_parses() {
        let p = parse(r#"{"name":"x","engines":{"node":">=18.0.0","npm":">=9"}}"#);
        assert_eq!(p.engines.get("node").unwrap(), ">=18.0.0");
        assert_eq!(p.engines.get("npm").unwrap(), ">=9");
    }

    #[test]
    fn engines_missing_field_is_empty() {
        let p = parse(r#"{"name":"x"}"#);
        assert!(p.engines.is_empty());
    }

    #[test]
    fn engines_map_drops_non_string_values() {
        // Stay consistent with how our dep-map parsers treat redacted
        // / non-string entries — drop, not fail.
        let p = parse(r#"{"name":"x","engines":{"node":">=18","weird":null,"n":42}}"#);
        assert_eq!(p.engines.get("node").unwrap(), ">=18");
        assert!(!p.engines.contains_key("weird"));
        assert!(!p.engines.contains_key("n"));
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

    // --- aube.* namespace parity --------------------------------------

    #[test]
    fn aube_namespace_read_when_pnpm_missing() {
        let p = parse(
            r#"{
                "aube": {
                    "onlyBuiltDependencies": ["esbuild"],
                    "neverBuiltDependencies": ["sharp"],
                    "ignoredOptionalDependencies": ["fsevents"],
                    "patchedDependencies": {"lodash@4.17.21": "patches/lodash.patch"},
                    "catalog": {"react": "^18.0.0"},
                    "catalogs": {"legacy": {"react": "^17.0.0"}},
                    "supportedArchitectures": {"os": ["linux", "win32"], "cpu": ["x64"]},
                    "overrides": {"lodash": "4.17.21"},
                    "packageExtensions": {"foo": {"dependencies": {"a": "1"}}},
                    "allowedDeprecatedVersions": {"request": "*"},
                    "peerDependencyRules": {
                        "ignoreMissing": ["react-native"],
                        "allowAny": ["@types/*"],
                        "allowedVersions": {"react": "^18.0.0"}
                    },
                    "updateConfig": {"ignoreDependencies": ["typescript"]},
                    "allowBuilds": {"esbuild": true}
                }
            }"#,
        );
        assert_eq!(p.pnpm_only_built_dependencies(), vec!["esbuild"]);
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["sharp"]);
        assert!(p.pnpm_ignored_optional_dependencies().contains("fsevents"));
        assert_eq!(
            p.pnpm_patched_dependencies().get("lodash@4.17.21").unwrap(),
            "patches/lodash.patch",
        );
        assert_eq!(p.pnpm_catalog().get("react").unwrap(), "^18.0.0");
        assert_eq!(
            p.pnpm_catalogs()
                .get("legacy")
                .and_then(|c| c.get("react"))
                .unwrap(),
            "^17.0.0",
        );
        let (os, cpu, libc) = p.pnpm_supported_architectures();
        assert_eq!(os, vec!["linux", "win32"]);
        assert_eq!(cpu, vec!["x64"]);
        assert!(libc.is_empty());
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "4.17.21");
        assert!(p.package_extensions().contains_key("foo"));
        assert_eq!(p.allowed_deprecated_versions().get("request").unwrap(), "*",);
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/*".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allowed_versions()
                .get("react")
                .unwrap(),
            "^18.0.0",
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["typescript"]);
        assert!(matches!(
            p.pnpm_allow_builds().get("esbuild"),
            Some(AllowBuildRaw::Bool(true)),
        ));
    }

    #[test]
    fn aube_overrides_pnpm_on_key_conflict() {
        // For map-valued configs, `aube.*` wins on key conflict while
        // disjoint keys from either namespace merge.
        let p = parse(
            r#"{
                "pnpm": {
                    "catalog": {"react": "^17.0.0", "lodash": "^4.0.0"},
                    "patchedDependencies": {"foo@1": "pnpm.patch"},
                    "allowedDeprecatedVersions": {"request": "^2.0.0"},
                    "overrides": {"lodash": "pnpm-value"}
                },
                "aube": {
                    "catalog": {"react": "^18.0.0"},
                    "patchedDependencies": {"foo@1": "aube.patch"},
                    "allowedDeprecatedVersions": {"request": "^3.0.0"},
                    "overrides": {"lodash": "aube-value"}
                }
            }"#,
        );
        let catalog = p.pnpm_catalog();
        assert_eq!(catalog.get("react").unwrap(), "^18.0.0");
        assert_eq!(catalog.get("lodash").unwrap(), "^4.0.0");
        assert_eq!(
            p.pnpm_patched_dependencies().get("foo@1").unwrap(),
            "aube.patch",
        );
        assert_eq!(
            p.allowed_deprecated_versions().get("request").unwrap(),
            "^3.0.0",
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "aube-value");
    }

    #[test]
    fn top_level_overrides_still_beat_aube_namespace() {
        // Top-level `overrides` is the npm-standard surface and
        // remains the highest-priority source.
        let p = parse(
            r#"{
                "pnpm": {"overrides": {"lodash": "1"}},
                "aube": {"overrides": {"lodash": "2"}},
                "overrides": {"lodash": "3"}
            }"#,
        );
        assert_eq!(p.overrides_map().get("lodash").unwrap(), "3");
    }

    #[test]
    fn aube_supported_architectures_merges_with_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {"supportedArchitectures": {"os": ["linux"], "cpu": ["x64"]}},
                "aube": {"supportedArchitectures": {"os": ["win32"], "libc": ["glibc"]}}
            }"#,
        );
        let (os, cpu, libc) = p.pnpm_supported_architectures();
        assert_eq!(os, vec!["linux", "win32"]);
        assert_eq!(cpu, vec!["x64"]);
        assert_eq!(libc, vec!["glibc"]);
    }

    #[test]
    fn aube_list_configs_union_with_pnpm() {
        let p = parse(
            r#"{
                "pnpm": {
                    "onlyBuiltDependencies": ["esbuild"],
                    "neverBuiltDependencies": ["sharp"],
                    "ignoredOptionalDependencies": ["fsevents"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react"],
                        "allowAny": ["@types/a"]
                    }
                },
                "aube": {
                    "onlyBuiltDependencies": ["swc"],
                    "neverBuiltDependencies": ["node-gyp"],
                    "ignoredOptionalDependencies": ["dtrace-provider"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react-native"],
                        "allowAny": ["@types/b"]
                    }
                }
            }"#,
        );
        assert_eq!(p.pnpm_only_built_dependencies(), vec!["esbuild", "swc"]);
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["sharp", "node-gyp"]);
        let ignored = p.pnpm_ignored_optional_dependencies();
        assert!(ignored.contains("fsevents"));
        assert!(ignored.contains("dtrace-provider"));
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/a".to_string(), "@types/b".to_string()],
        );
    }

    #[test]
    fn aube_catalogs_merge_per_key_within_named_catalog() {
        // Same semantics as `pnpm_catalog`: aube wins per-key, and
        // entries only declared on one side are preserved instead of
        // being dropped when the catalog name exists on both sides.
        let p = parse(
            r#"{
                "pnpm": {
                    "catalogs": {
                        "default": {"react": "^17.0.0", "lodash": "^4.0.0"},
                        "legacy": {"webpack": "^4.0.0"}
                    }
                },
                "aube": {
                    "catalogs": {
                        "default": {"react": "^18.0.0", "vite": "^5.0.0"}
                    }
                }
            }"#,
        );
        let cats = p.pnpm_catalogs();
        let default = cats.get("default").expect("default catalog present");
        assert_eq!(default.get("react").unwrap(), "^18.0.0");
        assert_eq!(default.get("lodash").unwrap(), "^4.0.0");
        assert_eq!(default.get("vite").unwrap(), "^5.0.0");
        let legacy = cats.get("legacy").expect("legacy catalog preserved");
        assert_eq!(legacy.get("webpack").unwrap(), "^4.0.0");
    }

    #[test]
    fn aube_list_configs_dedupe_duplicates_across_namespaces() {
        // Union semantics imply dedup: a name listed in both
        // namespaces appears once, with first-seen ordering preserved.
        let p = parse(
            r#"{
                "pnpm": {
                    "onlyBuiltDependencies": ["esbuild", "sharp"],
                    "neverBuiltDependencies": ["evil"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react"],
                        "allowAny": ["@types/a"]
                    }
                },
                "aube": {
                    "onlyBuiltDependencies": ["esbuild", "swc"],
                    "neverBuiltDependencies": ["evil", "node-gyp"],
                    "peerDependencyRules": {
                        "ignoreMissing": ["react", "react-native"],
                        "allowAny": ["@types/a", "@types/b"]
                    }
                }
            }"#,
        );
        assert_eq!(
            p.pnpm_only_built_dependencies(),
            vec!["esbuild", "sharp", "swc"],
        );
        assert_eq!(p.pnpm_never_built_dependencies(), vec!["evil", "node-gyp"]);
        assert_eq!(
            p.pnpm_peer_dependency_rules_ignore_missing(),
            vec!["react".to_string(), "react-native".to_string()],
        );
        assert_eq!(
            p.pnpm_peer_dependency_rules_allow_any(),
            vec!["@types/a".to_string(), "@types/b".to_string()],
        );
    }

    #[test]
    fn aube_update_config_merges_with_pnpm_and_top_level() {
        let p = parse(
            r#"{
                "pnpm": {"updateConfig": {"ignoreDependencies": ["a"]}},
                "aube": {"updateConfig": {"ignoreDependencies": ["b"]}},
                "updateConfig": {"ignoreDependencies": ["c"]}
            }"#,
        );
        assert_eq!(p.update_ignore_dependencies(), vec!["a", "b", "c"]);
    }
}
