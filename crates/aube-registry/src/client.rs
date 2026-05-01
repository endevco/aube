use crate::config::{FetchPolicy, NpmConfig};
use crate::{Error, NetworkMode, Packument};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Disk-cached packument with revalidation metadata.
#[derive(Debug, Serialize, Deserialize)]
struct CachedPackument {
    etag: Option<String>,
    last_modified: Option<String>,
    /// Unix epoch seconds when this entry was written
    fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_age_secs: Option<u64>,
    packument: Packument,
}

/// Disk-cached *full* (non-corgi) packument. Stored as raw JSON so we
/// preserve fields the resolver doesn't parse (`description`, `repository`,
/// `license`, `keywords`, `maintainers`, ...), for use by human-facing
/// commands like `aube view`.
#[derive(Debug, Serialize, Deserialize)]
struct CachedFullPackument {
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_age_secs: Option<u64>,
    packument: serde_json::Value,
}

#[derive(Debug, Default)]
pub struct CachedPackumentLookup {
    pub packument: Option<Packument>,
    pub stale: bool,
    cached: Option<CachedPackumentLookupEntry>,
}

#[derive(Debug)]
enum CachedPackumentLookupEntry {
    Abbreviated(CachedPackument),
    Full(CachedFullPackumentTyped),
}

#[derive(Debug)]
struct CachedFullPackumentTyped {
    etag: Option<String>,
    last_modified: Option<String>,
    fetched_at: u64,
    max_age_secs: Option<u64>,
    packument: Packument,
}

fn cached_is_fresh(fetched_at: u64, max_age_secs: Option<u64>) -> bool {
    let age = now_secs().saturating_sub(fetched_at);
    let budget = max_age_secs.unwrap_or(PACKUMENT_TTL_SECS);
    age < budget
}

/// How long to trust a cached packument before revalidating with the registry.
/// Trust cached packuments for 30 minutes before revalidating. This keeps
/// repeated installs in a long-lived dev session from devolving into hundreds
/// of conditional metadata requests once the cache is just over pnpm's 5-minute
/// default staleness window.
const PACKUMENT_TTL_SECS: u64 = 1800;

fn is_retriable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Accept header for packument requests. `vnd.npm.install-v1+json` is the
/// abbreviated (corgi) format npmjs emits for installs; the `application/json`
/// fallback covers registries (Verdaccio, older Artifactory, private mirrors)
/// whose proxy layer normalizes Accept and would otherwise return 406 on the
/// corgi-only form. `*/*` keeps us compatible with anything that strips the
/// fancy media types entirely. Same shape npm-cli / pnpm send.
const PACKUMENT_ACCEPT: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";

/// Accept header for the full (non-corgi) packument route used by `aube view`
/// and mutating commands. Adds `*/*` as a fallback for the same reason as
/// `PACKUMENT_ACCEPT` — some proxies won't serve JSON unless it's in the list.
const PACKUMENT_FULL_ACCEPT: &str = "application/json; q=1.0, */*";

// Packument and tarball body caps are configurable via the
// `packumentMaxBytes` / `tarballMaxBytes` settings. Defaults live in
// `FetchPolicy::default()`; setting either to `0` disables the cap.
// These are hardening knobs against hostile or misconfigured
// registries streaming runaway bodies into the resolver.

/// Hard cap for the `/-/npm/v1/security/advisories/bulk` response. The
/// body scales with the number of distinct `<name>@<version>` pairs in
/// the request, which is bounded by the lockfile. 256 MiB gives an
/// extremely generous upper bound for monorepos with tens of thousands
/// of locked versions.
const AUDIT_BODY_CAP: u64 = 256 << 20;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pull ETag + Last-Modified off a response as owned strings.
fn extract_cache_headers(resp: &reqwest::Response) -> (Option<String>, Option<String>) {
    let headers = resp.headers();
    let grab = |name: reqwest::header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    (
        grab(reqwest::header::ETAG),
        grab(reqwest::header::LAST_MODIFIED),
    )
}

fn parse_cache_control_max_age(resp: &reqwest::Response) -> Option<u64> {
    let raw = resp
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())?;
    let mut max_age = None;
    let mut s_maxage = None;
    let mut force_revalidate = false;
    for directive in raw.split(',').map(str::trim) {
        let directive_lc = directive.to_ascii_lowercase();
        match directive_lc.as_str() {
            "no-store" | "no-cache" | "private" => force_revalidate = true,
            _ => {}
        }
        if let Some(val) = directive_lc.strip_prefix("s-maxage=") {
            s_maxage = val.parse::<u64>().ok();
        } else if let Some(val) = directive_lc.strip_prefix("max-age=") {
            max_age = val.parse::<u64>().ok();
        }
    }
    if force_revalidate {
        return Some(0);
    }
    s_maxage.or(max_age)
}

/// Client for interacting with the npm registry.
pub struct RegistryClient {
    http: reqwest::Client,
    http_by_uri: BTreeMap<String, reqwest::Client>,
    token_helper_cache: Mutex<BTreeMap<String, Option<String>>>,
    /// Memoized result of `registry_auth_token_for(url)`. Without this,
    /// every authed request walks `auth_by_uri` for a longest-prefix
    /// match against `registry_url`. On a 2000-package install that's
    /// 2000 × O(N_uris × strcmp) wasted lookups. The token is fixed
    /// for the lifetime of the process (helpers are already memoized
    /// in `token_helper_cache`), so per-URL caching is safe.
    auth_token_by_url: Mutex<BTreeMap<String, Option<String>>>,
    config: NpmConfig,
    network_mode: NetworkMode,
    fetch_policy: FetchPolicy,
}

impl RegistryClient {
    pub fn new(registry_url: &str) -> Self {
        // `NpmConfig::load` folds proxy env vars into the config so
        // that `from_config` can later call `.no_proxy()` on the
        // reqwest builder and still honor them. This constructor
        // skips `load` (it has no `.npmrc` to read), so call
        // `apply_proxy_env` directly — otherwise disabling reqwest's
        // auto-detection would silently strip `HTTPS_PROXY` /
        // `HTTP_PROXY` support from every caller that uses
        // `RegistryClient::new` or `::default`.
        let mut config = NpmConfig {
            registry: crate::config::normalize_registry_url_pub(registry_url),
            ..Default::default()
        };
        config.apply_proxy_env();
        Self::from_config(config)
    }

    /// Build a client with the default [`FetchPolicy`]. Callers that
    /// have already resolved a [`ResolveCtx`] should prefer
    /// [`Self::from_config_with_policy`] so env / workspace-yaml /
    /// `.npmrc` overrides to the `fetch*` settings take effect.
    pub fn from_config(config: NpmConfig) -> Self {
        Self::from_config_with_policy(config, FetchPolicy::default())
    }

    /// Build a client with an explicit [`FetchPolicy`]. This is the
    /// primary constructor used by `aube::commands::make_client`,
    /// which resolves the policy from the full settings precedence
    /// chain before calling in.
    pub fn from_config_with_policy(config: NpmConfig, fetch_policy: FetchPolicy) -> Self {
        let http = build_http_client(&config, None, &fetch_policy);
        let mut http_by_uri = BTreeMap::new();
        for (uri, registry) in &config.auth_by_uri {
            if registry.tls.ca.is_empty()
                && registry.tls.cafile.is_none()
                && registry.tls.cert.is_none()
                && registry.tls.key.is_none()
            {
                continue;
            }
            http_by_uri.insert(
                uri.clone(),
                build_http_client(&config, Some(registry), &fetch_policy),
            );
        }

        Self {
            http,
            http_by_uri,
            token_helper_cache: Mutex::new(BTreeMap::new()),
            auth_token_by_url: Mutex::new(BTreeMap::new()),
            config,
            network_mode: NetworkMode::Online,
            fetch_policy,
        }
    }

    /// Force this client into a given network mode (online, prefer-offline,
    /// offline). Consumed by `install` when the user passes `--offline` or
    /// `--prefer-offline`.
    pub fn with_network_mode(mut self, mode: NetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    pub fn network_mode(&self) -> NetworkMode {
        self.network_mode
    }

    pub fn uses_default_npm_registry_for(&self, name: &str) -> bool {
        self.registry_url_for(name).trim_end_matches('/') == "https://registry.npmjs.org"
    }

    pub fn cached_packument_lookup(&self, name: &str, cache_dir: &Path) -> CachedPackumentLookup {
        let registry_url = self.config.registry_for(name).to_string();
        let Some(cache_path) = packument_cache_path(cache_dir, name, &registry_url) else {
            return CachedPackumentLookup::default();
        };
        let Some(cached) = read_cached_packument(&cache_path) else {
            return CachedPackumentLookup::default();
        };
        if self.trust_cached_packument(cached.fetched_at, cached.max_age_secs) {
            return CachedPackumentLookup {
                packument: Some(cached.packument),
                stale: false,
                cached: None,
            };
        }
        CachedPackumentLookup {
            packument: None,
            stale: true,
            cached: Some(CachedPackumentLookupEntry::Abbreviated(cached)),
        }
    }

    pub fn cached_full_packument_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> CachedPackumentLookup {
        let registry_url = self.config.registry_for(name).to_string();
        let Some(cache_path) = packument_full_cache_path(cache_dir, name, &registry_url) else {
            return CachedPackumentLookup::default();
        };
        read_cached_full_packument_typed_lookup(&cache_path, self.force_cache())
    }

    pub fn seed_packument_cache(
        &self,
        name: &str,
        cache_dir: &Path,
        packument: &Packument,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fresh: bool,
    ) {
        let registry_url = self.config.registry_for(name);
        let Some(cache_path) = packument_cache_path(cache_dir, name, registry_url) else {
            return;
        };
        if cache_path.exists() {
            return;
        }
        let cached = CachedPackument {
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            fetched_at: if fresh { now_secs() } else { 0 },
            max_age_secs: (!fresh).then_some(0),
            packument: packument.clone(),
        };
        if let Err(e) = write_cached_packument(&cache_path, &cached) {
            tracing::debug!(
                "failed to seed packument cache {} from bundled primer: {e}",
                cache_path.display()
            );
        }
    }

    pub fn seed_full_packument_cache(
        &self,
        name: &str,
        cache_dir: &Path,
        packument: &Packument,
        etag: Option<&str>,
        last_modified: Option<&str>,
        fresh: bool,
    ) {
        let registry_url = self.config.registry_for(name);
        let Some(cache_path) = packument_full_cache_path(cache_dir, name, registry_url) else {
            return;
        };
        if cache_path.exists() {
            return;
        }
        let Ok(packument) = serde_json::to_value(packument) else {
            return;
        };
        let cached = CachedFullPackument {
            etag: etag.map(str::to_owned),
            last_modified: last_modified.map(str::to_owned),
            fetched_at: if fresh { now_secs() } else { 0 },
            max_age_secs: (!fresh).then_some(0),
            packument,
        };
        if let Err(e) = write_cached_full_packument(&cache_path, &cached) {
            tracing::debug!(
                "failed to seed full packument cache {} from bundled primer: {e}",
                cache_path.display()
            );
        }
    }

    /// Get the registry URL for a given package name (respects scoped registries).
    fn registry_url_for(&self, name: &str) -> &str {
        self.config.registry_for(name)
    }

    fn force_cache(&self) -> bool {
        matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        )
    }

