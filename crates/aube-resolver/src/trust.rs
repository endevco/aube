//! Trust-policy enforcement.
//!
//! Mirrors pnpm's `failIfTrustDowngraded`
//! (resolving/npm-resolver/src/trustChecks.ts), verified against pnpm's
//! own test suite. Two trust-evidence sources, ranked
//! `TrustedPublisher (2) > Provenance (1)`. The check runs immediately
//! after a version is picked from a packument: if any strictly older
//! version of the same package had stronger trust evidence, the install
//! fails. Pre-2010 packuments without per-version `time` entries error
//! when the picked version isn't excluded — same as pnpm.

use aube_registry::{Packument, VersionMetadata};
use std::time::{SystemTime, UNIX_EPOCH};

/// Trust-evidence ranks. Higher is stronger. Variants intentionally do
/// not derive `Ord` — the variant declaration order does not match the
/// rank order, so callers must go through [`Self::rank`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustEvidence {
    TrustedPublisher,
    Provenance,
}

impl TrustEvidence {
    pub fn rank(self) -> u8 {
        match self {
            Self::TrustedPublisher => 2,
            Self::Provenance => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TrustedPublisher => "trusted publisher",
            Self::Provenance => "provenance attestation",
        }
    }
}

/// Strongest trust evidence carried by a single version's metadata.
/// Mirrors pnpm's `getTrustEvidence`: `_npmUser.trustedPublisher`
/// outranks `dist.attestations.provenance`. JS-style truthiness on the
/// trustedPublisher value: any non-null, non-`false` value counts.
pub fn evidence_for(meta: &VersionMetadata) -> Option<TrustEvidence> {
    if meta
        .npm_user
        .as_ref()
        .and_then(|u| u.trusted_publisher.as_ref())
        .is_some_and(is_truthy)
    {
        return Some(TrustEvidence::TrustedPublisher);
    }
    if meta
        .dist
        .as_ref()
        .and_then(|d| d.attestations.as_ref())
        .and_then(|a| a.provenance.as_ref())
        .is_some_and(is_truthy)
    {
        return Some(TrustEvidence::Provenance);
    }
    None
}

/// JS-style truthiness for `_npmUser.trustedPublisher`. Pnpm's check
/// is `manifest._npmUser?.trustedPublisher`, which evaluates the value
/// in a boolean context — so we have to reject every JS falsy value,
/// not just `null`/`false`. Falsy: `null`, `false`, `0`, `""`, `NaN`.
/// Truthy: every object (including `{}`), every array (including `[]`),
/// every non-zero number, every non-empty string. In practice the
/// field is always either an object or absent, but matching JS
/// semantics keeps a hostile registry from sneaking trust evidence
/// past us via `0` or an empty string.
fn is_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0 && !f.is_nan()),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

#[derive(Debug)]
pub enum TrustCheckError {
    Downgrade(TrustDowngradeDetails),
    MissingTime(MissingTimeDetails),
}

#[derive(Debug)]
pub struct TrustDowngradeDetails {
    pub name: String,
    pub picked_version: String,
    pub current_evidence: Option<TrustEvidence>,
    pub prior_evidence: TrustEvidence,
    pub prior_version: String,
}

#[derive(Debug)]
pub struct MissingTimeDetails {
    pub name: String,
    pub version: String,
}

