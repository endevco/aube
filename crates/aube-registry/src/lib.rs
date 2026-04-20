use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// npm allows the `os`, `cpu`, and `libc` fields on a package version
/// to be either a single string (e.g. `"libc": "glibc"`) or an array
/// of strings (e.g. `"libc": ["glibc"]`). An explicit `null` is also
/// treated as "no constraint", same as the field being absent — some
/// packuments emit it. Napi-rs additionally publishes `"libc": [null]`
/// on its Windows/macOS native-binding packages (e.g.
/// `@oxc-parser/binding-win32-x64-msvc`), meaning "no libc constraint";
/// drop null array entries so that shape round-trips to an empty vec
/// instead of failing the whole packument parse. Normalize all shapes
/// to a `Vec<String>` so the platform filter doesn't have to care.
fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrSeq {
        String(String),
        Seq(Vec<serde_json::Value>),
    }
    Ok(match Option::<StringOrSeq>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(StringOrSeq::String(s)) => vec![s],
        Some(StringOrSeq::Seq(v)) => v
            .into_iter()
            .filter_map(|e| match e {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
    })
}

/// Deserialize a `BTreeMap<String, String>` tolerant to any non-string
/// value — both at the whole-map level (`"dist-tags": null` → empty
/// map) and at the value level (`{"latest": null}` or
/// `{"vows": {"version": "0.6.4", ...}}` → entry dropped).
///
/// Two real-world sources of non-string values:
///
/// 1. Registry proxies (notably JFrog Artifactory's npm remote) emit
///    `null` in places where npmjs.org always emits a string: stripped
///    / tombstoned `dist-tags` values, per-version `time` entries for
///    deleted versions, or dep-map entries that were redacted by a
///    mirroring filter.
/// 2. Ancient publishes — some packages from the 2012–2013 era
///    (`deep-diff@0.1.0`, for example) have `devDependencies` entries
///    shaped like `{"version": "0.6.4", "dependencies": {...}}`
///    instead of a plain version string, because an old npm client
///    serialized a resolved tree into the manifest.
///
/// A strict `BTreeMap<String, String>` shape would fail these with
/// `invalid type: ..., expected a string`, blocking an install of any
/// package whose packument merely *lists* an affected version — even
/// when the user's range doesn't select it. Drop the unparseable
/// entries so the resolver sees the same shape npmjs would have served
/// for a modern publish. pnpm and bun behave the same way.
fn non_string_tolerant_map<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    let maybe: Option<BTreeMap<String, serde_json::Value>> = Option::deserialize(de)?;
    Ok(maybe
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| match v {
            serde_json::Value::String(s) => Some((k, s)),
            _ => None,
        })
        .collect())
}

pub mod client;
pub mod config;
pub mod jsr;

// Packuments and `package.json` files share the `bundledDependencies`
// shape, so the registry crate borrows the type from `aube-manifest`
// rather than defining its own copy. Re-exported for resolver callers
// that already import this crate.
pub use aube_manifest::BundledDependencies;

/// Controls whether the registry client is allowed to hit the network.
///
/// Mirrors pnpm's `--offline` / `--prefer-offline`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkMode {
    /// Normal behavior: honor the packument TTL, revalidate with the
    /// registry when the cache is stale, fetch tarballs over the network.
    #[default]
    Online,
    /// Use the packument cache regardless of age; only hit the network on a
    /// cache miss. Tarballs fall back to the network when the store doesn't
    /// already have them.
    PreferOffline,
    /// Never hit the network. Packument and tarball fetches fail with
    /// `Error::Offline` if the requested data isn't already on disk.
    Offline,
}