    fn trust_cached_packument(&self, fetched_at: u64, max_age_secs: Option<u64>) -> bool {
        self.force_cache() || cached_is_fresh(fetched_at, max_age_secs)
    }

    /// Build `{registry}/{encoded_name}` — the packument route. Scoped
    /// packages have their `/` encoded as `%2F` so intermediate proxies
    /// that route on path segments (Artifactory's npm remote is the
    /// known offender) don't reject the request with 406. npm-cli and
    /// pnpm encode the same way.
    fn packument_url(&self, name: &str) -> (String, &str) {
        let registry_url = self.registry_url_for(name);
        let url = format!(
            "{}/{}",
            registry_url.trim_end_matches('/'),
            encoded_name(name),
        );
        (url, registry_url)
    }

    /// Build a GET request with auth headers for the given registry URL.
    fn authed_get(&self, url: &str, registry_url: &str) -> reqwest::RequestBuilder {
        self.authed_request(reqwest::Method::GET, url, registry_url)
    }

    /// Build an HTTP request using this registry's configured TLS client
    /// and auth fallback order: bearer token, tokenHelper, then basic auth.
    pub fn authed_request(
        &self,
        method: reqwest::Method,
        url: &str,
        registry_url: &str,
    ) -> reqwest::RequestBuilder {
        self.authed(
            self.http_for(registry_url).request(method, url),
            registry_url,
        )
    }

    pub fn has_resolved_auth_for(&self, registry_url: &str) -> bool {
        self.registry_auth_token_for(registry_url).is_some()
            || self.config.basic_auth_for(registry_url).is_some()
            || self.config.global_auth_token.is_some()
    }

    /// Attach auth headers to any `RequestBuilder` keyed off the registry
    /// that owns `registry_url`. Shared between the GET helpers and the
    /// dist-tag / deprecate PUT calls so every write request picks up the
    /// same token/basic-auth resolution as reads. Future token-type
    /// changes (e.g. web-flow refresh) only have to be made here.
    fn authed(&self, req: reqwest::RequestBuilder, registry_url: &str) -> reqwest::RequestBuilder {
        if let Some(token) = self.registry_auth_token_for(registry_url) {
            req.bearer_auth(token)
        } else if let Some(auth) = self.config.basic_auth_for(registry_url) {
            req.header("Authorization", format!("Basic {auth}"))
        } else if let Some(token) = self.config.global_auth_token.as_ref()
            && same_host(&self.config.registry, registry_url)
        {
            // Only send the default _authToken when the request hits the
            // default registry. Stops a malicious scoped registry or a
            // packument with a dist.tarball pointing at attacker.example
            // from grabbing the user's npmjs token.
            req.bearer_auth(token)
        } else {
            req
        }
    }

    fn registry_auth_token_for(&self, registry_url: &str) -> Option<String> {
        // Fast path: memoized result. Hit on the second-and-later
        // request to the same registry URL within one process.
        if let Ok(cache) = self.auth_token_by_url.lock()
            && let Some(cached) = cache.get(registry_url)
        {
            return cached.clone();
        }
        let resolved = if let Some(auth) = self.config.registry_config_for(registry_url) {
            if let Some(token) = auth.auth_token.as_ref() {
                Some(token.to_string())
            } else if let Some(helper) = auth.token_helper.as_deref() {
                self.cached_token_helper_result(helper)
            } else {
                None
            }
        } else {
            None
        };
        if let Ok(mut cache) = self.auth_token_by_url.lock() {
            cache.insert(registry_url.to_string(), resolved.clone());
        }
        resolved
    }

    /// Cache key is the helper command itself, not the registry URL:
    /// `run_token_helper` spawns the helper as a subprocess that returns
    /// a token determined entirely by the command, with no URL input.
    /// Keying by URL would defeat the cache for tarball fetches (each
    /// tarball has a unique path) and re-spawn the helper hundreds of
    /// times during a large install.
    fn cached_token_helper_result(&self, helper: &str) -> Option<String> {
        {
            let cache = self.token_helper_cache.lock().ok()?;
            if let Some(token) = cache.get(helper) {
                return token.clone();
            }
        }
        let token = crate::config::run_token_helper(helper);
        if let Ok(mut cache) = self.token_helper_cache.lock() {
            cache.insert(helper.to_string(), token.clone());
        }
        token
    }

    fn http_for(&self, registry_url: &str) -> &reqwest::Client {
        let uri_key = crate::config::registry_uri_key_pub(registry_url);
        crate::config::lookup_by_uri_prefix(&self.http_by_uri, &uri_key).unwrap_or(&self.http)
    }