/// Run the trust-downgrade check. Returns `Ok(())` when the picked
/// version is acceptable (excluded, missing-evidence-everywhere, older
/// than `ignore_after_minutes`, or carrying evidence at least as strong
/// as the strongest prior version's). Errors otherwise.
///
/// Step ordering matters: exclude check runs *before* the time lookup
/// so an excluded `name@version` does not surface a `MissingTime` error
/// when the registry omits the `time` field. Verified against pnpm's
/// `does not fail with ERR_PNPM_MISSING_TIME when ... excluded` tests.
pub fn check_no_downgrade(
    packument: &Packument,
    picked_version: &str,
    picked_meta: &VersionMetadata,
    exclude: &TrustExcludeRules,
    ignore_after_minutes: Option<u64>,
) -> Result<(), TrustCheckError> {
    let picked_parsed = node_semver::Version::parse(picked_version).ok();

    if let Some(ref pv) = picked_parsed {
        if exclude.matches(&packument.name, pv) {
            return Ok(());
        }
    } else if exclude.matches_name_only(&packument.name) {
        return Ok(());
    }

    let Some(picked_time) = packument.time.get(picked_version) else {
        return Err(TrustCheckError::MissingTime(MissingTimeDetails {
            name: packument.name.clone(),
            version: picked_version.to_string(),
        }));
    };

    if let Some(minutes) = ignore_after_minutes
        && minutes > 0
        && let Some(cutoff) = cutoff_iso8601(minutes)
        && picked_time.as_str() < cutoff.as_str()
    {
        return Ok(());
    }

    // pnpm v10.24.0+: when the picked version is a stable release,
    // ignore prior prerelease evidence — a trusted alpha shouldn't
    // block a stable that omits attestation.
    let exclude_prereleases = picked_parsed
        .as_ref()
        .map(|v| v.pre_release.is_empty())
        .unwrap_or(false);

    let mut best: Option<(TrustEvidence, &str)> = None;
    for (other_ver, other_meta) in &packument.versions {
        if other_ver == picked_version {
            continue;
        }
        let Some(other_time) = packument.time.get(other_ver) else {
            continue;
        };
        if other_time.as_str() >= picked_time.as_str() {
            continue;
        }
        if exclude_prereleases
            && let Ok(parsed) = node_semver::Version::parse(other_ver)
            && !parsed.pre_release.is_empty()
        {
            continue;
        }
        let Some(evidence) = evidence_for(other_meta) else {
            continue;
        };
        match best {
            None => best = Some((evidence, other_ver.as_str())),
            Some((cur, _)) if evidence.rank() > cur.rank() => {
                best = Some((evidence, other_ver.as_str()));
            }
            _ => {}
        }
        if matches!(best, Some((TrustEvidence::TrustedPublisher, _))) {
            break;
        }
    }

    let Some((prior_evidence, prior_version)) = best else {
        return Ok(());
    };

    let current = evidence_for(picked_meta);
    let current_rank = current.map_or(0, TrustEvidence::rank);
    if current_rank < prior_evidence.rank() {
        return Err(TrustCheckError::Downgrade(TrustDowngradeDetails {
            name: packument.name.clone(),
            picked_version: picked_version.to_string(),
            current_evidence: current,
            prior_evidence,
            prior_version: prior_version.to_string(),
        }));
    }
    Ok(())
}

fn cutoff_iso8601(minutes_ago: u64) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let cutoff_secs = now.saturating_sub(minutes_ago * 60);
    Some(crate::types::format_iso8601_utc(cutoff_secs))
}

/// Parsed `trustPolicyExclude` rules. Mirrors pnpm's
/// `createPackageVersionPolicy` (config/version-policy/src/index.ts).
/// Each rule is `<name>` (matches all versions, supports `*` glob in
/// the name) or `<name>@<exact-version>[ || <exact-version>]…` (no
/// ranges, no name globs combined with versions).
#[derive(Debug, Clone, Default)]
pub struct TrustExcludeRules {
    rules: Vec<TrustExcludeRule>,
}

#[derive(Debug, Clone)]
struct TrustExcludeRule {
    name_matcher: NameMatcher,
    /// `None` → rule matches every version of any name match.
    /// `Some(versions)` → rule matches only those exact versions.
    exact_versions: Option<Vec<node_semver::Version>>,
}

#[derive(Debug, Clone)]
enum NameMatcher {
    Exact(String),
    Glob(GlobMatcher),
    Any,
}

#[derive(Debug, Clone)]
struct GlobMatcher {
    parts: Vec<String>,
    leading_wildcard: bool,
    trailing_wildcard: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TrustExcludeParseError {
    #[error(
        "invalid trustPolicyExclude pattern `{pattern}`: only exact versions are allowed in version unions, ranges (^/~/>=) are not supported"
    )]
    InvalidVersionUnion { pattern: String },
    #[error(
        "invalid trustPolicyExclude pattern `{pattern}`: name patterns (`*`) cannot be combined with version unions"
    )]
    NameGlobWithVersions { pattern: String },
}