/// A packument (package document) from the npm registry.
/// This is the metadata for all versions of a package.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Packument {
    pub name: String,
    #[serde(default)]
    pub versions: BTreeMap<String, VersionMetadata>,
    #[serde(
        rename = "dist-tags",
        default,
        deserialize_with = "non_string_tolerant_map"
    )]
    pub dist_tags: BTreeMap<String, String>,
    /// Per-version publish timestamps (ISO-8601). Populated
    /// opportunistically: npmjs.org's corgi (abbreviated) packument
    /// omits `time`, but Verdaccio v5.15.1+ includes it in corgi, and
    /// the full-packument path used for `--resolution-mode=time-based`
    /// and `minimumReleaseAge` always carries it. When present, the
    /// resolver round-trips it into the lockfile's top-level `time:`
    /// block — matching pnpm's `publishedAt` wiring — and, in
    /// time-based mode, uses it to derive the publish-date cutoff.
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub time: BTreeMap<String, String>,
}

/// Metadata for a specific version of a package.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadata {
    pub name: String,
    pub version: String,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub peer_dependencies_meta: BTreeMap<String, PeerDepMeta>,
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub optional_dependencies: BTreeMap<String, String>,
    /// `bundledDependencies` from the packument. Either a list of dep
    /// names or `true` (meaning "bundle every `dependencies` entry").
    /// Packages listed here are shipped inside the parent tarball, so
    /// the resolver must not recurse into them. npm serializes this
    /// under both `bundledDependencies` and `bundleDependencies`; we
    /// accept either via alias.
    #[serde(default, alias = "bundleDependencies")]
    pub bundled_dependencies: Option<BundledDependencies>,
    pub dist: Option<Dist>,
    #[serde(default, deserialize_with = "string_or_seq")]
    pub os: Vec<String>,
    #[serde(default, deserialize_with = "string_or_seq")]
    pub cpu: Vec<String>,
    #[serde(default, deserialize_with = "string_or_seq")]
    pub libc: Vec<String>,
    /// `engines:` from the package manifest (e.g. `{node: ">=8"}`).
    /// Round-tripped into the lockfile so pnpm-compatible output can
    /// emit `engines: {node: '>=8'}` on package entries without a
    /// packument re-fetch.
    ///
    /// Uses `aube_manifest::engines_tolerant` rather than
    /// `non_string_tolerant_map` so the packument parser tolerates the
    /// pre-npm-2.x legacy array shape (`"engines": ["node >= 0.4.0"]`,
    /// still present on `html-entities@1.0.0` and other ancient
    /// publishes). A strict map-only deserializer there aborts the
    /// whole resolve for any transitive dep whose packument merely
    /// *lists* an affected version — even when the user's range
    /// wouldn't have selected it.
    #[serde(default, deserialize_with = "aube_manifest::engines_tolerant")]
    pub engines: BTreeMap<String, String>,
    /// `license:` field from the package manifest. npm's lockfile
    /// keeps this per-package; other formats don't. Stored as
    /// `Option<String>` because packuments can emit a bare string
    /// (`"MIT"`), an SPDX object, or nothing at all — we only keep
    /// the simple case for lockfile round-trip. Non-string shapes
    /// degrade to `None` rather than failing to parse the packument.
    #[serde(default, deserialize_with = "license_string")]
    pub license: Option<String>,
    /// `funding:` URL extracted from the manifest's `funding` field.
    /// The field is documented as a string *or* an object with a
    /// `url:` key *or* an array of either — npm's lockfile
    /// normalizes to `{url: …}`, so we only keep the URL and let
    /// the writer emit the wrapping object. Serde `rename` because
    /// `rename_all = "camelCase"` would otherwise look for
    /// `fundingUrl` in the JSON.
    #[serde(default, rename = "funding", deserialize_with = "funding_url")]
    pub funding_url: Option<String>,
    /// `bin:` map from the packument, normalized to `name → path`.
    ///
    /// npm records `bin` in two shapes on a manifest: a string
    /// (`"bin": "cli.js"` — implicitly named after the package) or a
    /// map (`"bin": {"foo": "cli.js"}` — explicitly named). We
    /// normalize to the map form at parse time so downstream callers
    /// don't have to branch: an empty map means "no bins".
    ///
    /// pnpm collapses this to `hasBin: true` on its package entries;
    /// bun preserves the full map on its per-package meta. Keeping
    /// the map lets us feed both writers without an extra
    /// tarball-level re-parse.
    #[serde(default, rename = "bin", deserialize_with = "bin_map")]
    pub bin: BTreeMap<String, String>,
    #[serde(default)]
    pub has_install_script: bool,
    /// Deprecation message from the registry, if this version is deprecated.
    #[serde(default, deserialize_with = "deprecated_string")]
    pub deprecated: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct PeerDepMeta {
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Dist {
    pub tarball: String,
    pub integrity: Option<String>,
    pub shasum: Option<String>,
}

fn deprecated_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    })
}