    /// Same as [`Self::send_with_retry`] but also returns wall-clock
    /// elapsed from the first `.send()` to the returned response. Used
    /// by metadata call sites to compare against `fetchWarnTimeoutMs`
    /// without double-timing the retry backoff from caller code.
    async fn send_with_retry_timed<F>(
        &self,
        build: F,
    ) -> Result<(reqwest::Response, std::time::Duration), reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let started = std::time::Instant::now();
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    // Retry on 5xx server errors and 429 rate-limit.
                    // Everything else — 2xx/3xx successes and 4xx
                    // client errors the caller needs to see (404,
                    // 401, 403) — is returned verbatim.
                    if !is_retriable_status(status) || is_last {
                        return Ok((resp, started.elapsed()));
                    }
                    // 429 may carry a `Retry-After` header; honor it
                    // (seconds form) so a rate-limited registry gets
                    // the wait it asked for instead of our default
                    // exponential backoff. `make-fetch-happen` does
                    // the same. HTTP-date form is rare for npm and
                    // `chrono` isn't a dep — parse as u64 seconds or
                    // fall back to the computed backoff.
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    drop(resp);
                    // Surfaces at WARN so users see retry activity in
                    // the install output. The final failure still
                    // propagates up as a user-facing error if every
                    // attempt fails.
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = status.as_u16(),
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    if is_last {
                        return Err(e);
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %e,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
            }
        }
        // `FetchPolicy::retries` is `u32`, so `max_attempts =
        // retries + 1` is always ≥ 1 and the loop runs at least once;
        // every path inside the loop either returns or continues. An
        // exit past this point is a structural bug, not a runtime
        // input the caller can provoke.
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Metadata-request wrapper around [`Self::send_with_retry_timed`]
    /// that emits the `fetchWarnTimeoutMs` warning when total
    /// wall-clock (including any retry backoff) exceeds the configured
    /// threshold. `0` disables the warning, matching pnpm's
    /// convention and the default in `settings.toml`.
    ///
    /// `label` is the logical resource being fetched (e.g.
    /// `"packument lodash"`), used in the warn message so an operator
    /// can map a slow fetch back to a package name without re-enabling
    /// debug tracing.
    ///
    /// Not used by tarball downloads — `fetchMinSpeedKiBps` is the
    /// tarball-side observability knob, and the two warnings are
    /// semantically distinct (headers latency vs. body throughput).
    async fn send_metadata_with_retry<F>(
        &self,
        label: &str,
        build: F,
    ) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let (resp, elapsed) = self.send_with_retry_timed(build).await?;
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = elapsed.as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            tracing::warn!(
                elapsed_ms,
                threshold_ms = threshold,
                label,
                "slow registry metadata request exceeded fetchWarnTimeoutMs",
            );
        }
        Ok(resp)
    }

    fn maybe_warn_slow_metadata(&self, label: &str, started: std::time::Instant) {
        let threshold = self.fetch_policy.warn_timeout_ms;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if threshold > 0 && elapsed_ms > threshold {
            tracing::warn!(
                elapsed_ms,
                threshold_ms = threshold,
                label,
                "slow registry metadata request exceeded fetchWarnTimeoutMs",
            );
        }
    }

    async fn retry_bytes_body_read<F>(
        &self,
        label: &str,
        cap: u64,
        build: F,
    ) -> Result<(bytes::Bytes, std::time::Duration), Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        // Counted independently of `attempt` so a non-timeout failure
        // (e.g. a 503 on the first try) doesn't consume the timeout
        // budget. Increments only when a retry follows a timeout.
        let mut timeout_retries: u32 = 0;
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match build().send().await {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, cap, label)?;
                    let started = std::time::Instant::now();
                    match read_body_capped(resp, cap, label).await {
                        Ok(bytes) => return Ok((bytes, started.elapsed())),
                        Err(err) if !is_last => {
                            let is_timeout = matches!(&err, Error::Http(e) if e.is_timeout());
                            if is_timeout && timeout_retries >= TIMEOUT_RETRY_CAP {
                                return Err(err);
                            }
                            if is_timeout {
                                timeout_retries += 1;
                            }
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                "retrying HTTP request after response body read error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    if err.is_timeout() {
                        if timeout_retries >= TIMEOUT_RETRY_CAP {
                            return Err(Error::Http(err));
                        }
                        timeout_retries += 1;
                    }
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(Error::Http(err)),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch the *full* (non-corgi) packument for a package as raw JSON
    /// with disk caching + ETag revalidation, mirroring
    /// [`Self::fetch_packument_cached`]. Returns `serde_json::Value` so
    /// fields the resolver doesn't parse (`description`, `homepage`,
    /// `repository`, `license`, `keywords`, `maintainers`, `time`,
    /// `readme`, ...) are preserved for human-facing commands like
    /// `aube view`.
    ///
    /// Behavior:
    ///   - If a cached entry exists and is younger than `PACKUMENT_TTL_SECS`,
    ///     return it immediately (no network).
    ///   - Otherwise, send a conditional request with `If-None-Match` /
    ///     `If-Modified-Since`. On 304, refresh the cache timestamp and
    ///     return the cached body.
    ///   - On 200, write the new packument to disk.
    pub async fn fetch_packument_full_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<serde_json::Value, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = read_cached_full_packument(&cache_path);

        // --prefer-offline / --offline: trust any cached copy regardless of age.
        // --offline additionally forbids falling back to the network on a miss.
        let force_cache = self.force_cache();
        if let Some(c) = cached.as_ref()
            && (force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs))
        {
            return Ok(cached.unwrap().packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        let (url, registry_url) = self.packument_url(name);
        let started = std::time::Instant::now();

        // Rebuild the conditional request on each retry. Held in a
        // closure so the revalidation headers are consistent across
        // attempts — a 503 retry with stale `If-None-Match` would be
        // a caching bug.
        let cached_ref = cached.as_ref();
        let label = format!("packument {name}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self
                    .authed_get(&url, registry_url)
                    .header("Accept", PACKUMENT_FULL_ACCEPT);
                if let Some(c) = cached_ref {
                    if let Some(ref etag) = c.etag {
                        req = req.header("If-None-Match", etag);
                    }
                    if let Some(ref lm) = c.last_modified {
                        req = req.header("If-Modified-Since", lm);
                    }
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    if resp.status() == reqwest::StatusCode::NOT_FOUND {
                        self.maybe_warn_slow_metadata(&label, started);
                        return Err(Error::NotFound(name.to_string()));
                    }

                    if resp.status() == reqwest::StatusCode::NOT_MODIFIED
                        && let Some(c) = cached.as_ref()
                    {
                        let revalidated_max_age =
                            parse_cache_control_max_age(&resp).or(c.max_age_secs);
                        let to_cache = CachedFullPackument {
                            etag: c.etag.clone(),
                            last_modified: c.last_modified.clone(),
                            fetched_at: now_secs(),
                            max_age_secs: revalidated_max_age,
                            packument: c.packument.clone(),
                        };
                        if let Err(e) = write_cached_full_packument(&cache_path, &to_cache) {
                            tracing::warn!(
                                "failed to write packument cache {}: {e}",
                                cache_path.display()
                            );
                        }
                        self.maybe_warn_slow_metadata(&label, started);
                        return Ok(c.packument.clone());
                    }

                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<serde_json::Value>(resp).await {
                        Ok(packument) => {
                            let to_cache = CachedFullPackument {
                                etag,
                                last_modified,
                                fetched_at: now_secs(),
                                max_age_secs,
                                packument: packument.clone(),
                            };
                            if let Err(e) = write_cached_full_packument(&cache_path, &to_cache) {
                                tracing::warn!(
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            self.maybe_warn_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                "retrying HTTP request after response body decode error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch the full (non-corgi) packument for a package and parse it
    /// into [`Packument`]. Unlike [`Self::fetch_packument_cached`], the
    /// result includes the `time` map — needed for
    /// `--resolution-mode=time-based`. Shares on-disk cache layout with
    /// [`Self::fetch_packument_full_cached`] so callers pay one network
    /// fetch for both the `aube view`-style full JSON and the time map.
    ///
    /// Hot path on warm cache: reads the cache file once and uses
    /// `simd_json` to deserialize the wrapper directly into the typed
    /// [`Packument`] shape in a single pass. This avoids the older
    /// `serde_json::Value` + `serde_json::from_value` round-trip, which
    /// walked the cached JSON twice on every resolver read.
    pub async fn fetch_packument_with_time_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<Packument, Error> {
        // Fast path: try the warm-cache read first. Matches the
        // freshness window logic in `fetch_packument_full_cached`
        // exactly so the two APIs share revalidation behavior.
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let force_cache = self.force_cache();
        if let Some(packument) = read_cached_full_packument_typed(&cache_path, force_cache) {
            return Ok(packument);
        }

        // Slow path: full value round-trip covers revalidation + fresh
        // network fetches + all the ETag bookkeeping.
        // `fetch_packument_full_cached` is the single source of truth
        // for those branches; we just re-parse its `Value` into
        // `Packument` here. The one `from_value` walk this still pays
        // is amortized across the network round-trip so it doesn't
        // show up in steady-state resolves.
        let value = self.fetch_packument_full_cached(name, cache_dir).await?;
        let packument: Packument = serde_json::from_value(value)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        Ok(packument)
    }

    pub async fn fetch_packument_with_time_cached_after_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
        lookup: CachedPackumentLookup,
    ) -> Result<Packument, Error> {
        match lookup.cached {
            Some(CachedPackumentLookupEntry::Full(cached)) => {
                self.revalidate_full_packument_typed(name, cache_dir, cached)
                    .await
            }
            _ => self.fetch_packument_with_time_cached(name, cache_dir).await,
        }
    }

    async fn revalidate_full_packument_typed(
        &self,
        name: &str,
        cache_dir: &Path,
        cached: CachedFullPackumentTyped,
    ) -> Result<Packument, Error> {
        let force_cache = self.force_cache();
        if force_cache || cached_is_fresh(cached.fetched_at, cached.max_age_secs) {
            return Ok(cached.packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_full_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let (url, registry_url) = self.packument_url(name);
        let label = format!("packument {name}");
        let started = std::time::Instant::now();
        let max_attempts = self.fetch_policy.retries.saturating_add(1);

        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self
                    .authed_get(&url, registry_url)
                    .header("Accept", PACKUMENT_FULL_ACCEPT);
                if let Some(ref etag) = cached.etag {
                    req = req.header("If-None-Match", etag);
                }
                if let Some(ref lm) = cached.last_modified {
                    req = req.header("If-Modified-Since", lm);
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_warn_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_MODIFIED => {
                    let revalidated_max_age =
                        parse_cache_control_max_age(&resp).or(cached.max_age_secs);
                    let mut to_cache = if let Some(to_cache) =
                        read_cached_full_packument(&cache_path)
                    {
                        to_cache
                    } else {
                        let packument = serde_json::to_value(&cached.packument).map_err(|e| {
                            Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
                        })?;
                        CachedFullPackument {
                            etag: cached.etag.clone(),
                            last_modified: cached.last_modified.clone(),
                            fetched_at: cached.fetched_at,
                            max_age_secs: cached.max_age_secs,
                            packument,
                        }
                    };
                    to_cache.fetched_at = now_secs();
                    to_cache.max_age_secs = revalidated_max_age;
                    if let Err(e) = write_cached_full_packument(&cache_path, &to_cache) {
                        tracing::warn!(
                            "failed to write packument cache {}: {e}",
                            cache_path.display()
                        );
                    }
                    self.maybe_warn_slow_metadata(&label, started);
                    return Ok(cached.packument);
                }
                Ok(resp) => {
                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);
                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<serde_json::Value>(resp).await {
                        Ok(value) => {
                            let to_cache = CachedFullPackument {
                                etag,
                                last_modified,
                                fetched_at: now_secs(),
                                max_age_secs,
                                packument: value.clone(),
                            };
                            if let Err(e) = write_cached_full_packument(&cache_path, &to_cache) {
                                tracing::warn!(
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            let packument: Packument =
                                serde_json::from_value(value).map_err(|e| {
                                    Error::Io(std::io::Error::new(
                                        std::io::ErrorKind::InvalidData,
                                        e,
                                    ))
                                })?;
                            self.maybe_warn_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                "retrying HTTP request after response body decode error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        "retrying HTTP request after transport error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch the abbreviated packument for a package (corgi format).
    pub async fn fetch_packument(&self, name: &str) -> Result<Packument, Error> {
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }
        let (url, registry_url) = self.packument_url(name);
        let label = format!("packument {name}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let started = std::time::Instant::now();
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let req = self.authed_get(&url, registry_url);
                if force_full_packument() {
                    req
                } else {
                    req.header("Accept", PACKUMENT_ACCEPT)
                }
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_warn_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp) => {
                    match parse_full_response::<Packument>(resp.error_for_status()?).await {
                        Ok(packument) => {
                            self.maybe_warn_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                "retrying HTTP request after response body decode error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        "retrying HTTP request after response body decode error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// Fetch a packument using a disk-backed cache:
    ///   - If a cached entry exists and is younger than PACKUMENT_TTL_SECS, return it
    ///     immediately (no network).
    ///   - Otherwise, send a conditional request with If-None-Match/If-Modified-Since.
    ///     On 304, refresh the cache timestamp and return the cached body.
    ///   - On 200, write the new packument to disk.
    pub async fn fetch_packument_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<Packument, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = read_cached_packument(&cache_path);
        self.fetch_packument_cached_with_entry(name, cache_path, cached)
            .await
    }

    pub async fn fetch_packument_cached_after_lookup(
        &self,
        name: &str,
        cache_dir: &Path,
        lookup: CachedPackumentLookup,
    ) -> Result<Packument, Error> {
        let registry_url = self.config.registry_for(name).to_string();
        let cache_path = packument_cache_path(cache_dir, name, &registry_url)
            .ok_or_else(|| Error::InvalidName(name.to_string()))?;
        let cached = match lookup.cached {
            Some(CachedPackumentLookupEntry::Abbreviated(cached)) => Some(cached),
            _ => read_cached_packument(&cache_path),
        };
        self.fetch_packument_cached_with_entry(name, cache_path, cached)
            .await
    }

    async fn fetch_packument_cached_with_entry(
        &self,
        name: &str,
        cache_path: PathBuf,
        cached: Option<CachedPackument>,
    ) -> Result<Packument, Error> {
        // Fast path: trust the cache if it's still fresh.
        // Move out of the wrapper to avoid cloning the Packument.
        // --prefer-offline / --offline extend "fresh" to "any cached entry"
        // so we skip revalidation and, for --offline, the network entirely.
        let force_cache = self.force_cache();
        if let Some(c) = cached.as_ref()
            && (force_cache || cached_is_fresh(c.fetched_at, c.max_age_secs))
        {
            return Ok(cached.unwrap().packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        let (url, registry_url) = self.packument_url(name);

        // Normally we ask for the abbreviated (corgi) response so we
        // get a smaller payload. See `force_full_packument()` for why
        // this escape hatch exists — it is strictly a BATS/fixture
        // workaround, never a user-facing tunable.
        //
        // Revalidation headers are rebuilt per attempt (same contract
        // as `fetch_packument_full_cached`) so retries on 503 keep
        // using the correct `If-None-Match` / `If-Modified-Since`
        // without silently stripping cache hints.
        let cached_ref = cached.as_ref();
        let label = format!("packument {name}");
        let max_attempts = self.fetch_policy.retries.saturating_add(1);
        let started = std::time::Instant::now();
        for attempt in 0..max_attempts {
            let is_last = attempt + 1 >= max_attempts;
            match {
                let mut req = self.authed_get(&url, registry_url);
                if !force_full_packument() {
                    req = req.header("Accept", PACKUMENT_ACCEPT);
                }
                if let Some(c) = cached_ref {
                    if let Some(ref etag) = c.etag {
                        req = req.header("If-None-Match", etag);
                    }
                    if let Some(ref lm) = c.last_modified {
                        req = req.header("If-Modified-Since", lm);
                    }
                }
                req
            }
            .send()
            .await
            {
                Ok(resp) if is_retriable_status(resp.status()) && !is_last => {
                    let wait = retry_after_from(&resp)
                        .unwrap_or_else(|| self.fetch_policy.backoff_for_attempt(attempt + 1));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        status = resp.status().as_u16(),
                        label,
                        "retrying HTTP request after transient failure",
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                    self.maybe_warn_slow_metadata(&label, started);
                    return Err(Error::NotFound(name.to_string()));
                }
                Ok(resp)
                    if resp.status() == reqwest::StatusCode::NOT_MODIFIED && cached.is_some() =>
                {
                    let c = cached.as_ref().unwrap();
                    let revalidated_max_age = parse_cache_control_max_age(&resp).or(c.max_age_secs);
                    let to_cache = CachedPackument {
                        etag: c.etag.clone(),
                        last_modified: c.last_modified.clone(),
                        fetched_at: now_secs(),
                        max_age_secs: revalidated_max_age,
                        packument: c.packument.clone(),
                    };
                    if let Err(e) = write_cached_packument(&cache_path, &to_cache) {
                        tracing::warn!(
                            "failed to write packument cache {}: {e}",
                            cache_path.display()
                        );
                    }
                    self.maybe_warn_slow_metadata(&label, started);
                    return Ok(c.packument.clone());
                }
                Ok(resp) => {
                    let (etag, last_modified) = extract_cache_headers(&resp);
                    let max_age_secs = parse_cache_control_max_age(&resp);

                    let resp = resp.error_for_status()?;
                    check_body_cap(&resp, self.fetch_policy.packument_max_bytes, &label)?;
                    match parse_full_response::<Packument>(resp).await {
                        Ok(packument) => {
                            let to_cache = CachedPackument {
                                etag,
                                last_modified,
                                fetched_at: now_secs(),
                                max_age_secs,
                                packument: packument.clone(),
                            };
                            if let Err(e) = write_cached_packument(&cache_path, &to_cache) {
                                tracing::warn!(
                                    "failed to write packument cache {}: {e}",
                                    cache_path.display()
                                );
                            }
                            self.maybe_warn_slow_metadata(&label, started);
                            return Ok(packument);
                        }
                        Err(err) if !is_last => {
                            let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                            tracing::warn!(
                                attempt = attempt + 1,
                                max_attempts,
                                backoff_ms = wait.as_millis() as u64,
                                error = %err,
                                label,
                                "retrying HTTP request after response body decode error",
                            );
                            tokio::time::sleep(wait).await;
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(err) if !is_last => {
                    let wait = self.fetch_policy.backoff_for_attempt(attempt + 1);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        label,
                        "retrying HTTP request after response body decode error",
                    );
                    tokio::time::sleep(wait).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        unreachable!("retry loop exited without returning; max_attempts was {max_attempts}")
    }

    /// POST to the npm bulk security advisories endpoint used by `npm audit`
    /// and `pnpm audit`: `{registry}/-/npm/v1/security/advisories/bulk`.
    ///
    /// `pkg_versions` maps package name to the list of installed versions to
    /// check. The response is a map keyed by package name whose values are
    /// arrays of advisory objects; this function returns the raw JSON so the
    /// caller decides which fields to render (pnpm-compat: id, url, title,
    /// severity, vulnerable_versions, cwe, cvss, ...).
    pub async fn fetch_advisories_bulk(
        &self,
        pkg_versions: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<serde_json::Value, Error> {
        // The bulk endpoint lives on the default registry; scoped registries
        // don't all implement it, so we always post to the top-level one.
        let registry_url = &self.config.registry;
        let url = format!(
            "{}/-/npm/v1/security/advisories/bulk",
            registry_url.trim_end_matches('/')
        );

        let body = serde_json::to_vec(pkg_versions)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;

        let resp = self
            .authed(self.http_for(registry_url).post(&url), registry_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await?;

        // Some registries (Verdaccio, private mirrors) don't implement the
        // bulk advisory endpoint and return 404. Treat that as "no advisories"
        // — the alternative is making every air-gapped setup pass
        // `--ignore-registry-errors`, which is noisy.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }

        let resp = resp.error_for_status()?;
        check_body_cap(&resp, AUDIT_BODY_CAP, "bulk advisories")?;
        let json: serde_json::Value = resp.json().await?;
        Ok(json)
    }

    /// Download a tarball and return the bytes.
    ///
    /// Emits a `fetchMinSpeedKiBps` warning when the end-to-end average
    /// throughput of the body read falls below the configured threshold.
    /// Average (not instantaneous) speed because the call path is a
    /// single `resp.bytes().await?` — we keep the eager-read model and
    /// still give operators a signal for flaky links. `fetchWarnTimeoutMs`
    /// does *not* fire here: that one is scoped to metadata requests
    /// per its pnpm documentation, and the tarball-specific analogue
    /// is the min-speed warning.
    pub async fn fetch_tarball_bytes(&self, url: &str) -> Result<bytes::Bytes, Error> {
        // Refuse non-http(s) tarball URLs at the aube boundary so
        // attacker-controlled `dist.tarball` from a hostile mirror
        // cannot reach `file:///` (local file disclosure) or the
        // ssh / git transports inside reqwest. Belt-and-suspenders
        // against transport-layer regressions.
        let safe_url = aube_util::url::redact_url(url);
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| Error::Io(std::io::Error::other(format!("invalid tarball url: {e}"))))?;
        match parsed.scheme() {
            "https" | "http" => {}
            scheme => {
                return Err(Error::Io(std::io::Error::other(format!(
                    "tarball {safe_url}: refusing scheme {scheme:?}",
                ))));
            }
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("tarball {safe_url}")));
        }
        // Tarball URLs may point to any registry, try to match auth.
        // Pass the full tarball URL through so longest-prefix matching
        // in `registry_config_for` can find path-scoped auth entries
        // (e.g. `//host/artifactory/npm/`). Tarballs are already gzip
        // archives, so ask intermediaries not to wrap them in HTTP
        // content encoding that can fail independently of the payload.
        // Retries cover transient 5xx / 429 / connection errors; see
        // [`Self::send_with_retry`].
        let (bytes, body_elapsed) = self
            .retry_bytes_body_read(url, self.fetch_policy.tarball_max_bytes, || {
                self.authed_get(url, url)
                    .header(reqwest::header::ACCEPT_ENCODING, "identity")
            })
            .await?;
        warn_slow_tarball(
            self.fetch_policy.min_speed_kibps,
            url,
            bytes.len(),
            body_elapsed,
        );
        Ok(bytes)
    }

    /// Fetch the *full* (non-corgi) packument as raw JSON, bypassing the
    /// on-disk cache entirely. Used by mutating commands like `deprecate`
    /// that need a fresh read-modify-write against the authoritative copy
    /// on the registry — a stale cached document would roll back other
    /// publishers' changes on the subsequent PUT.
    pub async fn fetch_packument_json_fresh(&self, name: &str) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);
        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
                self.authed_get(&url, registry_url)
                    .header("Accept", PACKUMENT_FULL_ACCEPT)
            })
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }
        let resp = resp.error_for_status()?;
        check_body_cap(&resp, self.fetch_policy.packument_max_bytes, "packument")?;
        let value: serde_json::Value = resp.json().await?;
        Ok(value)
    }

    /// PUT a full packument back to the registry. Used by `deprecate` /
    /// `undeprecate`. Honors `--otp` via the `npm-otp` header.
    ///
    /// Returns the registry's raw response body as `serde_json::Value`
    /// (npm responds with `{ok: true, id, rev}` on success). On HTTP
    /// failure the body is included in the error so 401/403/409 messages
    /// make it to the user.
    pub async fn put_packument(
        &self,
        name: &str,
        body: &serde_json::Value,
        otp: Option<&str>,
    ) -> Result<serde_json::Value, Error> {
        let (url, registry_url) = self.packument_url(name);

        let mut req = self.authed(
            self.http_for(registry_url)
                .put(&url)
                .header("Content-Type", "application/json")
                .json(body),
            registry_url,
        );
        if let Some(code) = otp {
            req = req.header("npm-otp", code);
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::RegistryWrite {
                status: status.as_u16(),
                body,
            });
        }
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
        Ok(value)
    }

    /// Drop any on-disk *full* packument cache entry for `name`, if one
    /// exists. Call this after a successful mutating PUT (deprecate,
    /// dist-tag, ...) so subsequent `aube view` calls don't serve the
    /// pre-mutation document for the remaining TTL window. Missing files
    /// and I/O errors are swallowed — the cache is advisory, not load
    /// bearing.
    pub fn invalidate_full_packument_cache(&self, name: &str, cache_dir: &Path) {
        let registry_url = self.config.registry_for(name).to_string();
        if let Some(path) = packument_full_cache_path(cache_dir, name, &registry_url) {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Fetch the authoritative dist-tag map for a package from the
    /// registry's `/-/package/<pkg>/dist-tags` endpoint. This is the
    /// same endpoint `npm dist-tag ls` calls. A GET against this
    /// endpoint doesn't require auth for public packages, but we still
    /// attach the user's token so private packages Just Work.
    pub async fn fetch_dist_tags(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_root_url(registry_url, name);
        let resp = self
            .send_metadata_with_retry(&format!("dist-tags {name}"), || {
                self.authed_get(&url, registry_url)
            })
            .await?;
        check_dist_tag_status(&resp, name)?;
        let map: std::collections::BTreeMap<String, String> =
            resp.error_for_status()?.json().await?;
        Ok(map)
    }

    /// Create or update a dist-tag for a package. The npm registry
    /// expects a PUT with a JSON-string body — e.g. `"1.2.3"`, *with*
    /// the quotes — and Content-Type: application/json. Requires auth.
    pub async fn put_dist_tag(&self, name: &str, tag: &str, version: &str) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);

        // serde_json is already a workspace dep and used elsewhere in
        // this file; hand-serializing would miss control-character
        // escapes and other edge cases. The output is always a JSON
        // string literal like `"1.2.3"`.
        let body = serde_json::to_string(version).map_err(std::io::Error::other)?;

        let req = self
            .http_for(registry_url)
            .put(&url)
            .header("Content-Type", "application/json")
            .body(body);
        let resp = self.authed(req, registry_url).send().await?;
        check_dist_tag_status(&resp, name)?;
        resp.error_for_status()?;
        Ok(())
    }

    /// Remove a dist-tag from a package. Registry DELETE against
    /// `/-/package/<pkg>/dist-tags/<tag>`. Requires auth.
    pub async fn delete_dist_tag(&self, name: &str, tag: &str) -> Result<(), Error> {
        let registry_url = self.registry_url_for(name);
        let url = dist_tag_url(registry_url, name, tag);
        let req = self.http_for(registry_url).delete(&url);
        let resp = self.authed(req, registry_url).send().await?;
        // 404 here is ambiguous: package doesn't exist vs tag doesn't
        // exist on this package. Surface the `name@tag` form so the
        // caller can render it either way.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(format!("{name}@{tag}")));
        }
        if matches!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(Error::Unauthorized);
        }
        resp.error_for_status()?;
        Ok(())
    }

    /// Construct the tarball URL for a package from the registry.
    /// Format: {registry}/{name}/-/{unscoped_name}-{version}.tgz
    pub fn tarball_url(&self, name: &str, version: &str) -> String {
        let registry_url = self.registry_url_for(name);
        let registry = registry_url.trim_end_matches('/');
        let unscoped = if let Some(rest) = name.strip_prefix('@') {
            // @scope/pkg -> pkg
            rest.split('/').nth(1).unwrap_or(rest)
        } else {
            name
        };
        format!("{registry}/{name}/-/{unscoped}-{version}.tgz")
    }
}

impl Default for RegistryClient {
    fn default() -> Self {
        Self::new("https://registry.npmjs.org")
    }
}

fn same_host(a: &str, b: &str) -> bool {
    let Ok(a) = reqwest::Url::parse(a) else {
        return false;
    };
    let Ok(b) = reqwest::Url::parse(b) else {
        return false;
    };
    // Scheme comparison matters. An http://registry.example and an
    // https://registry.example would otherwise look identical here,
    // and a user who configured their registry over http would ship
    // the default _authToken in cleartext. The redirect policy already
    // blocks https to http downgrade on live requests, but only
    // scheme parity at this gate prevents an explicitly http-
    // configured registry from bypassing the check at request time.
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

fn build_http_client(
    config: &NpmConfig,
    registry_config: Option<&crate::config::AuthConfig>,
    fetch_policy: &FetchPolicy,
) -> reqwest::Client {
    // `maxsockets` (when set) overrides the default pool size. pnpm
    // documents this as "concurrent connections per origin"; reqwest
    // doesn't expose a hard cap, but `pool_max_idle_per_host` is the
    // closest knob and is what downstream users actually care about.
    let pool_max_idle = config.max_sockets.unwrap_or(64);
    // CDN edge cache hit rate keys partly off the User-Agent header.
    // Hardcoded `0.1.0` lands in cold buckets on Cloudflare/Fastly. Use
    // the real workspace version + an OS/arch tail in the same shape
    // pnpm and npm send so the registry recognises us.
    static UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let user_agent = UA.get_or_init(|| {
        format!(
            "aube/{} ({} {})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    });
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        // Wire-level decompression for packument JSON. Tarball
        // requests explicitly send `Accept-Encoding: identity`
        // (tarballs are already gzip on the payload), so this only
        // affects metadata calls. Popular packuments (`react`,
        // `webpack`, `next`) drop 3-5x on the wire when gzipped.
        .gzip(true)
        .brotli(true)
        .zstd(true)
        // `fetchTimeout` — applied to the whole response (headers +
        // body) via reqwest's single-knob timeout. pnpm / npm expose
        // this as `fetch-timeout` in `.npmrc`; the default matches
        // npm's 60s. Without this override reqwest would use its
        // built-in 30s default, which is tighter than pnpm's.
        .timeout(std::time::Duration::from_millis(fetch_policy.timeout_ms))
        // Bigger connection pool so concurrent fetches don't queue on a small set of conns.
        // HTTP/2 (when negotiated via ALPN, which npm registry supports) multiplexes many
        // requests over a single connection so this mostly matters for fallback HTTP/1.1.
        .pool_max_idle_per_host(pool_max_idle)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .http2_keep_alive_interval(std::time::Duration::from_secs(30))
        .http2_keep_alive_timeout(std::time::Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
        .http2_adaptive_window(true)
        .http2_initial_stream_window_size(Some(16 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(16 * 1024 * 1024))
        .http2_max_frame_size(Some(16 * 1024 * 1024 - 1))
        .tcp_nodelay(true)
        .tcp_keepalive(std::time::Duration::from_secs(60))
        // In-process DNS caching via hickory-dns. The system resolver
        // does not cache and uses a thread pool for `getaddrinfo`,
        // which serializes the first cold lookup per origin. hickory
        // resolves async + caches for the process lifetime.
        .hickory_dns(true)
        // `strict-ssl=false` disables cert validation entirely. This
        // is a security hole on purpose: corporate registries should
        // prefer per-registry `ca` / `cafile` so validation stays on.
        .danger_accept_invalid_certs(!config.strict_ssl)
        // rustls already defaults to TLS 1.2+, but pinning the floor
        // here makes the policy explicit so a future default-loosening
        // upstream does not silently re-enable TLS 1.1 for aube.
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        // Block https to http downgrades on redirect. reqwest already
        // strips Authorization on cross-host redirects as of 0.12, so
        // this policy only adds the scheme guard. A 302 from a good
        // registry to `http://evil/` would otherwise leak whatever
        // header survived into cleartext.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if let Some(prev) = attempt.previous().last()
                && prev.scheme() == "https"
                && attempt.url().scheme() != "https"
            {
                return attempt.stop();
            }
            attempt.follow()
        }))
        // Disable reqwest's built-in `system-proxy` auto-detection
        // before installing any explicit proxies. Without this, the
        // builder would silently read `HTTP(S)_PROXY` / `NO_PROXY`
        // from the environment *on top of* the values we already
        // pulled into `NpmConfig`, so a `.npmrc` that overrides an
        // env-var proxy would be ignored for one scheme and honored
        // for the other, and `noproxy` bypasses would only apply to
        // the manually-configured proxies. `NpmConfig::load` now
        // folds the env vars into the config itself, so this crate
        // is the single source of truth for proxy state.
        .no_proxy();

    if let Some(ip) = config.local_address {
        builder = builder.local_address(Some(ip));
    }

    let no_proxy = config
        .no_proxy
        .as_deref()
        .and_then(reqwest::NoProxy::from_string);

    if let Some(ref url) = config.https_proxy {
        match reqwest::Proxy::https(url) {
            Ok(mut p) => {
                if let Some(ref np) = no_proxy {
                    p = p.no_proxy(Some(np.clone()));
                }
                builder = builder.proxy(p);
            }
            Err(e) => tracing::warn!("ignoring https-proxy {url:?}: {e}"),
        }
    }
    if let Some(ref url) = config.http_proxy {
        match reqwest::Proxy::http(url) {
            Ok(mut p) => {
                if let Some(ref np) = no_proxy {
                    p = p.no_proxy(Some(np.clone()));
                }
                builder = builder.proxy(p);
            }
            Err(e) => tracing::warn!("ignoring http-proxy {url:?}: {e}"),
        }
    }

    if let Some(registry_config) = registry_config {
        for ca in &registry_config.tls.ca {
            match reqwest::Certificate::from_pem(ca.as_bytes()) {
                Ok(cert) => builder = builder.add_root_certificate(cert),
                Err(e) => tracing::warn!("ignoring invalid per-registry ca: {e}"),
            }
        }
        if let Some(cafile) = &registry_config.tls.cafile {
            match std::fs::read(cafile) {
                Ok(bytes) => match reqwest::Certificate::from_pem_bundle(&bytes) {
                    Ok(certs) => {
                        for cert in certs {
                            builder = builder.add_root_certificate(cert);
                        }
                    }
                    Err(e) => tracing::warn!("ignoring invalid cafile {}: {e}", cafile.display()),
                },
                Err(e) => tracing::warn!("ignoring unreadable cafile {}: {e}", cafile.display()),
            }
        }
        if let (Some(cert), Some(key)) = (&registry_config.tls.cert, &registry_config.tls.key) {
            let mut pem = Vec::with_capacity(cert.len() + key.len() + 1);
            pem.extend_from_slice(cert.as_bytes());
            if !cert.ends_with('\n') {
                pem.push(b'\n');
            }
            pem.extend_from_slice(key.as_bytes());
            match reqwest::Identity::from_pem(&pem) {
                Ok(identity) => builder = builder.identity(identity),
                Err(e) => tracing::warn!("ignoring invalid per-registry client cert/key: {e}"),
            }
        }
    }

    builder.build().expect("failed to build HTTP client")
}

/// BATS-fixture escape hatch: ask the registry for the unabbreviated
/// packument instead of the corgi (`application/vnd.npm.install-v1+json`)
/// shape. Our Verdaccio-backed fixture strips `bundledDependencies`
/// when it projects stored packuments to corgi, so the
/// `test/bundled_dependencies.bats` suite sets this to exercise the
/// resolver's bundled-skip path end-to-end. Production registries
/// include `bundleDependencies` in corgi per the npm spec, so the
/// default path stays cheap.
///
/// The name is deliberately `AUBE_INTERNAL_*` so nothing outside the
/// test harness grows a habit of relying on it, and we require the
/// exact literal `"1"` (not just any non-empty value) so an inherited
/// or accidentally-set empty value won't silently balloon registry
/// traffic on end-user machines.
fn force_full_packument() -> bool {
    std::env::var("AUBE_INTERNAL_FORCE_FULL_PACKUMENT").as_deref() == Ok("1")
}

/// Refuse a response whose declared `Content-Length` exceeds `cap`
/// before reading the body. A hostile registry (or MITM on a
/// compromised mirror) could otherwise stream gigabytes into the
/// resolver and OOM the install. Servers that omit `Content-Length`
/// (chunked transfer) still reach `bytes()` below, where the read is
/// bounded by the caller's operational timeout. The full-streaming
/// cap-while-reading variant is left for a follow-up, since it needs
/// a `futures` / `tokio-stream` dep that the crate does not yet pull
/// in.
///
/// A `cap` of `0` disables the check entirely — an escape hatch for
/// users who need to pull packuments that exceed the default (e.g.
/// packages with very long release histories) and accept the DoS
/// exposure on the trusted-registry side.
/// Stream-and-count read of a response body that enforces `cap` even
/// when the server omits `Content-Length` (chunked transfer encoding).
/// `check_body_cap` only inspects the precheck header; this function
/// is the runtime gate that closes the chunked-bypass primitive.
async fn read_body_capped(
    mut resp: reqwest::Response,
    cap: u64,
    label: &str,
) -> Result<bytes::Bytes, Error> {
    if cap == 0 {
        return Ok(resp.bytes().await?);
    }
    // Pre-size to a small fixed window when Content-Length is absent
    // so chunked-encoding bodies don't pay BytesMut's doubling-grow
    // overhead all the way up to `cap`.
    const STREAM_INITIAL: usize = 64 * 1024;
    let initial = resp
        .content_length()
        .map(|len| len.min(cap) as usize)
        .unwrap_or(STREAM_INITIAL);
    let mut buf = bytes::BytesMut::with_capacity(initial);
    while let Some(chunk) = resp.chunk().await? {
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{label}: response body exceeds cap {cap}"),
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

fn check_body_cap(resp: &reqwest::Response, cap: u64, label: &str) -> Result<(), Error> {
    if cap == 0 {
        return Ok(());
    }
    if let Some(len) = resp.content_length()
        && len > cap
    {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{label}: response Content-Length {len} exceeds cap {cap}"),
        )));
    }
    Ok(())
}

fn packument_cache_path(cache_dir: &Path, name: &str, registry_url: &str) -> Option<PathBuf> {
    // `name` is derived from registry responses and user-written
    // manifests. `replace('/', "__")` alone would let `../../evil`
    // escape the cache directory and turn a first resolve into an
    // arbitrary-file-write primitive. Delegate to the store's
    // shared validator so the grammar never drifts across crates.
    let safe_name = aube_store::validate_and_encode_name(name)?;
    // Partition by registry origin: a packument fetched against
    // registry A must never be returned to a request that resolves
    // to registry B (CVE-2018-7167 class). Hash the URL so port,
    // trailing-slash, and scheme variants share the same bucket only
    // when literally identical bytes were configured.
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

fn registry_origin_segment(registry_url: &str) -> String {
    let digest = blake3::hash(registry_url.as_bytes()).to_hex();
    format!("origin-{}", &digest.as_str()[..16])
}

/// URL-encode a package name for the `/-/package/<name>/...` path.
/// Only `/` needs encoding — scoped packages have exactly one, between
/// `@scope` and `pkg`. npm expects `@scope%2Fpkg` for scoped names on
/// the dist-tag routes.
fn encoded_name(name: &str) -> String {
    name.replace('/', "%2F")
}

/// `{registry}/-/package/{name}/dist-tags` — the ls endpoint.
fn dist_tag_root_url(registry_url: &str, name: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
    )
}

/// `{registry}/-/package/{name}/dist-tags/{tag}` — the add/rm endpoint.
fn dist_tag_url(registry_url: &str, name: &str, tag: &str) -> String {
    format!(
        "{}/-/package/{}/dist-tags/{}",
        registry_url.trim_end_matches('/'),
        encoded_name(name),
        tag,
    )
}

/// Shared pre-flight mapping for dist-tag responses: turns 404 into
/// `NotFound(name)` and 401/403 into `Unauthorized`, so callers don't
/// have to repeat the same `if resp.status() == ...` ladder around
/// every PUT/GET. DELETE has a richer 404 shape (`name@tag`) and
/// inlines its own handling.
fn check_dist_tag_status(resp: &reqwest::Response, name: &str) -> Result<(), Error> {
    match resp.status() {
        reqwest::StatusCode::NOT_FOUND => Err(Error::NotFound(name.to_string())),
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            Err(Error::Unauthorized)
        }
        _ => Ok(()),
    }
}

async fn parse_full_response<T>(resp: reqwest::Response) -> Result<T, Error>
where
    T: serde::de::DeserializeOwned,
{
    let bytes = resp.bytes().await?;
    let mut buf = bytes.to_vec();
    if let Ok(v) = simd_json::serde::from_slice::<T>(&mut buf) {
        return Ok(v);
    }
    serde_json::from_slice::<T>(&bytes)
        .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

fn read_cached_packument(path: &Path) -> Option<CachedPackument> {
    // simd-json is 2-3x faster than serde_json on large JSON payloads.
    // It mutates the input buffer in-place to do zero-copy parsing where possible.
    let mut content = std::fs::read(path).ok()?;
    simd_json::serde::from_slice(&mut content).ok()
}

fn write_cached_packument(path: &Path, cached: &CachedPackument) -> std::io::Result<()> {
    let json = serde_json::to_vec(cached).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

fn packument_full_cache_path(cache_dir: &Path, name: &str, registry_url: &str) -> Option<PathBuf> {
    let safe_name = aube_store::validate_and_encode_name(name)?;
    let origin = registry_origin_segment(registry_url);
    Some(cache_dir.join(origin).join(format!("{safe_name}.json")))
}

fn read_cached_full_packument(path: &Path) -> Option<CachedFullPackument> {
    let mut content = std::fs::read(path).ok()?;
    simd_json::serde::from_slice(&mut content).ok()
}

/// Typed fast-path read used by `fetch_packument_with_time_cached`
/// in the warm-cache branch. Reads the file once and uses `simd_json`
/// to deserialize the cached wrapper directly into a tiny typed struct
/// holding `fetched_at` plus a fully-typed [`Packument`].
///
/// Returns a missing lookup on file/parse errors, and a stale lookup
/// when revalidation is needed, so callers can decide whether a primer
/// fallback is safe without reading the cache a second time.
fn read_cached_full_packument_typed_lookup(
    path: &Path,
    force_cache: bool,
) -> CachedPackumentLookup {
    #[derive(Deserialize)]
    struct Typed {
        etag: Option<String>,
        last_modified: Option<String>,
        fetched_at: u64,
        #[serde(default)]
        max_age_secs: Option<u64>,
        packument: Packument,
    }

    let Ok(mut content) = std::fs::read(path) else {
        return CachedPackumentLookup::default();
    };
    let Ok(typed) = simd_json::serde::from_slice::<Typed>(&mut content) else {
        return CachedPackumentLookup::default();
    };
    let typed = CachedFullPackumentTyped {
        etag: typed.etag,
        last_modified: typed.last_modified,
        fetched_at: typed.fetched_at,
        max_age_secs: typed.max_age_secs,
        packument: typed.packument,
    };
    if !force_cache && !cached_is_fresh(typed.fetched_at, typed.max_age_secs) {
        return CachedPackumentLookup {
            packument: None,
            stale: true,
            cached: Some(CachedPackumentLookupEntry::Full(typed)),
        };
    }
    CachedPackumentLookup {
        packument: Some(typed.packument),
        stale: false,
        cached: None,
    }
}

fn read_cached_full_packument_typed(path: &Path, force_cache: bool) -> Option<Packument> {
    read_cached_full_packument_typed_lookup(path, force_cache).packument
}

fn write_cached_full_packument(path: &Path, cached: &CachedFullPackument) -> std::io::Result<()> {
    let json = serde_json::to_vec(cached).map_err(std::io::Error::other)?;
    aube_util::fs_atomic::atomic_write(path, &json)
}

/// Emit a `fetchMinSpeedKiBps` warning if the tarball downloaded slower
/// than the configured threshold. `threshold_kibps == 0` disables the
/// warning (pnpm convention). Transfers that completed in one second
/// or less are skipped: for small/fast responses the TCP/TLS handshake
/// and TTFB dominate the "average" throughput, producing spurious
/// warnings that don't reflect network health. This matches pnpm's
/// `elapsedSec > 1` gate in its tarball fetcher.
fn warn_slow_tarball(threshold_kibps: u64, url: &str, len: usize, elapsed: std::time::Duration) {
    if threshold_kibps == 0 {
        return;
    }
    if len == 0 || elapsed <= std::time::Duration::from_secs(1) {
        return;
    }
    let elapsed_ms = elapsed.as_millis() as u64;
    // speed (KiB/s) = bytes / 1024 / seconds = bytes * 1000 / elapsed_ms / 1024
    let kibps = ((len as u64).saturating_mul(1000)) / elapsed_ms / 1024;
    if kibps < threshold_kibps {
        let safe_url = aube_util::url::redact_url(url);
        tracing::warn!(
            kibps,
            threshold_kibps,
            bytes = len,
            elapsed_ms,
            url = %safe_url,
            "slow tarball download fell below fetchMinSpeedKiBps",
        );
    }
}

/// Parse the `Retry-After` response header as a number of seconds.
/// Per RFC 7231, this header can also be an HTTP-date, but the `Date`
/// format is rare in practice for npm-style registries and `chrono`
/// isn't a dep — callers fall back to the computed exponential
/// backoff if the header is missing, unparseable, or in date form.
/// `RETRY_AFTER_CAP_SECS` clamps the parsed value so a hostile
/// registry can't park an install for hours or years by returning
/// `Retry-After: 999999999`.
fn retry_after_from(resp: &reqwest::Response) -> Option<std::time::Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(
        secs.min(RETRY_AFTER_CAP_SECS),
    ))
}

/// Upper bound on `Retry-After` we are willing to honour. 60 seconds
/// is well above any real npm-style rate-limit cooldown and keeps the
/// total retry budget bounded even when a server hands us a bogus
/// value.
const RETRY_AFTER_CAP_SECS: u64 = 60;

/// Maximum number of timeout-shaped retries before we surface the
/// error to the caller, regardless of `fetchRetries`. A timeout has
/// already cost us `fetchTimeout` of wall-clock; retrying many more
/// times compounds the user-visible hang without much chance of
/// recovery on the same upstream. One retry is enough to absorb a
/// fluke; beyond that, fail fast and let the caller decide.
///
/// Counted separately from the global retry counter inside the retry
/// loop so a non-timeout failure (e.g. a 503 on the first attempt)
/// never consumes the timeout budget.
const TIMEOUT_RETRY_CAP: u32 = 1;

#[cfg(test)]
mod seed_tests {
    use super::*;

    fn packument() -> Packument {
        Packument {
            name: "demo".to_owned(),
            modified: None,
            versions: BTreeMap::new(),
            dist_tags: BTreeMap::new(),
            time: BTreeMap::new(),
        }
    }

    #[test]
    fn stale_primer_seed_revalidates() {
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://registry.npmjs.org/");
        let packument = packument();

        client.seed_packument_cache(
            "demo",
            dir.path(),
            &packument,
            Some("etag"),
            Some("last-modified"),
            false,
        );

        let path = packument_cache_path(dir.path(), "demo", "https://registry.npmjs.org/").unwrap();
        let cached = read_cached_packument(&path).unwrap();
        assert_eq!(cached.fetched_at, 0);
        assert_eq!(cached.max_age_secs, Some(0));
        assert!(!cached_is_fresh(cached.fetched_at, cached.max_age_secs));
    }

    #[test]
    fn fresh_primer_seed_skips_revalidation() {
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://registry.npmjs.org/");
        let packument = packument();

        client.seed_packument_cache(
            "demo",
            dir.path(),
            &packument,
            Some("etag"),
            Some("last-modified"),
            true,
        );

        let path = packument_cache_path(dir.path(), "demo", "https://registry.npmjs.org/").unwrap();
        let cached = read_cached_packument(&path).unwrap();
        assert!(cached.fetched_at > 0);
        assert_eq!(cached.max_age_secs, None);
        assert!(cached_is_fresh(cached.fetched_at, cached.max_age_secs));
    }

    #[test]
    fn stale_seed_is_reported_for_revalidation() {
        let dir = tempfile::tempdir().unwrap();
        let client = RegistryClient::new("https://registry.npmjs.org/");
        let packument = packument();

        client.seed_packument_cache("demo", dir.path(), &packument, None, None, false);

        let lookup = client.cached_packument_lookup("demo", dir.path());
        assert!(lookup.stale);
        assert!(lookup.packument.is_none());
    }

    #[test]
    fn default_registry_detection_ignores_trailing_slash() {
        assert!(
            RegistryClient::new("https://registry.npmjs.org").uses_default_npm_registry_for("demo")
        );
        assert!(
            RegistryClient::new("https://registry.npmjs.org/")
                .uses_default_npm_registry_for("demo")
        );
    }
}

#[cfg(test)]
mod retry_tests {
    //! End-to-end tests for [`RegistryClient::send_with_retry`] via the
    //! real fetch entry points. Uses `wiremock` as a local HTTP fixture
    //! so we can exercise 5xx / 429 / slow responses without touching
    //! the network.
    //!
    //! Each test spins up a fresh `MockServer` and a `RegistryClient`
    //! pointing at it, then asserts request counts + returned values.
    //! Timeouts use sub-second values so the suite stays fast.
    use super::*;
    use crate::config::FetchPolicy;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_with(server: &MockServer, policy: FetchPolicy) -> RegistryClient {
        let config = NpmConfig {
            registry: format!("{}/", server.uri()),
            ..Default::default()
        };
        RegistryClient::from_config_with_policy(config, policy)
    }

    fn make_packument_json() -> serde_json::Value {
        serde_json::json!({
            "name": "demo",
            "versions": {},
            "dist-tags": {},
        })
    }

    #[tokio::test]
    async fn retries_on_503_then_succeeds() {
        let server = MockServer::start().await;
        // Two 503s, then a 200. `retries = 2` allows 3 total attempts,
        // so the third one gets through.
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 2,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let packument = client
            .fetch_packument("demo")
            .await
            .expect("retry recovery");
        assert_eq!(packument.name, "demo");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "expected 3 attempts (2 retries)");
    }

    #[tokio::test]
    async fn retry_exhaustion_surfaces_final_5xx() {
        let server = MockServer::start().await;
        // retries=1 ⇒ 2 total attempts, both 503.
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 1,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let err = client
            .fetch_packument("demo")
            .await
            .expect_err("exhausted retries should error");
        // reqwest surfaces non-2xx as `reqwest::Error` via
        // `error_for_status`, wrapped in our `Error::Http`.
        match err {
            Error::Http(inner) => assert_eq!(inner.status().map(|s| s.as_u16()), Some(503)),
            other => panic!("unexpected error: {other}"),
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "retries=1 means 2 total attempts");
    }

    #[tokio::test]
    async fn non_retriable_4xx_does_not_retry() {
        let server = MockServer::start().await;
        // 404 is a terminal signal the caller needs, not a transient
        // failure. The retry helper must short-circuit after one try.
        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 3,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let err = client
            .fetch_packument("missing")
            .await
            .expect_err("404 should surface");
        assert!(matches!(err, Error::NotFound(_)));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "404 must not trigger retries");
    }

    #[tokio::test]
    async fn retry_after_header_on_429_overrides_computed_backoff() {
        // Server asks for a 0-second wait explicitly; our default
        // backoff would be >= mintimeout (1ms here, but production
        // defaults are 10s). If the Retry-After header is honored,
        // the test completes essentially instantly; if it's ignored,
        // the test still passes with tight policy but via a different
        // code path. We assert the helper parses the header correctly
        // by also checking a distinct header value routes through.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
            .mount(&server)
            .await;

        // Set the computed backoff extremely high so a test that
        // *ignored* Retry-After would timeout. We then put a short
        // tokio timeout around the call: if Retry-After is honored
        // (0s), the call completes well within 2s; otherwise it hits
        // the 60s default backoff and the timeout fires.
        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 2,
            retry_factor: 1,
            retry_min_timeout_ms: 60_000,
            retry_max_timeout_ms: 60_000,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let packument = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.fetch_packument("demo"),
        )
        .await
        .expect("Retry-After should be honored, overriding the 60s default backoff")
        .expect("request should succeed");
        assert_eq!(packument.name, "demo");
    }

    #[tokio::test]
    async fn retries_on_429_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 2,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let packument = client.fetch_packument("demo").await.expect("429 retry");
        assert_eq!(packument.name, "demo");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn tarball_fetch_requests_identity_encoding() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pkg.tgz"))
            .and(header("accept-encoding", "identity"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tgz bytes".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 0,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let url = format!("{}/pkg.tgz", server.uri());
        let bytes = client
            .fetch_tarball_bytes(&url)
            .await
            .expect("tarball fetch should succeed");
        assert_eq!(&bytes[..], b"tgz bytes");
    }

    #[tokio::test]
    async fn fetch_timeout_triggers_transport_error() {
        let server = MockServer::start().await;
        // Server delays 500ms; client timeout is 50ms. Every attempt
        // must time out before the body arrives. With retries=0 we get
        // exactly one attempt and a transport error.
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_packument_json())
                    .set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 50,
            retries: 0,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let err = client
            .fetch_packument("demo")
            .await
            .expect_err("timeout should surface");
        match err {
            Error::Http(inner) => assert!(
                inner.is_timeout() || inner.is_request(),
                "expected timeout-shaped reqwest error, got {inner:?}",
            ),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn tarball_headers_timeout_retries_at_most_once_even_with_high_retry_budget() {
        // Headers-stage timeout: server delays the entire response past
        // client `fetchTimeout`, so the `Err` arm of `send().await` fires.
        // With `retries=5` the unbounded policy would attempt 6 times;
        // the timeout cap collapses that to 2 (1 initial + 1 retry). The
        // body-read path is covered separately by
        // `tarball_body_read_timeout_retries_at_most_once`.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pkg.tgz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"unused".to_vec())
                    .set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 50,
            retries: 5,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let url = format!("{}/pkg.tgz", server.uri());
        let err = client
            .fetch_tarball_bytes(&url)
            .await
            .expect_err("timeout should surface");
        match err {
            Error::Http(inner) => assert!(
                inner.is_timeout() || inner.is_request(),
                "expected timeout-shaped reqwest error, got {inner:?}",
            ),
            other => panic!("unexpected error: {other}"),
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            2,
            "timeouts must cap retries at 1 regardless of fetchRetries",
        );
    }

    #[tokio::test]
    async fn timeout_cap_counts_only_timeouts_not_other_retries() {
        // Mixed-error reproducer: a non-timeout failure (503) consumes
        // a global retry slot, then a timeout still gets its allowed
        // retry. If the cap were keyed off the global `attempt`
        // counter, the second timeout would surface immediately and
        // the user would get *zero* timeout retries instead of one.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/pkg.tgz"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/pkg.tgz"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"unused".to_vec())
                    .set_delay(std::time::Duration::from_millis(500)),
            )
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 50,
            retries: 5,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let url = format!("{}/pkg.tgz", server.uri());
        let _ = client
            .fetch_tarball_bytes(&url)
            .await
            .expect_err("all attempts fail");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "expected 1 503 + 1 initial timeout + 1 capped timeout retry; \
             timeout cap must not consume non-timeout retry slots",
        );
    }

    #[tokio::test]
    async fn tarball_body_read_timeout_retries_at_most_once() {
        // Body-read timeout: a different code path from headers-stage
        // timeouts. Server sends the 200 status line + headers
        // immediately, then stalls the body. reqwest's `fetchTimeout`
        // fires inside `resp.chunk().await` during `read_body_capped`,
        // surfacing as `Error::Http(reqwest_timeout)` from the Ok-arm of
        // the retry loop — guarded by `timeout_retry_exhausted`. This
        // is the actual reproducer shape: `@cloudflare/workerd-*`
        // tarballs trickling under a degraded CDN edge.
        //
        // wiremock's `set_delay` delays the *whole* response (including
        // headers), so it can't reproduce this. We need a raw TCP
        // listener that splits header-write and body-stall.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let count_handle = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                count_handle.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    // Drain the request — reqwest waits for headers before
                    // returning from `send()`, so we must answer them.
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\n\
                              Content-Length: 1048576\r\n\
                              Content-Type: application/octet-stream\r\n\r\n",
                        )
                        .await;
                    let _ = sock.flush().await;
                    // Hold the connection without writing the body so the
                    // client times out mid-`chunk()`. Bounded so the test
                    // never wedges if the runtime forgets to drop us.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                });
            }
        });

        let policy = FetchPolicy {
            timeout_ms: 100,
            retries: 5,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let config = NpmConfig {
            registry: format!("http://{addr}/"),
            ..Default::default()
        };
        let client = RegistryClient::from_config_with_policy(config, policy);
        let url = format!("http://{addr}/pkg.tgz");
        let err = client
            .fetch_tarball_bytes(&url)
            .await
            .expect_err("body-read timeout should surface");
        assert!(
            matches!(&err, Error::Http(e) if e.is_timeout() || e.is_request()),
            "expected timeout-shaped error, got {err:?}",
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "body-read timeouts must cap retries at 1 regardless of fetchRetries",
        );
    }

    #[tokio::test]
    async fn warn_timeout_is_pure_observability_and_does_not_fail_request() {
        // Server returns a normal 200 after a 50ms delay. With
        // `warn_timeout_ms = 1`, the helper should log a warning but
        // the request must still succeed — the setting is advisory,
        // not a hard cutoff (that's `timeout_ms`). This pins the
        // invariant so a future refactor doesn't turn a warn into an
        // error by accident.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(make_packument_json())
                    .set_delay(std::time::Duration::from_millis(50)),
            )
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 0,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            warn_timeout_ms: 1,
            min_speed_kibps: 0,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let packument = client
            .fetch_packument("demo")
            .await
            .expect("warn-threshold is advisory — request must still succeed");
        assert_eq!(packument.name, "demo");
    }

    #[tokio::test]
    async fn retries_on_packument_body_decode_error_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(make_packument_json()))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 2,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let packument = client
            .fetch_packument("demo")
            .await
            .expect("decode error should be retried");
        assert_eq!(packument.name, "demo");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "expected retry after decode error");
    }

    #[tokio::test]
    async fn full_packument_cached_retries_on_body_decode_error_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "demo",
                "versions": {},
                "dist-tags": {},
                "time": {},
            })))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 2,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let temp = tempfile::tempdir().unwrap();
        let packument = client
            .fetch_packument_full_cached("demo", temp.path())
            .await
            .expect("decode error should be retried on full packument path");
        assert_eq!(packument["name"], "demo");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "expected retry after decode error");
    }

    #[tokio::test]
    async fn body_decode_retry_does_not_multiply_total_attempt_count() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("{not valid json", "application/json"),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/demo"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            timeout_ms: 5_000,
            retries: 1,
            retry_factor: 1,
            retry_min_timeout_ms: 1,
            retry_max_timeout_ms: 1,
            ..FetchPolicy::default()
        };
        let client = client_with(&server, policy);
        let err = client
            .fetch_packument("demo")
            .await
            .expect_err("retry budget should be exhausted after two total attempts");
        match err {
            Error::Http(inner) => assert_eq!(inner.status().map(|s| s.as_u16()), Some(503)),
            other => panic!("unexpected error: {other}"),
        }

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "expected total attempts to stay capped");
    }

    #[tokio::test]
    async fn scoped_packument_request_is_url_encoded() {
        // Artifactory's npm remote rejects the literal `@scope/pkg`
        // path form with 406 and only accepts `@scope%2Fpkg`. The
        // corgi Accept header must include `application/json` and
        // `*/*` fallbacks for the same reason. wiremock normalizes
        // `%2F` to `/` in its path matcher, so match on any GET and
        // assert the raw request line instead.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "@scope/pkg",
                "versions": {},
                "dist-tags": {},
            })))
            .mount(&server)
            .await;

        let client = client_with(&server, FetchPolicy::default());
        let packument = client
            .fetch_packument("@scope/pkg")
            .await
            .expect("scoped packument fetch must succeed");
        assert_eq!(packument.name, "@scope/pkg");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let raw = requests[0].url.as_str();
        assert!(
            raw.contains("/@scope%2Fpkg"),
            "expected %2F-encoded scope separator, got {raw}"
        );
        let accept = requests[0]
            .headers
            .get("accept")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(
            accept, "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*",
            "corgi Accept header must include JSON and */* fallbacks",
        );
    }
}

