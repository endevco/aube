use base64::Engine as _;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Parsed npm configuration from .npmrc files.
///
/// Only holds the *registry-client specific* fields — registry URL, auth,
/// scoped overrides. Generic pnpm settings (`auto-install-peers`,
/// `node-linker`, etc) are resolved by `aube_cli::settings_values` against
/// the raw `.npmrc` entries returned by [`load_npmrc_entries`], so that
/// the canonical list of source keys lives in `settings.toml` and adding
/// a new setting is a one-place change.
#[derive(Debug, Clone)]
pub struct NpmConfig {
    /// Default registry URL (e.g., "https://registry.npmjs.org/")
    pub registry: String,
    /// Scoped registry overrides: "@scope" -> "https://registry.example.com/"
    pub scoped_registries: BTreeMap<String, String>,
    /// Auth config keyed by registry URL prefix (e.g., "//registry.example.com/")
    pub auth_by_uri: BTreeMap<String, AuthConfig>,
    /// Global auth token (for default registry, when no URI-specific token exists)
    pub global_auth_token: Option<String>,
    /// Proxy URL for outgoing HTTPS requests (`https-proxy` / `HTTPS_PROXY`).
    pub https_proxy: Option<String>,
    /// Proxy URL for outgoing HTTP requests (`proxy` / `http-proxy` / `HTTP_PROXY`).
    pub http_proxy: Option<String>,
    /// Comma-separated list of hosts that bypass the proxy
    /// (`noproxy` / `NO_PROXY`). Passed through to
    /// `reqwest::NoProxy::from_string` verbatim so wildcards and
    /// port-qualified hosts behave the same as curl / node.
    pub no_proxy: Option<String>,
    /// Validate TLS certificates. Defaults to `true`. Setting this to
    /// `false` disables certificate verification entirely — only useful
    /// behind corporate MITM proxies with an untrusted CA.
    pub strict_ssl: bool,
    /// Local interface IP to bind outgoing connections to
    /// (`local-address`). Parsed as `IpAddr`; unparseable values are
    /// dropped at load time and logged.
    pub local_address: Option<std::net::IpAddr>,
    /// Maximum concurrent connections per origin (`maxsockets`).
    /// Plumbed into reqwest's `pool_max_idle_per_host`, which is the
    /// closest analogue to npm/pnpm's per-origin socket cap.
    pub max_sockets: Option<usize>,
    /// Value of `.npmrc`'s legacy `proxy=` key, tracked separately
    /// from `https_proxy` / `http_proxy` because pnpm treats it as
    /// the fallback for `httpsProxy` (and secondarily for
    /// `httpProxy`). Resolved into the final `https_proxy` /
    /// `http_proxy` values during `apply_proxy_env`.
    pub npmrc_proxy: Option<String>,
}

/// Authentication for a specific registry.
#[derive(Debug, Clone, Default)]
pub struct AuthConfig {
    pub auth_token: Option<String>,
    /// Base64-encoded "username:password"
    pub auth: Option<String>,
    pub username: Option<String>,
    /// npm stores the split-field password as base64-encoded bytes.
    pub password: Option<String>,
    pub token_helper: Option<String>,
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Default)]
pub struct TlsConfig {
    pub ca: Vec<String>,
    pub cafile: Option<PathBuf>,
    pub cert: Option<String>,
    pub key: Option<String>,
}

impl Default for NpmConfig {
    /// Hand-rolled so `strict_ssl` defaults to `true` instead of
    /// `bool::default()` / `false`. Any caller that builds an
    /// `NpmConfig` via `..Default::default()` (including
    /// `RegistryClient::new`) gets a TLS-validating client without
    /// having to remember to flip this field — the unsafe default is
    /// too easy to foot-gun otherwise.
    fn default() -> Self {
        Self {
            registry: String::new(),
            scoped_registries: BTreeMap::new(),
            auth_by_uri: BTreeMap::new(),
            global_auth_token: None,
            https_proxy: None,
            http_proxy: None,
            no_proxy: None,
            strict_ssl: true,
            local_address: None,
            max_sockets: None,
            npmrc_proxy: None,
        }
    }
}

impl NpmConfig {
    /// Load config by reading .npmrc files in priority order:
    /// 1. ~/.npmrc (user)
    /// 2. .npmrc in project dir (project)
    ///
    /// Project-level values override user-level values. Shares file
    /// discovery with [`load_npmrc_entries`] so the registry client and
    /// the generic settings resolver (`aube_cli::settings_values`) can
    /// never disagree on precedence.
    pub fn load(project_dir: &Path) -> Self {
        let env: Vec<(String, String)> = std::env::vars().collect();
        Self::load_with_env(project_dir, &env)
    }

