//! Speculative TCP+TLS handshake before the first real request.
//!
//! Cold installs pay one full handshake per distinct origin (~50-150 ms
//! on a 50 ms-RTT path). Issuing a HEAD against each known origin in
//! parallel during manifest parsing overlaps the handshake with the
//! resolver's first packument decision. The HEAD response itself is
//! discarded; the win is the warm pool entry the next real request
//! reuses.
//!
//! `AUBE_DISABLE_SPECULATIVE_TLS=1` skips the prewarm entirely. Wrong
//! registry, network failure, or auth rejection are silently dropped:
//! subsequent real requests take the standard path and surface their
//! own errors.

use std::time::Duration;

const PREWARM_TIMEOUT: Duration = Duration::from_secs(5);

/// Returns true when the speculative TLS prewarm is disabled.
#[inline]
pub fn is_disabled() -> bool {
    std::env::var_os("AUBE_DISABLE_SPECULATIVE_TLS").is_some()
}

/// Spawn a fire-and-forget HEAD against each `(client, url)` pair on
/// the current tokio runtime. Errors trace at debug — handshake
/// failures here predict the real-request failure that follows.
///
/// Each pair carries its own `reqwest::Client` because aube tracks one
/// pool per auth-uri (`http_by_uri`); a single shared client would
/// merge pools for registries with different auth headers.
pub fn spawn_head<I>(targets: I)
where
    I: IntoIterator<Item = (reqwest::Client, String)>,
{
    if is_disabled() {
        return;
    }
    for (http, url) in targets {
        tokio::spawn(async move {
            if let Err(e) = http.head(&url).timeout(PREWARM_TIMEOUT).send().await {
                tracing::debug!(error = %e, url = %url, "tls prewarm failed");
            }
        });
    }
}