#[cfg(test)]
mod slow_tarball_tests {
    //! Pure-function tests for [`warn_slow_tarball`]. The helper emits
    //! a `tracing::warn` so we can't directly assert on output here;
    //! instead we cover the branching (threshold=0 → no-op, sub-second
    //! transfer → no-op, empty body → no-op, slow real download → warn)
    //! by asserting the helper doesn't panic. The BATS smoke test
    //! exercises the log line end-to-end.
    use super::warn_slow_tarball;
    use std::time::Duration;

    #[test]
    fn zero_threshold_disables_warning() {
        // threshold=0 short-circuits before any math — safe with any
        // inputs, including a genuinely slow transfer.
        warn_slow_tarball(
            0,
            "https://example.com/pkg.tgz",
            1024,
            Duration::from_secs(10),
        );
    }

    #[test]
    fn sub_second_transfer_skipped_to_avoid_handshake_noise() {
        // Matches pnpm's `elapsedSec > 1` gate. A 2 KiB tarball
        // completing in 500ms computes to 4 KiB/s — well below the
        // 50 KiB/s threshold — but the "average" is dominated by TCP/
        // TLS handshake + TTFB, not real throughput. Must not warn.
        warn_slow_tarball(
            50,
            "https://example.com/quick.tgz",
            2048,
            Duration::from_millis(500),
        );
    }

