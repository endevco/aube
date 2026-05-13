//! Supply-chain checks for `aube add`.
//!
//! Two probes against public APIs run before any new spec lands in
//! `package.json`:
//!
//! - [`fetch_malicious_advisories`] batch-queries `api.osv.dev` for
//!   `MAL-*` advisories. A hit is a confirmed-malicious package — the
//!   caller refuses the add with `ERR_AUBE_MALICIOUS_PACKAGE`.
//! - [`fetch_weekly_downloads`] looks up a package's `last-week`
//!   download count via `api.npmjs.org`. Typosquats and impersonations
//!   have near-zero downloads on day one regardless of how cleverly
//!   they're named, so a download floor catches the long tail of
//!   reported-after-the-fact malicious names.
//!
//! Both probes target public hosts and use their own reqwest client
//! rather than [`crate::client::RegistryClient`] — they don't need
//! the registry's auth/scoped-route logic, and isolating them keeps
//! the OSV failure mode (fail-open with a warning) from interacting
//! with packument fetch retries.

use serde::Deserialize;
use std::time::Duration;

/// HTTP timeout applied to every supply-chain probe. Keep tight: these
/// are non-critical gates on the human-intent path of `aube add`, and
/// a slow OSV mirror shouldn't add minutes of perceived latency to an
/// otherwise local operation.
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// Public host for OSV's batch-query endpoint.
const OSV_ENDPOINT: &str = "https://api.osv.dev/v1/querybatch";

/// Public host for npm's downloads API. The `point/last-week/{pkg}`
/// route returns one integer per request — cheap and rate-limit
/// friendly compared to the `range` endpoint.
const NPM_DOWNLOADS_BASE: &str = "https://api.npmjs.org/downloads/point/last-week";

/// One malicious-package advisory hit. We surface the OSV id and the
/// candidate package name; the caller composes a link of the form
/// `https://osv.dev/vulnerability/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaliciousAdvisory {
    pub package: String,
    pub advisory_id: String,
}

/// Errors raised by the supply-chain probes. Distinct from
/// [`crate::Error`] so callers can react differently to fail-open vs
/// fail-closed paths without parsing the inner reqwest error chain.
#[derive(Debug, thiserror::Error)]
pub enum SupplyChainError {
    #[error("supply-chain probe HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("supply-chain probe JSON decode failed: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("supply-chain probe returned non-success status: {0}")]
    Status(reqwest::StatusCode),
    /// OSV's batch endpoint contract guarantees one `results[i]` per
    /// `queries[i]`. A short response means a trailing subset of
    /// candidate names was never actually checked — silently
    /// treating that as "no advisories" would let a known-malicious
    /// package slip through on a truncated reply. The caller
    /// surfaces this as a probe failure so the configured
    /// fail-open/fail-closed policy applies.
    #[error("OSV returned {got} results for {expected} queries — truncated response")]
    TruncatedOsvResponse { expected: usize, got: usize },
}

#[derive(Debug, serde::Serialize)]
struct OsvQuery<'a> {
    package: OsvPackage<'a>,
}

#[derive(Debug, serde::Serialize)]
struct OsvPackage<'a> {
    name: &'a str,
    ecosystem: &'a str,
}

#[derive(Debug, serde::Serialize)]
struct OsvBatchRequest<'a> {
    queries: Vec<OsvQuery<'a>>,
}

#[derive(Debug, Deserialize, Default)]
struct OsvBatchResponse {
    #[serde(default)]
    results: Vec<OsvResult>,
}

#[derive(Debug, Deserialize, Default)]
struct OsvResult {
    #[serde(default)]
    vulns: Vec<OsvVuln>,
}

#[derive(Debug, Deserialize)]
struct OsvVuln {
    id: String,
}

#[derive(Debug, Deserialize)]
struct NpmDownloadsResponse {
    /// `point/last-week/<pkg>` returns this field on success; the
    /// `error` branch (scoped packages, unknown names) omits it.
    #[serde(default)]
    downloads: Option<u64>,
    /// Present when the registry returns a soft error rather than a
    /// non-2xx — typically `"package @scope/name not found"` for
    /// scoped packages, which the downloads API doesn't support.
    #[serde(default)]
    error: Option<String>,
}