    /// Same as [`NpmConfig::load`] but takes a captured env snapshot
    /// instead of reading `std::env` directly. Tests that assert on
    /// file-only behavior pass an empty slice so `npm_config_*` vars
    /// leaking from the developer's shell can't perturb the result.
    pub(crate) fn load_with_env(project_dir: &Path, env: &[(String, String)]) -> Self {
        let mut config = Self {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        // `apply` processes entries in order and is last-write-wins, so
        // feeding it the merged user-then-project list produces the
        // same result as the prior two-pass implementation.
        config.apply(load_npmrc_entries(project_dir));
        // `npm_config_*` / `NPM_CONFIG_*` env vars beat file config in
        // npm/pnpm, and the docs promise the same for aube. Apply them
        // after `.npmrc` so last-write-wins gives env the higher slot.
        config.apply(npm_config_env_entries_from(env));
        // Env vars fill in any proxy fields the .npmrc didn't set.
        // npm/pnpm/curl all check both the upper- and lowercase forms.
        config.apply_proxy_env();
        config.apply_builtin_scoped_defaults();
        config
    }

    /// Register default scope→registry mappings that aube ships with
    /// out of the box. Currently only `@jsr` → <https://npm.jsr.io/>,
    /// which lets `jsr:` specs work without the user touching `.npmrc`.
    /// User-provided `.npmrc` entries win — `apply` has already run by
    /// the time we get here, so we only fill in gaps.
    fn apply_builtin_scoped_defaults(&mut self) {
        self.scoped_registries
            .entry(crate::jsr::JSR_NPM_SCOPE.to_string())
            .or_insert_with(|| crate::jsr::JSR_DEFAULT_REGISTRY.to_string());
    }

    /// Fallback-only: populate proxy/no_proxy from the standard
    /// `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY` environment variables
    /// when the `.npmrc` layer didn't already set them. A value from
    /// `.npmrc` wins over env so project configuration stays explicit.
    /// Resolve proxy/no_proxy fields using the same precedence
    /// chain pnpm's config reader applies (see
    /// `config/reader/src/index.ts` lines 559-568 in the pnpm
    /// repo):
    ///
    /// - `httpsProxy` ← `.npmrc httpsProxy` ?? `.npmrc proxy` ??
    ///   env `HTTPS_PROXY`/`https_proxy`
    /// - `httpProxy` ← `.npmrc httpProxy` ?? resolved `httpsProxy`
    ///   ?? env `HTTP_PROXY`/`http_proxy` ?? env `PROXY`/`proxy`
    /// - `noProxy` ← `.npmrc noProxy` ?? env `NO_PROXY`/`no_proxy`
    ///
    /// Note that `httpsProxy` does **not** fall back to
    /// `HTTP_PROXY`: pnpm (and npm) only inherit the HTTP proxy
    /// downward into HTTPS, never upward. The `httpProxy` field
    /// *does* inherit whatever `httpsProxy` resolved to, so a
    /// single `https-proxy=...` line in `.npmrc` configures both.
    pub fn apply_proxy_env(&mut self) {
        if self.https_proxy.is_none() {
            self.https_proxy = self
                .npmrc_proxy
                .clone()
                .or_else(|| env_any(&["HTTPS_PROXY", "https_proxy"]));
        }
        if self.http_proxy.is_none() {
            self.http_proxy = self
                .https_proxy
                .clone()
                .or_else(|| env_any(&["HTTP_PROXY", "http_proxy"]))
                .or_else(|| env_any(&["PROXY", "proxy"]));
        }
        if self.no_proxy.is_none() {
            self.no_proxy = env_any(&["NO_PROXY", "no_proxy"]);
        }
    }

    /// Get the registry URL for a given package name.
    pub fn registry_for(&self, package_name: &str) -> &str {
        if let Some(scope) = package_scope(package_name)
            && let Some(url) = self.scoped_registries.get(scope)
        {
            return url;
        }
        &self.registry
    }

    /// Get the auth token for a given registry URL.
    pub fn auth_token_for(&self, registry_url: &str) -> Option<&str> {
        if let Some(auth) = self.registry_config_for(registry_url)
            && let Some(ref token) = auth.auth_token
        {
            return Some(token);
        }
        self.global_auth_token.as_deref()
    }

    pub fn token_helper_for(&self, registry_url: &str) -> Option<&str> {
        self.registry_config_for(registry_url)
            .and_then(|auth| auth.token_helper.as_deref())
    }

    /// Get the basic auth (_auth) for a given registry URL.
    pub fn basic_auth_for(&self, registry_url: &str) -> Option<String> {
        let auth = self.registry_config_for(registry_url)?;
        if let Some(ref a) = auth.auth {
            return Some(a.clone());
        }
        let username = auth.username.as_ref()?;
        let password = auth.password.as_ref()?;
        let password = base64::engine::general_purpose::STANDARD
            .decode(password)
            .ok()?;
        let mut raw = Vec::with_capacity(username.len() + 1 + password.len());
        raw.extend_from_slice(username.as_bytes());
        raw.push(b':');
        raw.extend_from_slice(&password);
        Some(base64::engine::general_purpose::STANDARD.encode(raw))
    }

    pub fn registry_config_for(&self, registry_url: &str) -> Option<&AuthConfig> {
        let uri_key = registry_uri_key(registry_url);
        if let Some(auth) = self.auth_by_uri.get(&uri_key) {
            return Some(auth);
        }
        let trimmed = uri_key.trim_end_matches('/');
        self.auth_by_uri.get(trimmed)
    }

    fn apply(&mut self, entries: Vec<(String, String)>) {
        for (key, value) in entries {
            if key == "registry" {
                self.registry = normalize_registry_url(&value);
            } else if key == "_authToken" {
                self.global_auth_token = Some(value);
            } else if matches!(key.as_str(), "https-proxy" | "httpsProxy") {
                self.https_proxy = non_empty(value);
            } else if matches!(key.as_str(), "http-proxy" | "httpProxy") {
                self.http_proxy = non_empty(value);
            } else if key == "proxy" {
                // pnpm treats `.npmrc proxy=` as the fallback source
                // for `httpsProxy` (and, transitively, `httpProxy`) —
                // not as a direct alias for `httpProxy`. See the
                // `apply_proxy_env` resolution chain.
                self.npmrc_proxy = non_empty(value);
            } else if matches!(key.as_str(), "noproxy" | "noProxy" | "no-proxy") {
                self.no_proxy = non_empty(value);
            } else if matches!(key.as_str(), "strict-ssl" | "strictSsl") {
                if let Some(b) = aube_settings::parse_bool(&value) {
                    self.strict_ssl = b;
                }
            } else if matches!(key.as_str(), "local-address" | "localAddress") {
                match value.trim().parse::<std::net::IpAddr>() {
                    Ok(ip) => self.local_address = Some(ip),
                    Err(e) => tracing::warn!("ignoring invalid local-address {value:?}: {e}"),
                }
            } else if key == "maxsockets" {
                match value.trim().parse::<usize>() {
                    Ok(n) if n > 0 => self.max_sockets = Some(n),
                    Ok(_) => tracing::warn!("ignoring maxsockets=0"),
                    Err(e) => tracing::warn!("ignoring invalid maxsockets {value:?}: {e}"),
                }
            } else if let Some(scope) = key.strip_suffix(":registry") {
                if scope.starts_with('@') {
                    self.scoped_registries
                        .insert(scope.to_string(), normalize_registry_url(&value));
                }
            } else if key.starts_with("//") {
                // URI-specific config: //registry.url/:_authToken=TOKEN
                if let Some((uri, suffix)) = key.rsplit_once(':') {
                    let entry = self.auth_by_uri.entry(uri.to_string()).or_default();
                    match suffix {
                        "_authToken" => entry.auth_token = Some(value),
                        "_auth" => entry.auth = Some(value),
                        "username" => entry.username = Some(value),
                        "_password" => entry.password = Some(value),
                        "tokenHelper" | "token-helper" => entry.token_helper = non_empty(value),
                        "ca" | "ca[]" => entry.tls.ca.push(pem_value(value)),
                        "cafile" | "caFile" => entry.tls.cafile = Some(PathBuf::from(value)),
                        "cert" => entry.tls.cert = Some(pem_value(value)),
                        "key" => entry.tls.key = Some(pem_value(value)),
                        _ => {} // Ignore unknown suffixes for now
                    }
                }
            }
            // Generic pnpm settings (`auto-install-peers`, etc) are NOT
            // matched here — they're resolved by aube's settings
            // module against the raw entries, using the canonical
            // source list from settings.toml. Add a new branch here
            // only if the key maps to a registry-client concept.
        }
    }
}

/// Resolved values for the five `fetch*` settings declared in
/// `settings.toml`. Kept separate from [`NpmConfig`] because these are
/// generic pnpm settings (sourced by the settings resolver, not the
/// registry-client-specific `.npmrc` parser in [`NpmConfig::apply`]) and
/// because wiring them through a single struct keeps the retry helper
/// on [`crate::client::RegistryClient`] from growing five parameters.
///
/// All durations are stored in milliseconds to match pnpm / npm's
/// `.npmrc` conventions; callers convert to [`std::time::Duration`] at
/// the reqwest boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchPolicy {
    /// `fetchTimeout` — per-request HTTP timeout. Applied via
    /// `reqwest::ClientBuilder::timeout` so it covers the whole
    /// response (headers + body).
    pub timeout_ms: u64,
    /// `fetchRetries` — number of *additional* attempts on transient
    /// failure. `retries = 2` means up to 3 total attempts, matching
    /// pnpm / `make-fetch-happen`.
    pub retries: u32,
    /// `fetchRetryFactor` — exponential backoff factor. Attempt `n`
    /// waits `min(mintimeout * factor^n, maxtimeout)` ms before retry.
    pub retry_factor: u32,
    /// `fetchRetryMintimeout` — lower bound on the computed backoff.
    pub retry_min_timeout_ms: u64,
    /// `fetchRetryMaxtimeout` — upper bound on the computed backoff.
    pub retry_max_timeout_ms: u64,
    /// `fetchWarnTimeoutMs` — observability threshold: emit a warning
    /// when a *metadata* request (packument, dist-tags) takes longer
    /// than this to receive a response. Does not fail the request; the
    /// hard cut-off is still [`Self::timeout_ms`]. `0` disables the
    /// warning, matching pnpm's convention for "unset observability".
    pub warn_timeout_ms: u64,
    /// `fetchMinSpeedKiBps` — observability threshold: emit a warning
    /// when a tarball finishes downloading with an average speed below
    /// this value (KiB/s). `0` disables the warning. As with
    /// `warn_timeout_ms`, we only warn — we never abort the transfer.
    pub min_speed_kibps: u64,
}

