use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

/// npm allows the `os`, `cpu`, and `libc` fields on a package version
/// to be either a single string (e.g. `"libc": "glibc"`) or an array
/// of strings (e.g. `"libc": ["glibc"]`). An explicit `null` is also
/// treated as "no constraint", same as the field being absent — some
/// packuments emit it. Normalize all three shapes to a `Vec<String>`
/// so the platform filter doesn't have to care.
fn string_or_seq<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrSeq {
        String(String),
        Seq(Vec<String>),
    }
    Ok(match Option::<StringOrSeq>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(StringOrSeq::String(s)) => vec![s],
        Some(StringOrSeq::Seq(v)) => v,
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
    #[serde(default, deserialize_with = "non_string_tolerant_map")]
    pub engines: BTreeMap<String, String>,
    /// `bin:` presence — `true` when the packument carried any entry
    /// under `bin`, either as a string (`"bin": "cli.js"`) or a map
    /// (`"bin": {"foo": "cli.js"}`). pnpm records this as `hasBin: true`
    /// on the package line; `false` is never written. Renamed from
    /// `bin` in the packument because we collapse the shape to a bool
    /// at the deserialize step — callers that need the full map can
    /// re-fetch the tarball manifest.
    #[serde(default, rename = "bin", deserialize_with = "bin_is_present")]
    pub has_bin: bool,
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

/// pnpm's `hasBin: true` maps to "the manifest has at least one `bin`
/// entry". The packument records `bin` as either a string
/// (`"cli.js"`) or a map (`{name: path}`); either form with at least
/// one character/entry is truthy. A missing `bin` field deserializes
/// to `false`.
///
/// The disk cache round-trips this field as a plain `bool` (we serialize
/// through the same field name `bin`), so we *must* accept
/// `Value::Bool(b)` and preserve its value — otherwise a warm-cache
/// read flips `false` → `true` on the fallthrough arm and every package
/// ends up tagged `hasBin: true` on re-emit.
///
/// Other shapes (numbers, arrays) are treated as "present, assume
/// truthy" — no real packument emits those, but a permissive default
/// avoids parse failures on a registry that does something exotic.
fn bin_is_present<'de, D>(de: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(de)?;
    Ok(match value {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(b)) => b,
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Object(m)) => !m.is_empty(),
        Some(_) => true,
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

    #[test]
    fn has_bin_reads_bin_field() {
        let missing = parse(r#"{"name":"x","version":"1.0.0"}"#);
        assert!(!missing.has_bin, "missing bin → false");
        let empty_string = parse(r#"{"name":"x","version":"1.0.0","bin":""}"#);
        assert!(!empty_string.has_bin, "empty string bin → false");
        let null_bin = parse(r#"{"name":"x","version":"1.0.0","bin":null}"#);
        assert!(!null_bin.has_bin, "null bin → false");
        let empty_map = parse(r#"{"name":"x","version":"1.0.0","bin":{}}"#);
        assert!(!empty_map.has_bin, "empty map bin → false");
        let string_bin = parse(r#"{"name":"x","version":"1.0.0","bin":"cli.js"}"#);
        assert!(string_bin.has_bin, "non-empty string bin → true");
        let map_bin = parse(r#"{"name":"x","version":"1.0.0","bin":{"x":"cli.js"}}"#);
        assert!(map_bin.has_bin, "non-empty map bin → true");
    }

    /// Round-trip the `has_bin` field through the on-disk cache format
    /// (serialize → parse). Regression: the disk cache stores this
    /// field as a bool under the name `bin`; a permissive
    /// deserializer that treated any non-string/non-object value as
    /// "present" would flip `false` to `true` on every warm-cache
    /// read and re-tag every package as `hasBin: true`.
    #[test]
    fn has_bin_roundtrips_through_bool_serialization() {
        let v = VersionMetadata {
            name: "x".to_string(),
            version: "1.0.0".to_string(),
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
            has_bin: false,
            has_install_script: false,
            deprecated: None,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: VersionMetadata = serde_json::from_str(&json).unwrap();
        assert!(!back.has_bin, "false has_bin must round-trip as false");
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
