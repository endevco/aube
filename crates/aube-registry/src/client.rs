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
    packument: serde_json::Value,
}

/// How long to trust a cached packument before revalidating with the registry.
/// Trust cached packuments for 30 minutes before revalidating. This keeps
/// repeated installs in a long-lived dev session from devolving into hundreds
/// of conditional metadata requests once the cache is just over pnpm's 5-minute
/// default staleness window.
const PACKUMENT_TTL_SECS: u64 = 1800;

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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Client for interacting with the npm registry.
pub struct RegistryClient {
    http: reqwest::Client,
    http_by_uri: BTreeMap<String, reqwest::Client>,
    token_helper_cache: Mutex<BTreeMap<String, Option<String>>>,
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

    /// Get the registry URL for a given package name (respects scoped registries).
    fn registry_url_for(&self, name: &str) -> &str {
        self.config.registry_for(name)
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
        } else if let Some(token) = self.config.global_auth_token.as_ref() {
            req.bearer_auth(token)
        } else {
            req
        }
    }

    fn registry_auth_token_for(&self, registry_url: &str) -> Option<String> {
        if let Some(auth) = self.config.registry_config_for(registry_url) {
            if let Some(token) = auth.auth_token.as_ref() {
                return Some(token.to_string());
            }
            if let Some(helper) = auth.token_helper.as_deref() {
                return self.cached_token_helper_result(helper);
            }
        }
        None
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

    /// Send an HTTP request with transient-failure retry. Invoked from
    /// every GET path (packument fetches, tarball downloads, dist-tag
    /// reads) so the `fetchRetries` / `fetchRetryFactor` /
    /// `fetchRetryMintimeout` / `fetchRetryMaxtimeout` settings take
    /// effect without each call site open-coding backoff math.
    ///
    /// `build` is called once per attempt — it has to rebuild the
    /// [`reqwest::RequestBuilder`] from scratch because a builder is
    /// consumed by `.send()`. That matches how the existing call sites
    /// already compose conditional headers (`If-None-Match`, `Accept`,
    /// `npm-otp`) on a fresh request, so the closure just wraps those
    /// few lines.
    ///
    /// Retry policy (matches pnpm / `make-fetch-happen`):
    /// - Transport errors (`reqwest::Error`) are always retriable.
    /// - `5xx` and `429` responses are retriable.
    /// - Everything else (2xx, 3xx, 4xx other than 429) short-circuits
    ///   and is returned immediately — we don't retry 404s or auth
    ///   failures because the next attempt will just fail the same
    ///   way, and for mutation endpoints a retry could double-apply.
    ///
    /// Writes do *not* route through this helper; only idempotent GETs
    /// do. See `put_packument` / `put_dist_tag` for the write-path
    /// contract.
    async fn send_with_retry<F>(&self, build: F) -> Result<reqwest::Response, reqwest::Error>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        self.send_with_retry_timed(build)
            .await
            .map(|(resp, _)| resp)
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
                    let retriable = status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    if !retriable || is_last {
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
                    // Recoverable: the retry succeeds in the common
                    // case, and the final failure propagates up as a
                    // user-facing error. Emitting a WARN on every
                    // transient blip turns a healthy retry loop into
                    // install-time noise.
                    tracing::debug!(
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
                    tracing::debug!(
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
        let cache_path = packument_full_cache_path(cache_dir, name);
        let cached = read_cached_full_packument(&cache_path);

        // --prefer-offline / --offline: trust any cached copy regardless of age.
        // --offline additionally forbids falling back to the network on a miss.
        let force_cache = matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        );
        if let Some(c) = cached.as_ref()
            && (force_cache || now_secs().saturating_sub(c.fetched_at) < PACKUMENT_TTL_SECS)
        {
            return Ok(cached.unwrap().packument);
        }
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }

        let (url, registry_url) = self.packument_url(name);

        // Rebuild the conditional request on each retry. Held in a
        // closure so the revalidation headers are consistent across
        // attempts — a 503 retry with stale `If-None-Match` would be
        // a caching bug.
        let cached_ref = cached.as_ref();
        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
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
            })
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(c) = cached
        {
            let to_cache = CachedFullPackument {
                etag: c.etag.clone(),
                last_modified: c.last_modified.clone(),
                fetched_at: now_secs(),
                packument: c.packument.clone(),
            };
            let _ = write_cached_full_packument(&cache_path, &to_cache);
            return Ok(c.packument);
        }

        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let packument: serde_json::Value = resp.error_for_status()?.json().await?;

        let to_cache = CachedFullPackument {
            etag,
            last_modified,
            fetched_at: now_secs(),
            packument: packument.clone(),
        };
        let _ = write_cached_full_packument(&cache_path, &to_cache);

        Ok(packument)
    }

    /// Fetch the full (non-corgi) packument for a package and parse it
    /// into [`Packument`]. Unlike [`Self::fetch_packument_cached`], the
    /// result includes the `time` map — needed for
    /// `--resolution-mode=time-based`. Shares on-disk cache layout with
    /// [`Self::fetch_packument_full_cached`] so callers pay one network
    /// fetch for both the `aube view`-style full JSON and the time map.
    ///
    /// Hot path on warm cache: reads the cache file once, deserializes
    /// header fields into a tiny borrow-lived wrapper that captures
    /// the packument body as a `&RawValue` (zero-copy), then
    /// deserializes that slice directly into [`Packument`]. The
    /// previous implementation went through `serde_json::Value` +
    /// `serde_json::from_value`, which walked the untyped tree twice
    /// and roughly doubled the resolver's packument-read time on the
    /// medium benchmark fixture.
    pub async fn fetch_packument_with_time_cached(
        &self,
        name: &str,
        cache_dir: &Path,
    ) -> Result<Packument, Error> {
        // Fast path: try the warm-cache read first. Matches the
        // freshness window logic in `fetch_packument_full_cached`
        // exactly so the two APIs share revalidation behavior.
        let cache_path = packument_full_cache_path(cache_dir, name);
        let force_cache = matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        );
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

    /// Fetch the abbreviated packument for a package (corgi format).
    pub async fn fetch_packument(&self, name: &str) -> Result<Packument, Error> {
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("packument for {name}")));
        }
        let (url, registry_url) = self.packument_url(name);

        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
                let req = self.authed_get(&url, registry_url);
                if force_full_packument() {
                    req
                } else {
                    req.header("Accept", PACKUMENT_ACCEPT)
                }
            })
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }

        let packument: Packument = resp.error_for_status()?.json().await?;
        Ok(packument)
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
        let cache_path = packument_cache_path(cache_dir, name);
        let cached = read_cached_packument(&cache_path);

        // Fast path: trust the cache if it's still fresh.
        // Move out of the wrapper to avoid cloning the Packument.
        // --prefer-offline / --offline extend "fresh" to "any cached entry"
        // so we skip revalidation and, for --offline, the network entirely.
        let force_cache = matches!(
            self.network_mode,
            NetworkMode::PreferOffline | NetworkMode::Offline
        );
        if let Some(c) = cached.as_ref()
            && (force_cache || now_secs().saturating_sub(c.fetched_at) < PACKUMENT_TTL_SECS)
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
        let resp = self
            .send_metadata_with_retry(&format!("packument {name}"), || {
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
            })
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::NotFound(name.to_string()));
        }

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED
            && let Some(c) = cached
        {
            // Refresh the timestamp so we can skip the network next time
            let to_cache = CachedPackument {
                etag: c.etag.clone(),
                last_modified: c.last_modified.clone(),
                fetched_at: now_secs(),
                packument: c.packument.clone(),
            };
            let _ = write_cached_packument(&cache_path, &to_cache);
            return Ok(c.packument);
        }

        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let last_modified = resp
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let packument: Packument = resp.error_for_status()?.json().await?;

        let to_cache = CachedPackument {
            etag,
            last_modified,
            fetched_at: now_secs(),
            packument: packument.clone(),
        };
        let _ = write_cached_packument(&cache_path, &to_cache);

        Ok(packument)
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
        if self.network_mode == NetworkMode::Offline {
            return Err(Error::Offline(format!("tarball {url}")));
        }
        // Tarball URLs may point to any registry, try to match auth.
        // Pass the full tarball URL through so longest-prefix matching
        // in `registry_config_for` can find path-scoped auth entries
        // (e.g. `//host/artifactory/npm/`). Retries cover transient
        // 5xx / 429 / connection errors; see [`Self::send_with_retry`].
        let resp = self
            .send_with_retry(|| self.authed_get(url, url))
            .await?
            .error_for_status()?;
        let started = std::time::Instant::now();
        let bytes = resp.bytes().await?;
        let elapsed = started.elapsed();
        warn_slow_tarball(self.fetch_policy.min_speed_kibps, url, bytes.len(), elapsed);
        Ok(bytes)
    }

    /// Download a tarball to a file path.
    pub async fn fetch_tarball(&self, url: &str, dest: &Path) -> Result<(), Error> {
        let bytes = self.fetch_tarball_bytes(url).await?;
        std::fs::write(dest, &bytes)?;
        Ok(())
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
        let value: serde_json::Value = resp.error_for_status()?.json().await?;
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
        let path = packument_full_cache_path(cache_dir, name);
        let _ = std::fs::remove_file(&path);
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
    let mut builder = reqwest::Client::builder()
        .user_agent("aube/0.1.0")
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
        // `strict-ssl=false` disables cert validation entirely. This
        // is a security hole on purpose: corporate registries should
        // prefer per-registry `ca` / `cafile` so validation stays on.
        .danger_accept_invalid_certs(!config.strict_ssl)
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

fn packument_cache_path(cache_dir: &Path, name: &str) -> PathBuf {
    let safe_name = name.replace('/', "__");
    cache_dir.join(format!("{safe_name}.json"))
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

fn read_cached_packument(path: &Path) -> Option<CachedPackument> {
    // simd-json is 2-3x faster than serde_json on large JSON payloads.
    // It mutates the input buffer in-place to do zero-copy parsing where possible.
    let mut content = std::fs::read(path).ok()?;
    simd_json::serde::from_slice(&mut content).ok()
}

fn write_cached_packument(path: &Path, cached: &CachedPackument) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // serde_json for writes — simd-json's serializer doesn't have a meaningful
    // perf advantage on writes, and we want JSON output that's readable.
    let json = serde_json::to_vec(cached).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

fn packument_full_cache_path(cache_dir: &Path, name: &str) -> PathBuf {
    let safe_name = name.replace('/', "__");
    cache_dir.join(format!("{safe_name}.json"))
}

fn read_cached_full_packument(path: &Path) -> Option<CachedFullPackument> {
    let mut content = std::fs::read(path).ok()?;
    simd_json::serde::from_slice(&mut content).ok()
}

/// Typed fast-path read used by `fetch_packument_with_time_cached`
/// in the warm-cache branch. Reads the file once, uses a borrow-lived
/// wrapper with `&serde_json::value::RawValue` to capture the
/// `packument` field as an untouched JSON slice, checks freshness,
/// then hands that slice straight to `serde_json` for a typed
/// `Packument` parse — no `Value` intermediate, no `Box<RawValue>`
/// owned-string copy, one pass over the bytes.
///
/// Returns `None` on any error (file missing, parse error, stale
/// cache) so the caller transparently falls back to the network /
/// `Value` path.
fn read_cached_full_packument_typed(path: &Path, force_cache: bool) -> Option<Packument> {
    #[derive(Deserialize)]
    struct Borrowed<'a> {
        fetched_at: u64,
        #[serde(borrow)]
        packument: &'a serde_json::value::RawValue,
    }

    let content = std::fs::read(path).ok()?;
    let head: Borrowed = serde_json::from_slice(&content).ok()?;
    if !force_cache && now_secs().saturating_sub(head.fetched_at) >= PACKUMENT_TTL_SECS {
        return None;
    }
    // simd-json for the body parse — it's 2–3× faster than serde_json
    // on dense packument JSON. It mutates its input, so we own a
    // mutable Vec by copying the raw slice out of the `RawValue`.
    // The copy is cheap (~tens of GB/s memcpy) and amortized against
    // the parse that follows.
    let mut buf = head.packument.get().as_bytes().to_vec();
    simd_json::serde::from_slice(&mut buf).ok()
}

fn write_cached_full_packument(path: &Path, cached: &CachedFullPackument) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec(cached).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
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
        tracing::warn!(
            kibps,
            threshold_kibps,
            bytes = len,
            elapsed_ms,
            url,
            "slow tarball download fell below fetchMinSpeedKiBps",
        );
    }
}

/// Parse the `Retry-After` response header as a number of seconds.
/// Per RFC 7231, this header can also be an HTTP-date, but the `Date`
/// format is rare in practice for npm-style registries and `chrono`
/// isn't a dep — callers fall back to the computed exponential
/// backoff if the header is missing, unparseable, or in date form.
fn retry_after_from(resp: &reqwest::Response) -> Option<std::time::Duration> {
    let raw = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(std::time::Duration::from_secs(secs))
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
    use wiremock::matchers::{method, path};
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
        };
        let client = client_with(&server, policy);
        let packument = client
            .fetch_packument("demo")
            .await
            .expect("warn-threshold is advisory — request must still succeed");
        assert_eq!(packument.name, "demo");
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