impl Default for FetchPolicy {
    /// Matches the declared defaults in `settings.toml` (and npm / pnpm
    /// defaults). Callers that skip [`FetchPolicy::from_ctx`] still get
    /// sensible retry + timeout behavior.
    fn default() -> Self {
        Self {
            timeout_ms: 60_000,
            retries: 2,
            retry_factor: 10,
            retry_min_timeout_ms: 10_000,
            retry_max_timeout_ms: 60_000,
            warn_timeout_ms: 10_000,
            min_speed_kibps: 50,
        }
    }
}

impl FetchPolicy {
    /// Resolve every field from a settings [`ResolveCtx`]. Walks the
    /// full cli > env > npmrc > workspaceYaml precedence chain via the
    /// generated accessors, so env-var overrides like
    /// `NPM_CONFIG_FETCH_TIMEOUT` Just Work without bespoke parsing.
    pub fn from_ctx(ctx: &aube_settings::ResolveCtx<'_>) -> Self {
        Self {
            timeout_ms: aube_settings::resolved::fetch_timeout(ctx),
            retries: clamp_u32(aube_settings::resolved::fetch_retries(ctx)),
            retry_factor: clamp_u32(aube_settings::resolved::fetch_retry_factor(ctx)),
            retry_min_timeout_ms: aube_settings::resolved::fetch_retry_mintimeout(ctx),
            retry_max_timeout_ms: aube_settings::resolved::fetch_retry_maxtimeout(ctx),
            warn_timeout_ms: aube_settings::resolved::fetch_warn_timeout_ms(ctx),
            min_speed_kibps: aube_settings::resolved::fetch_min_speed_ki_bps(ctx),
        }
    }

    /// Compute the sleep duration before the given retry attempt
    /// (1-indexed: `attempt=1` is the wait before the *second* HTTP
    /// request, i.e. the first retry). Clamped into
    /// `[retry_min_timeout_ms, retry_max_timeout_ms]`.
    ///
    /// Algorithm mirrors `make-fetch-happen`'s exponential backoff:
    /// `min(mintimeout * factor^(attempt-1), maxtimeout)`. Arithmetic
    /// uses saturating math so huge `factor` values don't panic on
    /// overflow — they just get clamped to the max.
    pub fn backoff_for_attempt(&self, attempt: u32) -> std::time::Duration {
        let attempt = attempt.max(1);
        let factor = u64::from(self.retry_factor.max(1));
        let exp = attempt.saturating_sub(1);
        let mut wait = self.retry_min_timeout_ms;
        for _ in 0..exp {
            wait = wait.saturating_mul(factor);
            if wait >= self.retry_max_timeout_ms {
                wait = self.retry_max_timeout_ms;
                break;
            }
        }
        let clamped = wait
            .max(self.retry_min_timeout_ms)
            .min(self.retry_max_timeout_ms);
        std::time::Duration::from_millis(clamped)
    }
}

/// The generated accessors expose these counts as `u64` (the common
/// int wire type), but reqwest / our retry loop want `u32`. Values
/// that big are meaningless for "retry attempts" / "backoff factor" so
/// clamp instead of erroring — a user writing `fetchRetries=99999999`
/// gets `u32::MAX` attempts, which is effectively "retry forever".
fn clamp_u32(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

/// Synthesize `.npmrc`-style entries from a captured `npm_config_*` /
/// `NPM_CONFIG_*` environment-variable slice so [`NpmConfig::apply`]
/// can consume them uniformly. Only registry-client-owned keys (the
/// default registry, scoped registries, per-URI auth, proxies, TLS
/// knobs) are emitted — generic pnpm settings are already surfaced
/// via `aube_settings::resolved::*`, which consults its own env-var
/// aliases. Env entries must be applied *after* `.npmrc` entries so
/// last-write-wins gives env the higher precedence npm/pnpm document.
fn npm_config_env_entries_from(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|(n, v)| translate_npm_config_env(n, v))
        .collect()
}

/// Map a single `npm_config_*` / `NPM_CONFIG_*` env var to the
/// `.npmrc`-style `(key, value)` that [`NpmConfig::apply`] understands.
/// Returns `None` for env vars unrelated to registry-client config —
/// those are owned by the generic settings resolver. Pure function so
/// tests can exercise the mapping without mutating `std::env`.
fn translate_npm_config_env(name: &str, value: &str) -> Option<(String, String)> {
    let suffix = name
        .strip_prefix("npm_config_")
        .or_else(|| name.strip_prefix("NPM_CONFIG_"))?;
    // Per-URI auth keys (e.g. `//registry.example.com/:_authToken`)
    // already carry `.npmrc` syntax in the env-var name. Pass them
    // through unchanged so `apply`'s `starts_with("//")` arm picks
    // them up and preserves the `_authToken` / `_auth` / `username`
    // casing that the match inside it depends on.
    if suffix.starts_with("//") {
        return Some((suffix.to_string(), value.to_string()));
    }
    // Scoped-registry keys: `@myorg:REGISTRY` or `@MYORG:registry`,
    // translated to the canonical `@myorg:registry` form. The scope
    // segment is lowercased because npm scope names are
    // case-insensitive on the registry side, and `apply` matches the
    // `:registry` suffix literally.
    if let Some(rest) = suffix.strip_prefix('@')
        && let Some((scope, tail)) = rest.split_once(':')
        && tail.eq_ignore_ascii_case("registry")
    {
        return Some((
            format!("@{}:registry", scope.to_ascii_lowercase()),
            value.to_string(),
        ));
    }
    // Canonical single-word or `_`-separated multi-word keys. The
    // left column is the lowercased env-suffix (POSIX-style); the
    // right column is the `.npmrc` key `apply` matches on.
    let npmrc_key = match suffix.to_ascii_lowercase().as_str() {
        "registry" => "registry",
        "https_proxy" => "https-proxy",
        "http_proxy" => "http-proxy",
        "proxy" => "proxy",
        "noproxy" => "noproxy",
        "strict_ssl" => "strict-ssl",
        "local_address" => "local-address",
        "maxsockets" => "maxsockets",
        _ => return None,
    };
    Some((npmrc_key.to_string(), value.to_string()))
}

/// Load raw `.npmrc` key/value pairs from the same file precedence as
/// [`NpmConfig::load`]: user-level (`~/.npmrc`) first, then project-level
/// (`<cwd>/.npmrc`). Returned in encounter order — a later duplicate key
/// overrides an earlier one, matching npm's own precedence rules.
///
/// Callers that want typed, per-setting values should consume this via
/// `aube_cli::settings_values`, which walks `settings_meta::SETTINGS` and
/// looks up each setting's declared `sources.npmrc` keys. That keeps the
/// registry of "which keys map to which setting" in `settings.toml`
/// instead of scattering it through a hand-rolled parser.
pub fn load_npmrc_entries(project_dir: &Path) -> Vec<(String, String)> {
    // Read `XDG_CONFIG_HOME` only on the public entry point so that
    // `pnpm` and `aube` agree on where `~/.config/pnpm/auth.ini`
    // resolves when the user has a non-default XDG layout. The env
    // read is confined here — the `_with_home` helper keeps taking an
    // explicit override so tests don't inherit the developer's real
    // `XDG_CONFIG_HOME` and pick up whatever auth tokens live there.
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    load_npmrc_entries_with_home(home_dir().as_deref(), xdg.as_deref(), project_dir)
}