/// Accept the packument's `license:` field in any of its documented
/// shapes (string, `{type, url}` object, or missing) and collapse to
/// the simple string form npm emits in its lockfile. Non-string
/// shapes degrade to `None`; we don't try to normalize SPDX
/// expressions or license-file references here.
fn license_string<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Object(m)) => m
            .get("type")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        _ => None,
    })
}

/// Extract the first `url:` out of a packument's `funding:` field.
/// The field may be a URL string, a `{url: …}` object, or an array
/// of either — npm's lockfile normalizes to `{"url": "…"}` on each
/// package entry, so we only need the URL itself. Missing / empty
/// / non-url-bearing shapes degrade to `None`.
fn funding_url<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Object(m)) => m
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from),
        Some(serde_json::Value::Array(arr)) => arr.iter().find_map(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Object(m) => m
                .get("url")
                .and_then(|u| u.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            _ => None,
        }),
        _ => None,
    })
}

/// Normalize `package.json` `bin` into a `name → path` map.
///
/// Two canonical shapes on the npm registry: a string
/// (`"bin": "cli.js"` — implicitly keyed by the package name) and a
/// map (`"bin": {"foo": "cli.js"}`). Older or odd packuments also
/// surface `null` or an empty string; a missing `bin` field falls
/// through to the default empty map.
///
/// The string-form needs the package name to emit a well-formed map
/// — which we don't have here at deserialize time. We leave the key
/// as an empty string; every call site that cares about bin names
/// (`aube-linker`'s bin-symlink pass, the bun writer) already has
/// the package name in scope and can patch it up.
fn bin_map<'de, D>(de: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => BTreeMap::new(),
        // The implicit-name case — keep a single empty-keyed entry so
        // consumers can still detect presence and patch the name.
        Some(serde_json::Value::String(s)) if s.is_empty() => BTreeMap::new(),
        Some(serde_json::Value::String(s)) => {
            let mut m = BTreeMap::new();
            m.insert(String::new(), s);
            m
        }
        Some(serde_json::Value::Object(m)) => m
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some((k, s)),
                _ => None,
            })
            .collect(),
        Some(_) => BTreeMap::new(),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("package not found: {0}")]
    NotFound(String),
    #[error("version not found: {0}@{1}")]
    VersionNotFound(String, String),
    /// The registry rejected the request with 401/403 — either no auth
    /// token was configured, it was invalid, or the account doesn't
    /// have permission for this package. Callers should point the user
    /// at `aube login`.
    #[error("authentication required")]
    Unauthorized,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry rejected write: HTTP {status}: {body}")]
    RegistryWrite { status: u16, body: String },
    #[error("offline: {0} is not available in the local cache")]
    Offline(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> VersionMetadata {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn libc_accepts_string() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":"glibc"}"#);
        assert_eq!(v.libc, vec!["glibc"]);
    }

    #[test]
    fn libc_accepts_array() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":["glibc","musl"]}"#);
        assert_eq!(v.libc, vec!["glibc", "musl"]);
    }

    #[test]
    fn os_and_cpu_accept_string() {
        let v = parse(r#"{"name":"x","version":"1.0.0","os":"linux","cpu":"x64"}"#);
        assert_eq!(v.os, vec!["linux"]);
        assert_eq!(v.cpu, vec!["x64"]);
    }

    #[test]
    fn null_is_treated_as_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0","os":null,"cpu":null,"libc":null}"#);
        assert!(v.os.is_empty());
        assert!(v.cpu.is_empty());
        assert!(v.libc.is_empty());
    }

    /// Napi-rs emits `"libc": [null]` on Windows/macOS native-binding
    /// publishes (e.g. `@oxc-parser/binding-win32-x64-msvc`), meaning
    /// "no libc constraint". Drop the null entry so the packument
    /// parses — otherwise every version with that shape blocks resolve.
    #[test]
    fn libc_array_containing_null_drops_null() {
        let v = parse(r#"{"name":"x","version":"1.0.0","libc":[null]}"#);
        assert!(v.libc.is_empty());
    }

    #[test]
    fn os_cpu_libc_arrays_drop_non_string_entries() {
        let v = parse(
            r#"{
                "name":"x","version":"1.0.0",
                "os":["linux",null,42],
                "cpu":["x64",null],
                "libc":["glibc",{"x":1}]
            }"#,
        );
        assert_eq!(v.os, vec!["linux"]);
        assert_eq!(v.cpu, vec!["x64"]);
        assert_eq!(v.libc, vec!["glibc"]);
    }

    #[test]
    fn bin_normalizes_packument_shapes() {
        let missing = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(missing.bin.is_empty(), "missing bin → empty map");
        let empty_string = parse(r#"{"name":"x","version":"1.0.0","bin":""}"#);
        assert!(empty_string.bin.is_empty(), "empty string bin → empty map");
        let null_bin = parse(r#"{"name":"x","version":"1.0.0","bin":null}"#);
        assert!(null_bin.bin.is_empty(), "null bin → empty map");
        let empty_map = parse(r#"{"name":"x","version":"1.0.0","bin":{}}"#);
        assert!(empty_map.bin.is_empty(), "empty map bin → empty map");
        // String bin leaves the name blank — callers patch it with the
        // package name before materializing a symlink / writing to
        // bun.lock.
        let string_bin = parse(r#"{"name":"x","version":"1.0.0","bin":"cli.js"}"#);
        assert_eq!(string_bin.bin.get(""), Some(&"cli.js".to_string()));
        let map_bin = parse(r#"{"name":"x","version":"1.0.0","bin":{"foo":"cli.js"}}"#);
        assert_eq!(map_bin.bin.get("foo"), Some(&"cli.js".to_string()));
    }

    /// Round-trip the `bin` map through the on-disk cache format
    /// (serialize → parse). Regression: the disk cache round-trips
    /// the field under the name `bin`, so the deserializer *must*
    /// accept a map back (and not interpret the already-normalized
    /// map as an implicit-name string).
    #[test]
    fn bin_map_roundtrips_through_cache_serialization() {
        let mut bin = BTreeMap::new();
        bin.insert("semver".to_string(), "bin/semver.js".to_string());
        let v = VersionMetadata {
            name: "semver".to_string(),
            version: "7.7.4".to_string(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            bundled_dependencies: None,
            dist: None,
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            engines: BTreeMap::new(),
            license: None,
            funding_url: None,
            bin,
            has_install_script: false,
            deprecated: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VersionMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.bin.get("semver"),
            Some(&"bin/semver.js".to_string()),
            "bin map must round-trip through cache serialization"
        );
    }

    #[test]
    fn missing_fields_default_to_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(v.os.is_empty());
        assert!(v.cpu.is_empty());
        assert!(v.libc.is_empty());
    }

    #[test]
    fn deprecated_string_is_preserved_and_false_is_empty() {
        let v = parse(r#"{"name":"x","version":"1.0.0","deprecated":"use y"}"#);
        assert_eq!(v.deprecated.as_deref(), Some("use y"));

        let v = parse(r#"{"name":"x","version":"1.0.1","deprecated":false}"#);
        assert!(v.deprecated.is_none());
    }

    /// Artifactory's npm remote proxies sometimes emit `null` entries
    /// in dep maps where stripped/redacted deps used to be. The
    /// resolver must not bail on that — the null dep is semantically
    /// "not present", same shape npmjs would have served.
    #[test]
    fn dependency_maps_drop_null_entries() {
        let v = parse(
            r#"{
                "name": "x",
                "version": "1.0.0",
                "dependencies": {"kept": "^1", "stripped": null},
                "devDependencies": {"dkept": "^2", "dstripped": null},
                "peerDependencies": {"pkept": "^3", "pstripped": null},
                "optionalDependencies": {"okept": "^4", "ostripped": null}
            }"#,
        );
        assert_eq!(v.dependencies.len(), 1);
        assert_eq!(v.dependencies["kept"], "^1");
        assert_eq!(v.dev_dependencies.len(), 1);
        assert_eq!(v.peer_dependencies.len(), 1);
        assert_eq!(v.optional_dependencies.len(), 1);
    }

    /// Ancient publishes (e.g. `deep-diff@0.1.0`, published 2013) have
    /// dep-map entries where the value is an object
    /// (`{"version": "0.6.4", "dependencies": {...}}`) rather than a
    /// version string. That shape would fail a strict string-valued
    /// map — drop those entries, same as null ones, so the packument
    /// still parses and unaffected versions stay resolvable.
    #[test]
    fn dependency_maps_drop_object_valued_entries() {
        let v = parse(
            r#"{
                "name": "deep-diff",
                "version": "0.1.0",
                "devDependencies": {
                    "vows": {"version": "0.6.4", "dependencies": {"diff": {"version": "1.0.4"}}},
                    "extend": {"version": "1.1.1"},
                    "lodash": "0.9.2"
                }
            }"#,
        );
        assert_eq!(v.dev_dependencies.len(), 1);
        assert_eq!(v.dev_dependencies["lodash"], "0.9.2");
    }

    /// `html-entities@1.0.0` (and other pre-npm-2.x publishes) ship
    /// `"engines": ["node >= 0.4.0"]` — a legacy array shape. A strict
    /// map-only deserializer fails the whole packument with
    /// "invalid type: sequence, expected a map", aborting resolve for
    /// every dependent. Tolerate the array (same behavior as
    /// `aube-manifest::engines_tolerant`) by dropping it to an empty
    /// map, matching what modern npm does with that field.
    #[test]
    fn engines_tolerates_legacy_array_shape() {
        let v = parse(
            r#"{
                "name": "html-entities",
                "version": "1.0.0",
                "engines": ["node >= 0.4.0"]
            }"#,
        );
        assert!(v.engines.is_empty());
    }

    #[test]
    fn engines_map_is_preserved() {
        let v = parse(r#"{"name":"x","version":"1.0.0","engines":{"node":">=18"}}"#);
        assert_eq!(v.engines["node"], ">=18");
    }

    #[test]
    fn dependency_maps_null_whole_field_is_empty() {
        let v = parse(
            r#"{
                "name": "x",
                "version": "1.0.0",
                "dependencies": null,
                "devDependencies": null,
                "peerDependencies": null,
                "optionalDependencies": null
            }"#,
        );
        assert!(v.dependencies.is_empty());
        assert!(v.dev_dependencies.is_empty());
        assert!(v.peer_dependencies.is_empty());
        assert!(v.optional_dependencies.is_empty());
    }

    fn parse_packument(json: &str) -> Packument {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn packument_dist_tags_drops_null_tag() {
        let p = parse_packument(
            r#"{
                "name": "pkg",
                "dist-tags": {"latest": "1.2.3", "beta": null}
            }"#,
        );
        assert_eq!(p.dist_tags.len(), 1);
        assert_eq!(p.dist_tags["latest"], "1.2.3");
    }

    #[test]
    fn packument_dist_tags_null_whole_field_is_empty() {
        let p = parse_packument(r#"{"name":"pkg","dist-tags":null}"#);
        assert!(p.dist_tags.is_empty());
    }

    #[test]
    fn packument_time_drops_null_entries() {
        let p = parse_packument(
            r#"{
                "name": "pkg",
                "time": {"1.0.0": "2024-01-01T00:00:00.000Z", "0.9.0": null}
            }"#,
        );
        assert_eq!(p.time.len(), 1);
        assert!(p.time.contains_key("1.0.0"));
    }

    #[test]
    fn packument_time_null_whole_field_is_empty() {
        let p = parse_packument(r#"{"name":"pkg","time":null}"#);
        assert!(p.time.is_empty());
    }
}