impl TrustExcludeRules {
    pub fn parse<I, S>(patterns: I) -> Result<Self, TrustExcludeParseError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut rules = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if pattern.is_empty() {
                continue;
            }
            rules.push(parse_one(pattern)?);
        }
        Ok(Self { rules })
    }

    /// Parse a list of patterns, keeping every rule that succeeds and
    /// returning the per-pattern errors for everything that didn't.
    /// Lets the caller log malformed entries individually without
    /// dropping the rules that did parse — a strict batch `parse` would
    /// turn one typo into a silent security regression where every
    /// exclude vanishes.
    pub fn parse_lossy<I, S>(patterns: I) -> (Self, Vec<TrustExcludeParseError>)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut rules = Vec::new();
        let mut errors = Vec::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            if pattern.is_empty() {
                continue;
            }
            match parse_one(pattern) {
                Ok(rule) => rules.push(rule),
                Err(err) => errors.push(err),
            }
        }
        (Self { rules }, errors)
    }

    fn matches(&self, name: &str, version: &node_semver::Version) -> bool {
        for rule in &self.rules {
            if !rule.name_matcher.matches(name) {
                continue;
            }
            match &rule.exact_versions {
                None => return true,
                Some(versions) => {
                    if versions.iter().any(|v| v == version) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Used when the picked version string fails semver parse — only a
    /// no-version rule can match in that case (pnpm behavior:
    /// `evaluateVersionPolicy` returns `true` for name-only rules
    /// before the version array branch is taken).
    fn matches_name_only(&self, name: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.exact_versions.is_none() && r.name_matcher.matches(name))
    }
}

fn parse_one(pattern: &str) -> Result<TrustExcludeRule, TrustExcludeParseError> {
    let scoped = pattern.starts_with('@');
    let at_index = if scoped {
        pattern[1..].find('@').map(|i| i + 1)
    } else {
        pattern.find('@')
    };

    let (name_part, versions_part) = match at_index {
        Some(i) => (&pattern[..i], Some(&pattern[i + 1..])),
        None => (pattern, None),
    };

    let exact_versions = match versions_part {
        None => None,
        Some(versions_str) => {
            if name_part.contains('*') {
                return Err(TrustExcludeParseError::NameGlobWithVersions {
                    pattern: pattern.to_string(),
                });
            }
            let mut parsed = Vec::new();
            for chunk in versions_str.split("||") {
                let trimmed = chunk.trim();
                if trimmed.is_empty() {
                    return Err(TrustExcludeParseError::InvalidVersionUnion {
                        pattern: pattern.to_string(),
                    });
                }
                let v = node_semver::Version::parse(trimmed).map_err(|_| {
                    TrustExcludeParseError::InvalidVersionUnion {
                        pattern: pattern.to_string(),
                    }
                })?;
                parsed.push(v);
            }
            Some(parsed)
        }
    };

    Ok(TrustExcludeRule {
        name_matcher: NameMatcher::compile(name_part),
        exact_versions,
    })
}

impl NameMatcher {
    fn compile(pattern: &str) -> Self {
        if pattern == "*" {
            return Self::Any;
        }
        if !pattern.contains('*') {
            return Self::Exact(pattern.to_string());
        }
        let parts: Vec<String> = pattern.split('*').map(str::to_string).collect();
        Self::Glob(GlobMatcher {
            leading_wildcard: parts.first().is_some_and(String::is_empty),
            trailing_wildcard: parts.last().is_some_and(String::is_empty),
            parts: parts.into_iter().filter(|s| !s.is_empty()).collect(),
        })
    }

    fn matches(&self, input: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(s) => s == input,
            Self::Glob(g) => g.matches(input),
        }
    }
}

impl GlobMatcher {
    fn matches(&self, input: &str) -> bool {
        if self.parts.is_empty() {
            return true;
        }
        let mut cursor = 0usize;
        for (i, segment) in self.parts.iter().enumerate() {
            let search_window = &input[cursor..];
            let is_first = i == 0;
            let is_last = i == self.parts.len() - 1;
            if is_first && !self.leading_wildcard {
                if !search_window.starts_with(segment.as_str()) {
                    return false;
                }
                cursor += segment.len();
            } else if is_last && !self.trailing_wildcard {
                if !search_window.ends_with(segment.as_str()) {
                    return false;
                }
                if search_window.len() < segment.len() {
                    return false;
                }
                cursor = input.len();
            } else {
                let Some(idx) = search_window.find(segment.as_str()) else {
                    return false;
                };
                cursor += idx + segment.len();
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_registry::{Attestations, Dist, NpmUser};
    use std::collections::BTreeMap;

    fn version(name: &str, ver: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_string(),
            version: ver.to_string(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            bundled_dependencies: None,
            dist: Some(Dist {
                tarball: format!("https://r/{name}/-/{name}-{ver}.tgz"),
                integrity: None,
                shasum: None,
                attestations: None,
            }),
            os: vec![],
            cpu: vec![],
            libc: vec![],
            engines: BTreeMap::new(),
            license: None,
            funding_url: None,
            bin: BTreeMap::new(),
            has_install_script: false,
            deprecated: None,
            npm_user: None,
        }
    }

    fn with_provenance(mut v: VersionMetadata) -> VersionMetadata {
        let dist = v.dist.as_mut().unwrap();
        dist.attestations = Some(Attestations {
            provenance: Some(serde_json::json!({"predicateType": "slsa"})),
        });
        v
    }

    fn with_trusted_publisher(mut v: VersionMetadata) -> VersionMetadata {
        v.npm_user = Some(NpmUser {
            trusted_publisher: Some(serde_json::json!({"id": "gh"})),
        });
        v
    }

    fn packument(name: &str, versions: Vec<(&str, &str, VersionMetadata)>) -> Packument {
        let mut p = Packument {
            name: name.to_string(),
            modified: None,
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        for (ver, time, meta) in versions {
            p.versions.insert(ver.to_string(), meta);
            p.time.insert(ver.to_string(), time.to_string());
        }
        p
    }

    #[test]
    fn evidence_trusted_publisher_outranks_provenance() {
        let v = with_trusted_publisher(with_provenance(version("foo", "1.0.0")));
        assert_eq!(evidence_for(&v), Some(TrustEvidence::TrustedPublisher));
    }

    #[test]
    fn evidence_provenance_only() {
        let v = with_provenance(version("foo", "1.0.0"));
        assert_eq!(evidence_for(&v), Some(TrustEvidence::Provenance));
    }

    #[test]
    fn evidence_npm_user_without_trusted_publisher_is_none() {
        let mut v = version("foo", "1.0.0");
        v.npm_user = Some(NpmUser {
            trusted_publisher: None,
        });
        assert_eq!(evidence_for(&v), None);
    }

    #[test]
    fn evidence_falsy_trusted_publisher_is_none() {
        let mut v = version("foo", "1.0.0");
        for falsy in [
            serde_json::Value::Bool(false),
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(0.0),
            serde_json::json!(""),
        ] {
            v.npm_user = Some(NpmUser {
                trusted_publisher: Some(falsy.clone()),
            });
            assert_eq!(evidence_for(&v), None, "{falsy:?} should be falsy");
        }
    }

    #[test]
    fn evidence_falsy_provenance_is_none() {
        // Symmetric with the trustedPublisher truthiness check —
        // `dist.attestations.provenance` is also evaluated in JS
        // boolean context in pnpm, so falsy values must not count
        // as evidence. Real registries always emit an object here,
        // but this guards against a hostile registry shipping
        // `provenance: false` to bypass the no-downgrade check.
        let mut v = version("foo", "1.0.0");
        for falsy in [
            serde_json::Value::Bool(false),
            serde_json::Value::Null,
            serde_json::json!(0),
            serde_json::json!(""),
        ] {
            v.dist.as_mut().unwrap().attestations = Some(Attestations {
                provenance: Some(falsy.clone()),
            });
            assert_eq!(evidence_for(&v), None, "{falsy:?} should be falsy");
        }
    }

    #[test]
    fn evidence_truthy_non_object_trusted_publisher_counts() {
        // JS truthy values that aren't objects: non-zero numbers,
        // non-empty strings, empty arrays/objects. Real registries only
        // ever emit an object here, but match JS semantics for
        // defense-in-depth against a hostile/buggy registry.
        let mut v = version("foo", "1.0.0");
        for truthy in [
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!("oidc:gh"),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            v.npm_user = Some(NpmUser {
                trusted_publisher: Some(truthy.clone()),
            });
            assert_eq!(
                evidence_for(&v),
                Some(TrustEvidence::TrustedPublisher),
                "{truthy:?} should be truthy"
            );
        }
    }

    #[test]
    fn evidence_none_when_neither() {
        let v = version("foo", "1.0.0");
        assert_eq!(evidence_for(&v), None);
    }

    #[test]
    fn no_evidence_anywhere_passes() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                ("2.0.0", "2025-02-01T00:00:00.000Z", version("foo", "2.0.0")),
            ],
        );
        let picked = p.versions.get("2.0.0").unwrap();
        let result = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn first_attested_version_passes() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("1.0.0").unwrap();
        let result = check_no_downgrade(&p, "1.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_ok(),
            "version 1.0.0 was published first; it has nothing prior to compare against"
        );
    }

    #[test]
    fn downgrade_provenance_to_none_fails() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let err = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("3.0.0 should fail: prior version had provenance, this one has none");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::Provenance);
                assert_eq!(d.prior_version, "2.0.0");
                assert_eq!(d.current_evidence, None);
            }
            _ => panic!("expected Downgrade"),
        }
    }

    #[test]
    fn downgrade_trusted_publisher_to_provenance_fails() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "2.0.0")),
                ),
                (
                    "3.0.0",
                    "2025-03-01T00:00:00.000Z",
                    with_provenance(version("foo", "3.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let err = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("trustedPublisher → provenance is a downgrade");
        match err {
            TrustCheckError::Downgrade(d) => {
                assert_eq!(d.prior_evidence, TrustEvidence::TrustedPublisher);
                assert_eq!(d.current_evidence, Some(TrustEvidence::Provenance));
            }
            _ => panic!("expected Downgrade"),
        }
    }

    #[test]
    fn same_trust_level_passes() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "2.0.0")),
                ),
                (
                    "3.0.0",
                    "2025-03-01T00:00:00.000Z",
                    with_trusted_publisher(version("foo", "3.0.0")),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(result.is_ok());
    }

    #[test]
    fn prior_prerelease_ignored_when_picking_stable() {
        let p = packument(
            "foo",
            vec![
                ("1.0.0", "2025-01-01T00:00:00.000Z", version("foo", "1.0.0")),
                (
                    "2.0.0-0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0-0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_ok(),
            "trusted prerelease shouldn't block a stable that omits attestation"
        );
    }

    #[test]
    fn prior_prerelease_counts_when_picking_prerelease() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0-0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0-0")),
                ),
                (
                    "3.0.0-0",
                    "2025-03-01T00:00:00.000Z",
                    version("foo", "3.0.0-0"),
                ),
            ],
        );
        let picked = p.versions.get("3.0.0-0").unwrap();
        let result = check_no_downgrade(&p, "3.0.0-0", picked, &TrustExcludeRules::default(), None);
        assert!(
            result.is_err(),
            "prerelease pick should compare against prior prereleases"
        );
    }

    #[test]
    fn missing_time_for_picked_version_errors() {
        let mut p = packument(
            "foo",
            vec![
                (
                    "1.0.0",
                    "2025-01-01T00:00:00.000Z",
                    with_provenance(version("foo", "1.0.0")),
                ),
                ("2.0.0", "2025-02-01T00:00:00.000Z", version("foo", "2.0.0")),
            ],
        );
        // Drop the time entry for 2.0.0.
        p.time.remove("2.0.0");
        let picked = p.versions.get("2.0.0").unwrap();
        let err = check_no_downgrade(&p, "2.0.0", picked, &TrustExcludeRules::default(), None)
            .expect_err("missing time should error");
        assert!(matches!(err, TrustCheckError::MissingTime(_)));
    }

    #[test]
    fn exclude_name_at_version_bypasses_missing_time() {
        // No time field anywhere — would normally error.
        let p = Packument {
            name: "baz".to_string(),
            modified: None,
            versions: {
                let mut m = BTreeMap::new();
                m.insert("1.0.0".to_string(), version("baz", "1.0.0"));
                m
            },
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        let picked = p.versions.get("1.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["baz@1.0.0"]).unwrap();
        let result = check_no_downgrade(&p, "1.0.0", picked, &exclude, None);
        assert!(result.is_ok(), "excluded version must skip the time lookup");
    }

    #[test]
    fn exclude_name_only_bypasses_missing_time() {
        let p = Packument {
            name: "qux".to_string(),
            modified: None,
            versions: {
                let mut m = BTreeMap::new();
                m.insert("2.0.0".to_string(), version("qux", "2.0.0"));
                m
            },
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        };
        let picked = p.versions.get("2.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["qux"]).unwrap();
        let result = check_no_downgrade(&p, "2.0.0", picked, &exclude, None);
        assert!(result.is_ok());
    }

    #[test]
    fn exclude_blocks_downgrade_failure() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        let exclude = TrustExcludeRules::parse(["foo@3.0.0"]).unwrap();
        let result = check_no_downgrade(&p, "3.0.0", picked, &exclude, None);
        assert!(result.is_ok(), "exclude should bypass the downgrade");
    }

    #[test]
    fn ignore_after_skips_old_versions() {
        let p = packument(
            "foo",
            vec![
                (
                    "2.0.0",
                    "2025-02-01T00:00:00.000Z",
                    with_provenance(version("foo", "2.0.0")),
                ),
                ("3.0.0", "2025-03-01T00:00:00.000Z", version("foo", "3.0.0")),
            ],
        );
        let picked = p.versions.get("3.0.0").unwrap();
        // 1 minute cutoff — both versions are way older, should skip.
        let result =
            check_no_downgrade(&p, "3.0.0", picked, &TrustExcludeRules::default(), Some(1));
        assert!(result.is_ok());
    }

    // ---------- TrustExcludeRules parsing ----------

    #[test]
    fn exclude_parses_name_only() {
        let r = TrustExcludeRules::parse(["foo"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("99.0.0").unwrap()));
        assert!(!r.matches("bar", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_name_at_version() {
        let r = TrustExcludeRules::parse(["foo@1.0.0"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("1.0.1").unwrap()));
    }

    #[test]
    fn exclude_parses_version_union() {
        let r = TrustExcludeRules::parse(["foo@1.0.0 || 2.0.0 || 3.0.0"]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("2.0.0").unwrap()));
        assert!(r.matches("foo", &node_semver::Version::parse("3.0.0").unwrap()));
        assert!(!r.matches("foo", &node_semver::Version::parse("4.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_scoped_name() {
        let r = TrustExcludeRules::parse(["@babel/core@7.20.0"]).unwrap();
        assert!(r.matches(
            "@babel/core",
            &node_semver::Version::parse("7.20.0").unwrap()
        ));
        assert!(!r.matches(
            "@babel/core",
            &node_semver::Version::parse("7.20.1").unwrap()
        ));
    }

    #[test]
    fn exclude_parses_scoped_name_only() {
        let r = TrustExcludeRules::parse(["@babel/core"]).unwrap();
        assert!(r.matches(
            "@babel/core",
            &node_semver::Version::parse("9.9.9").unwrap()
        ));
    }

    #[test]
    fn exclude_parses_glob() {
        let r = TrustExcludeRules::parse(["is-*"]).unwrap();
        assert!(r.matches("is-odd", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(r.matches("is-even", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(!r.matches("lodash", &node_semver::Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn exclude_parses_star_matches_all() {
        let r = TrustExcludeRules::parse(["*"]).unwrap();
        assert!(r.matches("anything", &node_semver::Version::parse("0.0.1").unwrap()));
    }

    #[test]
    fn exclude_rejects_range_operators() {
        for bad in ["foo@^1.0.0", "foo@~1.0.0", "foo@>=1.0.0"] {
            let err = TrustExcludeRules::parse([bad]).expect_err(bad);
            assert!(matches!(
                err,
                TrustExcludeParseError::InvalidVersionUnion { .. }
            ));
        }
    }

    #[test]
    fn exclude_rejects_glob_with_version() {
        let err = TrustExcludeRules::parse(["is-*@1.0.0"]).expect_err("glob+version");
        assert!(matches!(
            err,
            TrustExcludeParseError::NameGlobWithVersions { .. }
        ));
    }

    #[test]
    fn parse_lossy_keeps_valid_drops_invalid() {
        let (rules, errors) = TrustExcludeRules::parse_lossy([
            "good",
            "bad@^1.0.0",
            "@scope/also-good@1.0.0",
            "is-*@nope",
        ]);
        // Two valid rules survive; two invalid surface as separate errors.
        assert!(rules.matches("good", &node_semver::Version::parse("1.0.0").unwrap()));
        assert!(rules.matches(
            "@scope/also-good",
            &node_semver::Version::parse("1.0.0").unwrap()
        ));
        assert_eq!(errors.len(), 2, "two malformed entries reported");
    }

    #[test]
    fn exclude_skips_empty_patterns() {
        // npm config arrays sometimes include empty entries; ignore them.
        let r = TrustExcludeRules::parse(["", "foo", ""]).unwrap();
        assert!(r.matches("foo", &node_semver::Version::parse("1.0.0").unwrap()));
    }
}