/// Same as [`load_npmrc_entries`] but with an injectable user-home
/// directory and XDG config-home override. Used by tests that need to
/// isolate from the developer's real `~/.npmrc` and pnpm config dir
/// without mutating process-wide environment variables.
fn load_npmrc_entries_with_home(
    home: Option<&Path>,
    xdg_config_home: Option<&Path>,
    project_dir: &Path,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(home) = home {
        let user_rc = home.join(".npmrc");
        if user_rc.exists()
            && let Ok(entries) = parse_npmrc(&user_rc)
        {
            out.extend(entries);
        }
        // pnpm's global auth file: `~/.config/pnpm/auth.ini`. Same
        // `key=value` grammar as `.npmrc`, but lives under the pnpm
        // config dir so a user can keep registry credentials out of
        // `~/.npmrc` (which tooling like `npm login` rewrites). Loaded
        // after the user rc so it overrides any stale token there but
        // before the project rc, which still wins for per-repo pins.
        let auth_ini = pnpm_global_auth_ini_path(home, xdg_config_home);
        if auth_ini.exists()
            && let Ok(entries) = parse_npmrc(&auth_ini)
        {
            out.extend(entries);
        }
    }
    let project_rc = project_dir.join(".npmrc");
    if project_rc.exists()
        && let Ok(entries) = parse_npmrc(&project_rc)
    {
        out.extend(entries);
    }
    // pnpm's `npmrcAuthFile` setting points at an out-of-tree file
    // (typically a CI secret mount or a per-user override) that holds
    // auth tokens. Load it last so anything declared there wins —
    // users who put auth tokens in this file expect them to take
    // precedence over whatever happens to be in `~/.npmrc`.
    if let Some(auth_path) = resolve_npmrc_auth_file(home, project_dir, &out)
        && auth_path.exists()
        && let Ok(entries) = parse_npmrc(&auth_path)
    {
        out.extend(entries);
    }
    out
}

/// Walk the loaded `.npmrc` entries (last-write-wins) for an
/// `npmrcAuthFile` / `npmrc-auth-file` key and resolve it to an
/// absolute path. `~` expands against `home`; relative paths resolve
/// against the project root, matching the storeDir convention.
fn resolve_npmrc_auth_file(
    home: Option<&Path>,
    project_dir: &Path,
    entries: &[(String, String)],
) -> Option<PathBuf> {
    let raw = entries
        .iter()
        .rev()
        .find(|(k, _)| matches!(k.as_str(), "npmrcAuthFile" | "npmrc-auth-file"))
        .map(|(_, v)| v.as_str())?;
    let expanded = if let Some(rest) = raw.strip_prefix("~/") {
        home.map(|h| h.join(rest))?
    } else if raw == "~" {
        home.map(PathBuf::from)?
    } else {
        PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(project_dir.join(expanded))
    }
}

/// Parse a .npmrc file into key=value pairs.
/// Supports environment variable substitution (${VAR}).
fn parse_npmrc(path: &Path) -> Result<Vec<(String, String)>, std::io::Error> {
    let content = std::fs::read_to_string(path)?;
    let mut entries = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = substitute_env(value.trim());
            entries.push((key, value));
        }
    }

    Ok(entries)
}

/// Substitute ${VAR} references with environment variable values.
fn substitute_env(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var_name.push(c);
            }
            if let Ok(val) = std::env::var(&var_name) {
                result.push_str(&val);
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Extract the scope from a package name (e.g., "@myorg/pkg" -> "@myorg").
fn package_scope(name: &str) -> Option<&str> {
    if name.starts_with('@') {
        name.find('/').map(|idx| &name[..idx])
    } else {
        None
    }
}

/// Convert a registry URL to the URI key used in .npmrc for auth lookup.
/// "https://registry.example.com/" -> "//registry.example.com/"
fn registry_uri_key(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https:") {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix("http:") {
        rest.to_string()
    } else {
        url.to_string()
    }
}

/// Public wrapper for normalize_registry_url.
pub fn normalize_registry_url_pub(url: &str) -> String {
    normalize_registry_url(url)
}

/// Public wrapper for [`registry_uri_key`], so callers outside the
/// crate can convert a full registry URL into the `//host[:port]/path/`
/// key `.npmrc` uses for per-registry auth entries without reimplementing
/// the scheme-stripping logic.
pub fn registry_uri_key_pub(url: &str) -> String {
    registry_uri_key(url)
}