    #[test]
    fn exactly_one_second_skipped() {
        // Boundary: pnpm uses `elapsedSec > 1` (strictly greater), so
        // a transfer that took exactly one second must not warn even
        // though its computed average is below threshold.
        warn_slow_tarball(
            50,
            "https://example.com/boundary.tgz",
            10_240,
            Duration::from_secs(1),
        );
    }

    #[test]
    fn zero_elapsed_skipped_to_avoid_division_by_zero() {
        // `resp.bytes()` can plausibly complete in under a millisecond
        // for cached/in-memory responses (wiremock is in-process). The
        // sub-second gate covers this too, but we keep the test to pin
        // the branch.
        warn_slow_tarball(50, "https://example.com/fast.tgz", 10_240, Duration::ZERO);
    }

    #[test]
    fn fast_download_does_not_warn() {
        // 10 MiB in 2 seconds ≈ 5_120 KiB/s, far above the 50 KiB/s
        // default threshold. Elapsed clears the one-second gate so
        // the math runs — and must not warn.
        warn_slow_tarball(
            50,
            "https://example.com/pkg.tgz",
            10 * 1024 * 1024,
            Duration::from_secs(2),
        );
    }

    #[test]
    fn slow_download_triggers_warning_path() {
        // 10 KiB in 2 seconds = 5 KiB/s, well below the 50 KiB/s
        // threshold and past the one-second gate. The helper should
        // take the warn branch; we rely on the BATS smoke test to
        // observe the log line itself, but this call must at least
        // not panic on arithmetic.
        warn_slow_tarball(
            50,
            "https://example.com/slow.tgz",
            10_240,
            Duration::from_secs(2),
        );
    }
}