/// Build the shared probe `reqwest::Client`. Centralized so the OSV
/// and downloads probes use identical timeout / TLS settings and so
/// `aube add a b c` can reuse a single client + connection pool
/// across all per-package downloads requests.
pub fn build_probe_client() -> Result<reqwest::Client, SupplyChainError> {
    Ok(reqwest::Client::builder().timeout(PROBE_TIMEOUT).build()?)
}

/// Probe OSV for `MAL-*` advisories on every candidate against a
/// caller-supplied shared client. Versions are intentionally
/// omitted from the query: typosquats and impersonation packages
/// are usually malicious in every published version, and we
/// haven't run the resolver yet when this fires.
///
/// Returns the subset of input names that hit a `MAL-*` advisory.
/// An `Err` is a fetch / decode / truncated-response failure — the
/// caller decides whether to surface it (`advisoryCheck=required`)
/// or warn-and-continue (`advisoryCheck=on`).
///
/// Mirrors [`fetch_weekly_downloads_with`]: the gate caller builds
/// one [`build_probe_client`] up front and threads it through both
/// probes so the OSV → downloads sequence reuses the same connection
/// pool across all per-package requests.
pub async fn fetch_malicious_advisories(
    client: &reqwest::Client,
    names: &[String],
) -> Result<Vec<MaliciousAdvisory>, SupplyChainError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let body = OsvBatchRequest {
        queries: names
            .iter()
            .map(|n| OsvQuery {
                package: OsvPackage {
                    name: n.as_str(),
                    ecosystem: "npm",
                },
            })
            .collect(),
    };
    let resp = client.post(OSV_ENDPOINT).json(&body).send().await?;
    if !resp.status().is_success() {
        return Err(SupplyChainError::Status(resp.status()));
    }
    let bytes = resp.bytes().await?;
    let parsed: OsvBatchResponse = serde_json::from_slice(&bytes)?;
    // Enforce the OSV `results[i] ↔ queries[i]` parity contract.
    // A short response is treated as a probe failure (not "no
    // advisories") so the trailing names aren't silently skipped —
    // the `advisoryCheck` policy then decides whether to warn-and-
    // continue or fail closed.
    if parsed.results.len() != names.len() {
        return Err(SupplyChainError::TruncatedOsvResponse {
            expected: names.len(),
            got: parsed.results.len(),
        });
    }
    Ok(extract_malicious(names, &parsed))
}

fn extract_malicious(names: &[String], resp: &OsvBatchResponse) -> Vec<MaliciousAdvisory> {
    // Caller (`fetch_malicious_advisories`) has already validated
    // `names.len() == resp.results.len()` and bailed otherwise, so
    // the zip below is safe — every input name has a corresponding
    // result slot. Tests call this helper directly with hand-built
    // responses; those happen to pass matched-length slices, so no
    // runtime check is needed here.
    let mut hits = Vec::new();
    for (name, result) in names.iter().zip(resp.results.iter()) {
        for vuln in &result.vulns {
            if vuln.id.starts_with("MAL-") {
                hits.push(MaliciousAdvisory {
                    package: name.clone(),
                    advisory_id: vuln.id.clone(),
                });
            }
        }
    }
    hits
}

/// Lookup result for a single package on npm's downloads API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadCount {
    /// Weekly downloads reported by the API.
    Known(u64),
    /// The API doesn't have data for this name. Common cases: scoped
    /// packages (`@scope/name`), brand-new packages with no published
    /// version, registry mirrors that don't proxy `api.npmjs.org`.
    /// Callers should treat this as "no signal" — skip the gate
    /// rather than fail closed, since absence of data is not
    /// evidence of typosquat.
    Unknown,
}