/// Ensure registry URL has a trailing slash.
fn normalize_registry_url(url: &str) -> String {
    let url = url.trim();
    if url.ends_with('/') {
        url.to_string()
    } else {
        format!("{url}/")
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

/// Resolve the path to pnpm's global auth file. When an explicit
/// `xdg_config_home` is supplied (production reads it from
/// `$XDG_CONFIG_HOME` in [`load_npmrc_entries`]; tests pass an
/// injected override or `None`), the file lives at
/// `<xdg>/pnpm/auth.ini`. Otherwise it falls back to
/// `<home>/.config/pnpm/auth.ini`, matching pnpm's default layout
/// on Linux and the README's documented path.
fn pnpm_global_auth_ini_path(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    let config_root = xdg_config_home
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    config_root.join("pnpm").join("auth.ini")
}

/// Map an empty string to `None` so a blank `.npmrc` value like
/// `https-proxy=` reliably *unsets* the field instead of installing an
/// unparseable empty URL into the reqwest builder. Trimming matches
/// npm's own line handling.
fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn pem_value(s: String) -> String {
    s.replace("\\n", "\n")
}

/// Return the first set (and non-empty) env var in `names`. Used to
/// read proxy config from both the upper- and lowercase spellings that
/// curl / node conventionally accept.
fn env_any(names: &[&str]) -> Option<String> {
    for n in names {
        if let Ok(v) = std::env::var(n) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                // Trim before returning so a shell-quoted value like
                // `HTTPS_PROXY=" http://proxy "` doesn't slip past
                // `reqwest::Proxy::https` with surrounding whitespace
                // and silently fail.
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub(crate) fn run_token_helper(command: &str) -> Option<String> {
    let output = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", command])
            .output()
            .ok()?
    } else {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .ok()?
    };
    if !output.status.success() {
        tracing::warn!("tokenHelper {command:?} exited with {}", output.status);
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    non_empty(token.lines().next().unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_npmrc_basic() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".npmrc");
        std::fs::write(
            &rc,
            "registry=https://registry.example.com\n_authToken=secret123\n",
        )
        .unwrap();

        let entries = parse_npmrc(&rc).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            (
                "registry".to_string(),
                "https://registry.example.com".to_string()
            )
        );
        assert_eq!(
            entries[1],
            ("_authToken".to_string(), "secret123".to_string())
        );
    }

    #[test]
    fn test_parse_npmrc_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let rc = dir.path().join(".npmrc");
        std::fs::write(
            &rc,
            "# comment\n\n; another comment\nregistry=https://r.com\n",
        )
        .unwrap();

        let entries = parse_npmrc(&rc).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_substitute_env() {
        // Use a unique var name and unsafe block (required in edition 2024)
        unsafe { std::env::set_var("AUBE_TEST_TOKEN_CFG", "mytoken") };
        assert_eq!(substitute_env("${AUBE_TEST_TOKEN_CFG}"), "mytoken");
        assert_eq!(
            substitute_env("prefix-${AUBE_TEST_TOKEN_CFG}-suffix"),
            "prefix-mytoken-suffix"
        );
        assert_eq!(substitute_env("no-vars-here"), "no-vars-here");
        unsafe { std::env::remove_var("AUBE_TEST_TOKEN_CFG") };
    }

    #[test]
    fn test_substitute_env_missing_var() {
        assert_eq!(substitute_env("${AUBE_DEFINITELY_NOT_SET}"), "");
    }

    #[test]
    fn test_package_scope() {
        assert_eq!(package_scope("@myorg/pkg"), Some("@myorg"));
        assert_eq!(package_scope("lodash"), None);
        assert_eq!(package_scope("@types/node"), Some("@types"));
    }

    #[test]
    fn test_registry_uri_key() {
        assert_eq!(
            registry_uri_key("https://registry.example.com/"),
            "//registry.example.com/"
        );
        assert_eq!(
            registry_uri_key("http://localhost:4873/"),
            "//localhost:4873/"
        );
    }

    #[test]
    fn test_normalize_registry_url() {
        assert_eq!(normalize_registry_url("https://r.com"), "https://r.com/");
        assert_eq!(normalize_registry_url("https://r.com/"), "https://r.com/");
    }

    #[test]
    fn test_config_load_project_npmrc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "registry=https://custom.registry.com\n\
             @myorg:registry=https://myorg.registry.com\n\
             //myorg.registry.com/:_authToken=org-secret\n\
             //custom.registry.com/:_authToken=custom-secret\n",
        )
        .unwrap();

        // HOME + env isolation: `NpmConfig::load` would layer the
        // developer's real `~/.npmrc` and `NPM_CONFIG_REGISTRY` env
        // var on top of the project file, either of which can shadow
        // the `registry=` we're asserting on.
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));

        assert_eq!(config.registry, "https://custom.registry.com/");
        assert_eq!(
            config.registry_for("@myorg/pkg"),
            "https://myorg.registry.com/"
        );
        assert_eq!(
            config.registry_for("lodash"),
            "https://custom.registry.com/"
        );
        assert_eq!(
            config.auth_token_for("https://myorg.registry.com/"),
            Some("org-secret")
        );
        assert_eq!(
            config.auth_token_for("https://custom.registry.com/"),
            Some("custom-secret")
        );
    }

    #[test]
    fn split_username_password_auth_resolves_to_basic_header_payload() {
        let dir = tempfile::tempdir().unwrap();
        let encoded_password = base64::engine::general_purpose::STANDARD.encode("s3cr3t");
        std::fs::write(
            dir.path().join(".npmrc"),
            format!(
                "//registry.example.com/:username=alice\n\
                 //registry.example.com/:_password={encoded_password}\n"
            ),
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig::default();
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        let expected = base64::engine::general_purpose::STANDARD.encode("alice:s3cr3t");
        assert_eq!(
            config.basic_auth_for("https://registry.example.com/"),
            Some(expected),
        );
    }

    #[test]
    fn token_helper_resolves_external_command_stdout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "//registry.example.com/:tokenHelper=printf helper-token\n",
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig::default();
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert_eq!(
            config.token_helper_for("https://registry.example.com/"),
            Some("printf helper-token"),
        );
        assert_eq!(
            run_token_helper(
                config
                    .token_helper_for("https://registry.example.com/")
                    .unwrap()
            ),
            Some("helper-token".to_string())
        );
    }

    #[test]
    fn per_registry_tls_config_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "//registry.example.com/:ca=-----BEGIN CERTIFICATE-----\\nca\\n-----END CERTIFICATE-----\n\
             //registry.example.com/:cafile=corp-ca.pem\n\
             //registry.example.com/:cert=-----BEGIN CERTIFICATE-----\\nclient\\n-----END CERTIFICATE-----\n\
             //registry.example.com/:key=-----BEGIN PRIVATE KEY-----\\nkey\\n-----END PRIVATE KEY-----\n",
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig::default();
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        let tls = &config
            .registry_config_for("https://registry.example.com/")
            .expect("registry config")
            .tls;
        assert_eq!(tls.ca.len(), 1);
        assert!(tls.ca[0].contains("\nca\n"));
        assert!(!tls.ca[0].contains("\\n"));
        assert_eq!(tls.cafile.as_deref(), Some(Path::new("corp-ca.pem")));
        assert!(tls.cert.as_deref().unwrap().contains("\nclient\n"));
        assert!(tls.key.as_deref().unwrap().contains("\nkey\n"));
    }

    #[test]
    fn test_config_global_auth_token() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".npmrc"), "_authToken=global-token\n").unwrap();

        // Isolate from the host's real `~/.npmrc`: a developer or CI
        // runner with `//registry.npmjs.org/:_authToken=...` already
        // logged in would have that URI-specific token beat our
        // project-level `_authToken` fallback, since `auth_token_for`
        // checks per-URI auth before dropping to `global_auth_token`.
        // `NpmConfig::load` is the host-reading entry point we
        // deliberately avoid here; `load_npmrc_entries_with_home` lets
        // us pin the user home to a tempdir without mutating
        // process-wide env vars.
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        // Global token used as fallback
        assert_eq!(
            config.auth_token_for("https://registry.npmjs.org/"),
            Some("global-token")
        );
    }

    #[test]
    fn test_config_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // No .npmrc at all. Same HOME isolation rationale as
        // `test_config_global_auth_token` — without it this assertion
        // flakes on any developer box whose `~/.npmrc` has ever been
        // touched by `npm login`.
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert_eq!(config.registry, "https://registry.npmjs.org/");
        assert!(
            config
                .auth_token_for("https://registry.npmjs.org/")
                .is_none()
        );
    }

    #[test]
    fn test_config_scoped_registry_without_auth() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "@private:registry=https://private.registry.com\n",
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig::default();
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert_eq!(
            config.registry_for("@private/my-lib"),
            "https://private.registry.com/"
        );
        assert!(
            config
                .auth_token_for("https://private.registry.com/")
                .is_none()
        );
    }

    #[test]
    fn test_http_proxy_inherits_https_proxy() {
        // pnpm's fallback: `httpProxy` inherits whatever `httpsProxy`
        // resolved to when no HTTP-specific value is configured,
        // so a single `https-proxy=` line configures both schemes.
        //
        // We scrub the proxy env vars inside the `apply_proxy_env`
        // helper's view by staging the field value directly: the
        // real resolver is pure once `https_proxy` is already set,
        // so `env_any` is never consulted for the HTTPS half and
        // this assertion can't race a developer's shell.
        let mut config = NpmConfig {
            https_proxy: Some("http://corp.proxy:8080".to_string()),
            ..Default::default()
        };
        // Drop any ambient `HTTP_PROXY` so the second `or_else` in
        // `apply_proxy_env` can't beat us to the fallback. We can't
        // use `std::env::remove_var` safely across parallel tests;
        // instead, pre-populate `http_proxy` to `None` and rely on
        // the field-level fallback only.
        // Since `https_proxy` is already `Some`, the resolver takes
        // that branch first — `env_any("HTTP_PROXY", ...)` is never
        // called.
        config.apply_proxy_env();
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://corp.proxy:8080"),
            "http_proxy must inherit https_proxy"
        );
    }

    #[test]
    fn test_npmrc_proxy_key_feeds_https_proxy() {
        // pnpm treats `.npmrc proxy=` as the fallback for
        // `httpsProxy`, not as a direct alias for `httpProxy`.
        let mut config = NpmConfig {
            npmrc_proxy: Some("http://legacy:3128".to_string()),
            ..Default::default()
        };
        config.apply_proxy_env();
        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://legacy:3128"),
            "legacy `proxy=` key must resolve into https_proxy"
        );
        assert_eq!(
            config.http_proxy.as_deref(),
            Some("http://legacy:3128"),
            "http_proxy then inherits the resolved https_proxy"
        );
    }

    #[test]
    fn test_explicit_https_proxy_wins_over_npmrc_proxy() {
        let mut config = NpmConfig {
            https_proxy: Some("http://explicit:1".to_string()),
            npmrc_proxy: Some("http://fallback:2".to_string()),
            ..Default::default()
        };
        config.apply_proxy_env();
        assert_eq!(config.https_proxy.as_deref(), Some("http://explicit:1"));
    }

    #[test]
    fn test_default_strict_ssl_is_true() {
        // Regression: `NpmConfig::default()` must not leave
        // `strict_ssl = false` (bool::default), because
        // `RegistryClient::new` spreads the default and would
        // otherwise silently disable TLS cert validation.
        let c = NpmConfig::default();
        assert!(c.strict_ssl);
    }

    #[test]
    fn test_parses_proxy_and_ssl_settings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "https-proxy=http://proxy.example.com:8080\n\
             proxy=http://plain.example.com:3128\n\
             noproxy=localhost,.internal\n\
             strict-ssl=false\n\
             local-address=127.0.0.1\n\
             maxsockets=12\n",
        )
        .unwrap();

        // Isolate from the developer's real ~/.npmrc
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            registry: "https://registry.npmjs.org/".to_string(),
            strict_ssl: true,
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));

        assert_eq!(
            config.https_proxy.as_deref(),
            Some("http://proxy.example.com:8080")
        );
        // `.npmrc proxy=` stores into `npmrc_proxy`, which feeds
        // `https_proxy`/`http_proxy` only via `apply_proxy_env`. We
        // called raw `apply` here, so the field is still the
        // verbatim legacy key.
        assert_eq!(
            config.npmrc_proxy.as_deref(),
            Some("http://plain.example.com:3128")
        );
        assert!(config.http_proxy.is_none());
        assert_eq!(config.no_proxy.as_deref(), Some("localhost,.internal"));
        assert!(!config.strict_ssl);
        assert_eq!(
            config.local_address,
            Some("127.0.0.1".parse::<std::net::IpAddr>().unwrap())
        );
        assert_eq!(config.max_sockets, Some(12));
    }

    #[test]
    fn test_strict_ssl_default_true() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".npmrc"), "").unwrap();
        let mut config = NpmConfig {
            strict_ssl: true,
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert!(config.strict_ssl);
    }

    #[test]
    fn test_camel_case_proxy_aliases() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "httpsProxy=http://a\nhttpProxy=http://b\nnoProxy=foo\nstrictSsl=false\nlocalAddress=::1\n",
        )
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            strict_ssl: true,
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert_eq!(config.https_proxy.as_deref(), Some("http://a"));
        assert_eq!(config.http_proxy.as_deref(), Some("http://b"));
        assert_eq!(config.no_proxy.as_deref(), Some("foo"));
        assert!(!config.strict_ssl);
        assert_eq!(
            config.local_address,
            Some("::1".parse::<std::net::IpAddr>().unwrap())
        );
    }

    #[test]
    fn test_invalid_proxy_values_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "local-address=not-an-ip\nmaxsockets=zero\nstrict-ssl=perhaps\n",
        )
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let mut config = NpmConfig {
            strict_ssl: true,
            ..Default::default()
        };
        config.apply(load_npmrc_entries_with_home(
            Some(home.path()),
            None,
            dir.path(),
        ));
        assert!(config.local_address.is_none());
        assert!(config.max_sockets.is_none());
        // Garbage boolean leaves the previous value in place.
        assert!(config.strict_ssl);
    }

    // `auto-install-peers` parsing lives in aube's settings_values
    // module now — see tests there. NpmConfig only knows about
    // registry-client config (URL, auth, scopes).

    #[test]
    fn test_load_npmrc_entries_orders_user_before_project() {
        // The downstream settings resolver iterates the returned Vec in
        // reverse to give project-level entries priority, so the
        // invariant this test pins is specifically the ordering: user
        // entries MUST appear before project entries for the same key.
        //
        // Uses `load_npmrc_entries_with_home` (test-only helper) to
        // inject a fake user home rather than mutating `$HOME` on the
        // process, which would race with any other test reading env.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            home_dir.path().join(".npmrc"),
            "auto-install-peers=true\nfoo=user-only\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            "auto-install-peers=false\nbar=project-only\n",
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());

        // Both keys from each file are present.
        assert!(entries.iter().any(|(k, v)| k == "foo" && v == "user-only"));
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "bar" && v == "project-only")
        );

        // The shared key appears twice, in the right order.
        let positions: Vec<_> = entries
            .iter()
            .filter(|(k, _)| k == "auto-install-peers")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            positions.len(),
            2,
            "expected both user and project entries for shared key: {entries:?}"
        );
        assert_eq!(
            positions[0], "true",
            "user entry must come first (precedence is last-write-wins downstream)"
        );
        assert_eq!(
            positions[1], "false",
            "project entry must come second so it overrides the user entry"
        );
    }

    #[test]
    fn pnpm_global_auth_ini_loads_and_overrides_user_rc() {
        // `~/.config/pnpm/auth.ini` is pnpm's out-of-band credential
        // file. Aube needs to read it so users who stash tokens there
        // (to keep them out of `~/.npmrc`) don't get "401 Unauthorized"
        // on a fresh clone. It should beat `~/.npmrc` for the same
        // key, since the entire reason to use it is to override
        // whatever npm-side tooling writes to `.npmrc`.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();

        std::fs::write(
            home_dir.path().join(".npmrc"),
            "//registry.example.com/:_authToken=stale-npmrc\n",
        )
        .unwrap();
        let auth_ini = home_dir.path().join(".config/pnpm/auth.ini");
        std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
        std::fs::write(
            &auth_ini,
            "//registry.example.com/:_authToken=fresh-auth-ini\n\
             //other.example.com/:_authToken=other-token\n",
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        let mut cfg = NpmConfig::default();
        cfg.apply(entries);
        assert_eq!(
            cfg.auth_token_for("https://registry.example.com/"),
            Some("fresh-auth-ini"),
            "auth.ini token should override stale ~/.npmrc token",
        );
        assert_eq!(
            cfg.auth_token_for("https://other.example.com/"),
            Some("other-token"),
            "additional auth.ini entries should be picked up",
        );
    }

    #[test]
    fn pnpm_global_auth_ini_honors_xdg_config_home_override() {
        // When `XDG_CONFIG_HOME` is set, pnpm reads
        // `$XDG_CONFIG_HOME/pnpm/auth.ini` instead of
        // `$HOME/.config/pnpm/auth.ini`. Aube must match, or a user
        // with a custom XDG layout will see pnpm and aube disagree on
        // where credentials live. The injected override here is the
        // same value `load_npmrc_entries` reads from the real env var.
        let home_dir = tempfile::tempdir().unwrap();
        let xdg_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();

        let auth_ini = xdg_dir.path().join("pnpm/auth.ini");
        std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
        std::fs::write(&auth_ini, "//registry.example.com/:_authToken=xdg-token\n").unwrap();
        // Decoy at the default `$HOME/.config/pnpm/auth.ini` location
        // to prove the XDG override replaces the fallback instead of
        // being merged alongside it.
        let decoy = home_dir.path().join(".config/pnpm/auth.ini");
        std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();
        std::fs::write(&decoy, "//registry.example.com/:_authToken=decoy\n").unwrap();

        let entries = load_npmrc_entries_with_home(
            Some(home_dir.path()),
            Some(xdg_dir.path()),
            proj_dir.path(),
        );
        let mut cfg = NpmConfig::default();
        cfg.apply(entries);
        assert_eq!(
            cfg.auth_token_for("https://registry.example.com/"),
            Some("xdg-token"),
        );
    }

    #[test]
    fn pnpm_global_auth_ini_loses_to_project_npmrc() {
        // Project `.npmrc` pins still win — per-repo configuration is
        // the most specific layer, and a user's global auth.ini
        // must not clobber a token a project explicitly set.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();

        let auth_ini = home_dir.path().join(".config/pnpm/auth.ini");
        std::fs::create_dir_all(auth_ini.parent().unwrap()).unwrap();
        std::fs::write(
            &auth_ini,
            "//registry.example.com/:_authToken=global-auth-ini\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            "//registry.example.com/:_authToken=project-pin\n",
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        let mut cfg = NpmConfig::default();
        cfg.apply(entries);
        assert_eq!(
            cfg.auth_token_for("https://registry.example.com/"),
            Some("project-pin"),
        );
    }

    #[test]
    fn npmrc_auth_file_overrides_user_token() {
        // The whole point of `npmrcAuthFile`: a token declared in the
        // out-of-tree auth file must beat the same token in `~/.npmrc`,
        // so CI can mount a secret-bearing file at a fixed path and
        // know it wins regardless of any leftover entries in user rc.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();
        let auth_file = proj_dir.path().join("auth.npmrc");

        std::fs::write(
            home_dir.path().join(".npmrc"),
            "//registry.example.com/:_authToken=stale-user-token\n",
        )
        .unwrap();
        std::fs::write(
            &auth_file,
            "//registry.example.com/:_authToken=fresh-from-auth-file\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            format!("npmrc-auth-file={}\n", auth_file.display()),
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        let mut cfg = NpmConfig::default();
        cfg.apply(entries);
        assert_eq!(
            cfg.auth_token_for("https://registry.example.com/"),
            Some("fresh-from-auth-file"),
        );
    }

    #[test]
    fn npmrc_auth_file_resolves_relative_to_project_root() {
        // A relative `npmrc-auth-file` path should resolve against the
        // project root, NOT the cwd of the test runner — same convention
        // as the storeDir setting.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj_dir.path().join("secrets")).unwrap();
        std::fs::write(
            proj_dir.path().join("secrets/npm"),
            "//registry.example.com/:_authToken=relative-path-token\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            "npmrc-auth-file=secrets/npm\n",
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "//registry.example.com/:_authToken"
                    && v == "relative-path-token"),
            "auth file entries missing — got {entries:?}",
        );
    }

    #[test]
    fn npmrc_auth_file_camel_case_alias_works() {
        // The kebab-case spelling is exercised by the other tests; pin
        // the camelCase alias separately so a future tweak to the
        // `matches!` arm can't silently drop one of the spellings.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();
        let auth_file = proj_dir.path().join("auth.npmrc");

        std::fs::write(
            &auth_file,
            "//registry.example.com/:_authToken=camel-token\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            format!("npmrcAuthFile={}\n", auth_file.display()),
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "//registry.example.com/:_authToken" && v == "camel-token"),
            "camelCase alias did not load auth file — got {entries:?}",
        );
    }

    #[test]
    fn npmrc_auth_file_expands_tilde_against_home() {
        // `~/secrets/npm` should expand to `<home>/secrets/npm`, mirroring
        // the storeDir / pnpm convention.
        let home_dir = tempfile::tempdir().unwrap();
        let proj_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home_dir.path().join("secrets")).unwrap();
        std::fs::write(
            home_dir.path().join("secrets/npm"),
            "//registry.example.com/:_authToken=tilde-token\n",
        )
        .unwrap();
        std::fs::write(
            proj_dir.path().join(".npmrc"),
            "npmrc-auth-file=~/secrets/npm\n",
        )
        .unwrap();

        let entries = load_npmrc_entries_with_home(Some(home_dir.path()), None, proj_dir.path());
        assert!(
            entries
                .iter()
                .any(|(k, v)| k == "//registry.example.com/:_authToken" && v == "tilde-token"),
            "tilde expansion failed — got {entries:?}",
        );
    }

    #[test]
    fn fetch_policy_default_matches_settings_toml_declared_defaults() {
        // `settings.toml` declares these defaults; `FetchPolicy::default`
        // must match them verbatim so callers that skip
        // `FetchPolicy::from_ctx` still get pnpm-compatible behavior.
        let p = FetchPolicy::default();
        assert_eq!(p.timeout_ms, 60_000);
        assert_eq!(p.retries, 2);
        assert_eq!(p.retry_factor, 10);
        assert_eq!(p.retry_min_timeout_ms, 10_000);
        assert_eq!(p.retry_max_timeout_ms, 60_000);
    }

    #[test]
    fn fetch_policy_backoff_sequence_matches_make_fetch_happen() {
        // Defaults: min=10s, factor=10, max=60s. Sequence:
        //   attempt 1 → 10s  (10 * 10^0 = 10)
        //   attempt 2 → 60s  (10 * 10^1 = 100 → clamped to 60)
        //   attempt 3 → 60s  (10 * 10^2 = 1000 → clamped to 60)
        let p = FetchPolicy::default();
        assert_eq!(
            p.backoff_for_attempt(1),
            std::time::Duration::from_millis(10_000)
        );
        assert_eq!(
            p.backoff_for_attempt(2),
            std::time::Duration::from_millis(60_000)
        );
        assert_eq!(
            p.backoff_for_attempt(3),
            std::time::Duration::from_millis(60_000)
        );
    }

    #[test]
    fn fetch_policy_backoff_clamps_on_huge_factor() {
        // Saturating math: even `factor=u32::MAX` doesn't panic; the
        // first retry hits the max ceiling and stays there.
        let p = FetchPolicy {
            timeout_ms: 60_000,
            retries: 5,
            retry_factor: u32::MAX,
            retry_min_timeout_ms: 100,
            retry_max_timeout_ms: 5_000,
            ..FetchPolicy::default()
        };
        assert_eq!(
            p.backoff_for_attempt(1),
            std::time::Duration::from_millis(100),
            "first attempt is the min (no multiplier applied yet)",
        );
        assert_eq!(
            p.backoff_for_attempt(2),
            std::time::Duration::from_millis(5_000),
        );
        assert_eq!(
            p.backoff_for_attempt(10),
            std::time::Duration::from_millis(5_000),
            "deep retries still clamp; no overflow panic",
        );
    }

    #[test]
    fn fetch_policy_from_ctx_reads_npmrc_overrides() {
        // Full precedence chain is tested in `aube_settings`; this test
        // just proves the composite struct wires each field through to
        // the right generated accessor.
        let entries = vec![
            ("fetchTimeout".to_string(), "1234".to_string()),
            ("fetch-retries".to_string(), "5".to_string()),
            ("fetch-retry-factor".to_string(), "3".to_string()),
            ("fetch-retry-mintimeout".to_string(), "250".to_string()),
            ("fetch-retry-maxtimeout".to_string(), "9_999".to_string()),
        ];
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
        let p = FetchPolicy::from_ctx(&ctx);
        assert_eq!(p.timeout_ms, 1234);
        assert_eq!(p.retries, 5);
        assert_eq!(p.retry_factor, 3);
        assert_eq!(p.retry_min_timeout_ms, 250);
        // `9_999` with the underscore doesn't parse as u64 under the
        // generic `str::parse`; the accessor falls through to the
        // declared default. Assert that to lock the behavior.
        assert_eq!(p.retry_max_timeout_ms, 60_000);
    }

    #[test]
    fn fetch_policy_from_ctx_reads_warn_timeout_and_min_speed() {
        // Pin the wiring for the two observability knobs. `from_ctx`
        // must route each through its generated accessor or a later
        // rename in the build script will silently fall back to the
        // declared default.
        let entries = vec![
            ("fetchWarnTimeoutMs".to_string(), "500".to_string()),
            ("fetchMinSpeedKiBps".to_string(), "123".to_string()),
        ];
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
        let p = FetchPolicy::from_ctx(&ctx);
        assert_eq!(p.warn_timeout_ms, 500);
        assert_eq!(p.min_speed_kibps, 123);
    }

    #[test]
    fn fetch_policy_default_includes_observability_thresholds() {
        // Regression lock: the `settings.toml` defaults for the two
        // observability knobs (10s warn threshold, 50 KiB/s floor) must
        // remain reflected in `FetchPolicy::default()` so callers that
        // skip `from_ctx` still behave like a default-configured pnpm.
        let p = FetchPolicy::default();
        assert_eq!(p.warn_timeout_ms, 10_000);
        assert_eq!(p.min_speed_kibps, 50);
    }

    #[test]
    fn translate_npm_config_env_maps_default_registry() {
        // Both the lowercase and SCREAMING_SNAKE spellings must land
        // on the canonical `.npmrc` key `registry`. The docs promise
        // `NPM_CONFIG_REGISTRY aube install` works; this is the hook
        // that makes it true.
        assert_eq!(
            translate_npm_config_env("NPM_CONFIG_REGISTRY", "https://r.example/"),
            Some(("registry".to_string(), "https://r.example/".to_string()))
        );
        assert_eq!(
            translate_npm_config_env("npm_config_registry", "https://r.example/"),
            Some(("registry".to_string(), "https://r.example/".to_string()))
        );
        // Non-npm env vars are ignored so the entry list stays tight
        // and `apply` isn't fed noise.
        assert_eq!(translate_npm_config_env("HOME", "/tmp"), None);
    }

    #[test]
    fn translate_npm_config_env_maps_proxy_and_tls_knobs() {
        // Multi-word env suffix → hyphenated `.npmrc` key. Pins the
        // mapping for every registry-client knob that's exposed via
        // an env alias so future regressions show up as test
        // failures, not silent drops.
        let cases = [
            ("NPM_CONFIG_HTTPS_PROXY", "http://p:8", "https-proxy"),
            ("NPM_CONFIG_HTTP_PROXY", "http://p:9", "http-proxy"),
            ("NPM_CONFIG_PROXY", "http://p:0", "proxy"),
            ("NPM_CONFIG_NOPROXY", "localhost,.internal", "noproxy"),
            ("NPM_CONFIG_STRICT_SSL", "false", "strict-ssl"),
            ("NPM_CONFIG_LOCAL_ADDRESS", "127.0.0.1", "local-address"),
            ("NPM_CONFIG_MAXSOCKETS", "16", "maxsockets"),
        ];
        for (name, value, expected_key) in cases {
            assert_eq!(
                translate_npm_config_env(name, value),
                Some((expected_key.to_string(), value.to_string())),
                "mapping failed for {name}"
            );
        }
    }

    #[test]
    fn translate_npm_config_env_maps_scoped_registry() {
        // `NPM_CONFIG_@MYORG:REGISTRY` should normalise to the
        // lowercase canonical `@myorg:registry` key that `apply`
        // matches via `strip_suffix(":registry")`.
        assert_eq!(
            translate_npm_config_env("NPM_CONFIG_@MYORG:REGISTRY", "https://r.mycorp/"),
            Some((
                "@myorg:registry".to_string(),
                "https://r.mycorp/".to_string()
            ))
        );
        assert_eq!(
            translate_npm_config_env("npm_config_@myorg:registry", "https://r.mycorp/"),
            Some((
                "@myorg:registry".to_string(),
                "https://r.mycorp/".to_string()
            ))
        );
    }

    #[test]
    fn translate_npm_config_env_passes_uri_auth_through_verbatim() {
        // Per-URI auth keys carry `.npmrc` syntax in the env name.
        // Passthrough preserves the `_authToken` casing that `apply`
        // matches inside its `starts_with("//")` branch.
        assert_eq!(
            translate_npm_config_env(
                "NPM_CONFIG_//registry.example.com/:_authToken",
                "secret-token"
            ),
            Some((
                "//registry.example.com/:_authToken".to_string(),
                "secret-token".to_string()
            ))
        );
    }

    #[test]
    fn load_with_env_npm_config_registry_overrides_project_file() {
        // Integration-ish: `load_with_env` stitches file config and
        // env together. Project `.npmrc` sets one registry URL; the
        // captured env carries `NPM_CONFIG_REGISTRY` with another.
        // The env value must win so the code path a user exercises
        // with `NPM_CONFIG_REGISTRY=... aube install` really does
        // route traffic to the configured host.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".npmrc"),
            "registry=https://file.registry.example/\n",
        )
        .unwrap();
        let env = vec![(
            "NPM_CONFIG_REGISTRY".to_string(),
            "https://env.registry.example/".to_string(),
        )];
        let config = NpmConfig::load_with_env(dir.path(), &env);
        assert_eq!(config.registry, "https://env.registry.example/");
    }

    #[test]
    fn env_registry_overrides_project_npmrc() {
        // End-to-end: `apply` consumes the synthesised env entry last,
        // so a `NPM_CONFIG_REGISTRY` value beats whatever the project
        // `.npmrc` declares. This is the behaviour the user-facing
        // docs (`docs/package-manager/configuration.md`) guarantee.
        //
        // Driven through `apply` directly to avoid racing other tests
        // on the process-wide env (edition 2024 requires `unsafe` for
        // `set_var`, and the test harness runs cases in parallel).
        let mut config = NpmConfig {
            registry: "https://registry.npmjs.org/".to_string(),
            ..Default::default()
        };
        config.apply(vec![(
            "registry".to_string(),
            "https://file.registry/".to_string(),
        )]);
        assert_eq!(config.registry, "https://file.registry/");
        // Emulate the `load_npm_config_env_entries` output for
        // `NPM_CONFIG_REGISTRY=https://env.registry/`.
        let env = translate_npm_config_env("NPM_CONFIG_REGISTRY", "https://env.registry/")
            .map(|e| vec![e])
            .unwrap_or_default();
        config.apply(env);
        assert_eq!(
            config.registry, "https://env.registry/",
            "env var must override file-based registry"
        );
    }

    #[test]
    fn fetch_policy_clamps_giant_retry_counts_into_u32() {
        // A user writing `fetch-retries=99999999999` should not panic;
        // the retry loop just caps at u32::MAX attempts.
        let entries = vec![("fetch-retries".to_string(), "99999999999999".to_string())];
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = aube_settings::ResolveCtx::files_only(&entries, &ws);
        let p = FetchPolicy::from_ctx(&ctx);
        assert_eq!(p.retries, u32::MAX);
    }
}