/// Look up `name`'s weekly download count using a caller-supplied
/// shared client. The caller is expected to reuse one
/// [`build_probe_client`] across every probe in an invocation so
/// the reqwest connection pool stays warm — see
/// `crates/aube/src/commands/add_supply_chain.rs::downloads_gate`.
pub async fn fetch_weekly_downloads_with(
    client: &reqwest::Client,
    name: &str,
) -> Result<DownloadCount, SupplyChainError> {
    // Scoped names contain `/` which must be percent-encoded for the
    // path segment. We still fire the request — npm returns a 404
    // with a JSON `error` body that the parse step recognizes.
    let encoded = name.replace('/', "%2F");
    let url = format!("{NPM_DOWNLOADS_BASE}/{encoded}");
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(DownloadCount::Unknown);
    }
    if !status.is_success() {
        return Err(SupplyChainError::Status(status));
    }
    let bytes = resp.bytes().await?;
    let parsed: NpmDownloadsResponse = serde_json::from_slice(&bytes)?;
    Ok(parse_downloads(&parsed))
}

fn parse_downloads(resp: &NpmDownloadsResponse) -> DownloadCount {
    if resp.error.is_some() {
        return DownloadCount::Unknown;
    }
    match resp.downloads {
        Some(n) => DownloadCount::Known(n),
        None => DownloadCount::Unknown,
    }
}

/// `https://osv.dev/vulnerability/<id>` — the user-facing URL for an
/// advisory id surfaced by [`fetch_malicious_advisories`]. Centralized
/// so the format stays consistent across the warn and error sites.
pub fn advisory_url(advisory_id: &str) -> String {
    format!("https://osv.dev/vulnerability/{advisory_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_malicious_filters_non_mal_ids() {
        // OSV returns GHSA-*/CVE-* alongside MAL-*; only MAL-* should
        // make it through this filter. Audit-class advisories belong
        // to `aube audit`, not the add-time block.
        let names = vec!["evil-pkg".to_string(), "fine-pkg".to_string()];
        let resp = OsvBatchResponse {
            results: vec![
                OsvResult {
                    vulns: vec![
                        OsvVuln {
                            id: "MAL-2026-3652".to_string(),
                        },
                        OsvVuln {
                            id: "GHSA-xxxx".to_string(),
                        },
                    ],
                },
                OsvResult {
                    vulns: vec![OsvVuln {
                        id: "CVE-2024-9999".to_string(),
                    }],
                },
            ],
        };
        let hits = extract_malicious(&names, &resp);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].package, "evil-pkg");
        assert_eq!(hits[0].advisory_id, "MAL-2026-3652");
    }

    #[test]
    fn truncated_osv_response_carries_lengths_in_error() {
        // `fetch_malicious_advisories` rejects a short response
        // rather than silently zipping the prefix — a missing
        // `results[i]` would otherwise let the corresponding query's
        // package skip the malicious-advisory gate entirely. The
        // error carries both expected and actual lengths so the
        // operator-facing log message names the gap.
        let err = SupplyChainError::TruncatedOsvResponse {
            expected: 3,
            got: 1,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("3"), "expected count missing: {rendered}");
        assert!(rendered.contains("1"), "got count missing: {rendered}");
        assert!(
            rendered.contains("truncated"),
            "category word missing: {rendered}"
        );
    }

    #[test]
    fn parse_downloads_treats_error_body_as_unknown() {
        // Scoped packages return 200 with `{"error": "package
        // @scope/name not found"}`. We need that to fold into
        // `Unknown` so callers don't trip the low-download gate
        // on every scoped install.
        let resp = NpmDownloadsResponse {
            downloads: None,
            error: Some("package @scope/name not found".to_string()),
        };
        assert_eq!(parse_downloads(&resp), DownloadCount::Unknown);
    }

    #[test]
    fn parse_downloads_reads_known_count() {
        let resp = NpmDownloadsResponse {
            downloads: Some(42_000_000),
            error: None,
        };
        assert_eq!(parse_downloads(&resp), DownloadCount::Known(42_000_000));
    }

    #[test]
    fn advisory_url_uses_osv_domain() {
        assert_eq!(
            advisory_url("MAL-2026-3652"),
            "https://osv.dev/vulnerability/MAL-2026-3652"
        );
    }
}
