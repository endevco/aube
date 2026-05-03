//! Warning codes (`WARN_AUBE_*`).
//!
//! Same shape as `errors`: each constant value matches its
//! identifier; `ALL` carries the [`crate::CodeMeta`] entries the
//! generated docs page and self-tests consume. Warnings never
//! change exit status, so `exit_code` is always `None` here.

use crate::CodeMeta;

// ── pnpmfile / hooks ────────────────────────────────────────────────
pub const WARN_AUBE_PNPMFILE_NOT_FOUND: &str = "WARN_AUBE_PNPMFILE_NOT_FOUND";
pub const WARN_AUBE_PNPMFILE_STDERR_FORWARDER: &str = "WARN_AUBE_PNPMFILE_STDERR_FORWARDER";
pub const WARN_AUBE_HOOK_IMPORTER_MUTATED: &str = "WARN_AUBE_HOOK_IMPORTER_MUTATED";
pub const WARN_AUBE_HOOK_IMPORTER_ADDED: &str = "WARN_AUBE_HOOK_IMPORTER_ADDED";
pub const WARN_AUBE_HOOK_IDENTITY_REWRITTEN: &str = "WARN_AUBE_HOOK_IDENTITY_REWRITTEN";
pub const WARN_AUBE_HOOK_PACKAGE_ADDED: &str = "WARN_AUBE_HOOK_PACKAGE_ADDED";

// ── install lifecycle ───────────────────────────────────────────────
pub const WARN_AUBE_IGNORED_BUILD_SCRIPTS: &str = "WARN_AUBE_IGNORED_BUILD_SCRIPTS";
pub const WARN_AUBE_MISSING_INTEGRITY: &str = "WARN_AUBE_MISSING_INTEGRITY";
pub const WARN_AUBE_CACHE_WRITE_FAILED: &str = "WARN_AUBE_CACHE_WRITE_FAILED";
pub const WARN_AUBE_CLONE_STRATEGY_FALLBACK: &str = "WARN_AUBE_CLONE_STRATEGY_FALLBACK";
pub const WARN_AUBE_LTHASH_MISMATCH: &str = "WARN_AUBE_LTHASH_MISMATCH";
pub const WARN_AUBE_DELTA_INVALIDATE_FAILED: &str = "WARN_AUBE_DELTA_INVALIDATE_FAILED";
pub const WARN_AUBE_GVS_INCOMPATIBLE: &str = "WARN_AUBE_GVS_INCOMPATIBLE";
pub const WARN_AUBE_GVS_MODE_CHANGED: &str = "WARN_AUBE_GVS_MODE_CHANGED";

// ── settings / config validation ────────────────────────────────────
pub const WARN_AUBE_INVALID_CONCURRENCY: &str = "WARN_AUBE_INVALID_CONCURRENCY";
pub const WARN_AUBE_INVALID_TRUST_POLICY: &str = "WARN_AUBE_INVALID_TRUST_POLICY";
pub const WARN_AUBE_OVERRIDE_MISSING_DEP: &str = "WARN_AUBE_OVERRIDE_MISSING_DEP";
pub const WARN_AUBE_INVALID_PEER_PATTERN: &str = "WARN_AUBE_INVALID_PEER_PATTERN";
pub const WARN_AUBE_INVALID_SAVE_PREFIX: &str = "WARN_AUBE_INVALID_SAVE_PREFIX";
pub const WARN_AUBE_CONCURRENCY_ENV_INVALID: &str = "WARN_AUBE_CONCURRENCY_ENV_INVALID";

// ── update / prerelease ─────────────────────────────────────────────
pub const WARN_AUBE_PRERELEASE_CHECK_SKIPPED: &str = "WARN_AUBE_PRERELEASE_CHECK_SKIPPED";

// ── audit / npmrc ───────────────────────────────────────────────────
pub const WARN_AUBE_AUDIT_FETCH_FAILED: &str = "WARN_AUBE_AUDIT_FETCH_FAILED";
pub const WARN_AUBE_TOKEN_CHMOD_FAILED: &str = "WARN_AUBE_TOKEN_CHMOD_FAILED";

// ── registry config (trust gates + validation) ──────────────────────
pub const WARN_AUBE_UNTRUSTED_PROXY: &str = "WARN_AUBE_UNTRUSTED_PROXY";
pub const WARN_AUBE_UNTRUSTED_STRICT_SSL_DISABLE: &str = "WARN_AUBE_UNTRUSTED_STRICT_SSL_DISABLE";
pub const WARN_AUBE_INVALID_LOCAL_ADDRESS: &str = "WARN_AUBE_INVALID_LOCAL_ADDRESS";
pub const WARN_AUBE_INVALID_MAXSOCKETS: &str = "WARN_AUBE_INVALID_MAXSOCKETS";
pub const WARN_AUBE_UNTRUSTED_TOKEN_HELPER: &str = "WARN_AUBE_UNTRUSTED_TOKEN_HELPER";
pub const WARN_AUBE_INVALID_TOKEN_HELPER: &str = "WARN_AUBE_INVALID_TOKEN_HELPER";
pub const WARN_AUBE_TOKEN_HELPER_SPAWN_FAILED: &str = "WARN_AUBE_TOKEN_HELPER_SPAWN_FAILED";
pub const WARN_AUBE_TOKEN_HELPER_NON_ZERO_EXIT: &str = "WARN_AUBE_TOKEN_HELPER_NON_ZERO_EXIT";

// ── registry HTTP retries ───────────────────────────────────────────
pub const WARN_AUBE_HTTP_RETRY_TRANSIENT: &str = "WARN_AUBE_HTTP_RETRY_TRANSIENT";
pub const WARN_AUBE_HTTP_RETRY_TRANSPORT: &str = "WARN_AUBE_HTTP_RETRY_TRANSPORT";
pub const WARN_AUBE_HTTP_RETRY_BODY_READ: &str = "WARN_AUBE_HTTP_RETRY_BODY_READ";
pub const WARN_AUBE_HTTP_RETRY_BODY_DECODE: &str = "WARN_AUBE_HTTP_RETRY_BODY_DECODE";

// ── registry caching / perf ─────────────────────────────────────────
pub const WARN_AUBE_PACKUMENT_CACHE_WRITE: &str = "WARN_AUBE_PACKUMENT_CACHE_WRITE";
pub const WARN_AUBE_SLOW_METADATA: &str = "WARN_AUBE_SLOW_METADATA";
pub const WARN_AUBE_SLOW_TARBALL: &str = "WARN_AUBE_SLOW_TARBALL";

// ── registry TLS / proxy config ─────────────────────────────────────
pub const WARN_AUBE_INVALID_HTTPS_PROXY: &str = "WARN_AUBE_INVALID_HTTPS_PROXY";
pub const WARN_AUBE_INVALID_HTTP_PROXY: &str = "WARN_AUBE_INVALID_HTTP_PROXY";
pub const WARN_AUBE_INVALID_CA: &str = "WARN_AUBE_INVALID_CA";
pub const WARN_AUBE_INVALID_CAFILE: &str = "WARN_AUBE_INVALID_CAFILE";
pub const WARN_AUBE_UNREADABLE_CAFILE: &str = "WARN_AUBE_UNREADABLE_CAFILE";
pub const WARN_AUBE_INVALID_CLIENT_CERT: &str = "WARN_AUBE_INVALID_CLIENT_CERT";

// ── resolver ────────────────────────────────────────────────────────
pub const WARN_AUBE_UNSUPPORTED_PLATFORM_INSTALL: &str = "WARN_AUBE_UNSUPPORTED_PLATFORM_INSTALL";
pub const WARN_AUBE_EXOTIC_SUBDEP_SKIPPED: &str = "WARN_AUBE_EXOTIC_SUBDEP_SKIPPED";
pub const WARN_AUBE_PEER_DEDUPE_COLLISION: &str = "WARN_AUBE_PEER_DEDUPE_COLLISION";

// ── lockfile ────────────────────────────────────────────────────────
pub const WARN_AUBE_LOCKFILE_MERGE_CONFLICT: &str = "WARN_AUBE_LOCKFILE_MERGE_CONFLICT";
pub const WARN_AUBE_LOCKFILE_MERGE_CLEANUP_FAILED: &str = "WARN_AUBE_LOCKFILE_MERGE_CLEANUP_FAILED";
pub const WARN_AUBE_YARN_BERRY_UNSUPPORTED: &str = "WARN_AUBE_YARN_BERRY_UNSUPPORTED";

/// Stable category labels that group codes in the generated docs.
/// Public so the docs generator can iterate them deterministically.
pub mod category {
    pub const PNPMFILE_HOOKS: &str = "pnpmfile / hooks";
    pub const INSTALL_LIFECYCLE: &str = "Install lifecycle";
    pub const SETTINGS_CONFIG: &str = "Settings / config validation";
    pub const UPDATE_PRERELEASE: &str = "Update / prerelease";
    pub const AUDIT_NPMRC: &str = "Audit / npmrc";
    pub const REGISTRY_CONFIG: &str = "Registry config (trust gates)";
    pub const HTTP_RETRIES: &str = "Registry HTTP retries";
    pub const REGISTRY_PERF: &str = "Registry caching / perf";
    pub const REGISTRY_TLS: &str = "Registry TLS / proxy";
    pub const RESOLVER: &str = "Resolver";
    pub const LOCKFILE: &str = "Lockfile";
}

/// Registry of every warning code with its category and description.
/// Walked by the `generate-error-codes-docs` binary and by the
/// self-tests in `lib.rs`. New codes must be added here.
pub const ALL: &[CodeMeta] = &[
    // pnpmfile / hooks
    CodeMeta {
        name: WARN_AUBE_PNPMFILE_NOT_FOUND,
        category: category::PNPMFILE_HOOKS,
        description: "A pnpmfile path (CLI arg, workspace setting, global) pointed at a missing file.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_PNPMFILE_STDERR_FORWARDER,
        category: category::PNPMFILE_HOOKS,
        description: "The background task forwarding pnpmfile stderr panicked.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HOOK_IMPORTER_MUTATED,
        category: category::PNPMFILE_HOOKS,
        description: "A pnpmfile `afterAllResolved` hook mutated `importers[...]`; aube ignored the edit.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HOOK_IMPORTER_ADDED,
        category: category::PNPMFILE_HOOKS,
        description: "A pnpmfile `afterAllResolved` hook added a new `importers[...]` entry; aube ignored it.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HOOK_IDENTITY_REWRITTEN,
        category: category::PNPMFILE_HOOKS,
        description: "A pnpmfile hook rewrote a package's `(name, version)` identity; aube reverted the edit.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HOOK_PACKAGE_ADDED,
        category: category::PNPMFILE_HOOKS,
        description: "A pnpmfile hook added a wholly-new package entry; aube ignored it.",
        exit_code: None,
    },
    // Install lifecycle
    CodeMeta {
        name: WARN_AUBE_IGNORED_BUILD_SCRIPTS,
        category: category::INSTALL_LIFECYCLE,
        description: "Dep had `preinstall`/`install`/`postinstall` scripts but isn't on the `allowBuilds` allowlist. Run `aube approve-builds`.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_MISSING_INTEGRITY,
        category: category::INSTALL_LIFECYCLE,
        description: "Lockfile entry / registry response had no `dist.integrity`; importing without verification. Set `strict-store-integrity=true` to refuse.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_CACHE_WRITE_FAILED,
        category: category::INSTALL_LIFECYCLE,
        description: "Couldn't write a package index to the on-disk cache. Non-fatal.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_CLONE_STRATEGY_FALLBACK,
        category: category::INSTALL_LIFECYCLE,
        description: "`package-import-method=clone` will silently fall back to copy if the filesystem doesn't support reflinks.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_LTHASH_MISMATCH,
        category: category::INSTALL_LIFECYCLE,
        description: "Incremental and full LtHash digests disagreed — homomorphic invariant broken. Real bug signal.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_DELTA_INVALIDATE_FAILED,
        category: category::INSTALL_LIFECYCLE,
        description: "Delta install couldn't invalidate a package directory during cleanup.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_GVS_INCOMPATIBLE,
        category: category::INSTALL_LIFECYCLE,
        description: "A package isn't compatible with aube's global virtual store; installed per-project instead.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_GVS_MODE_CHANGED,
        category: category::INSTALL_LIFECYCLE,
        description: "Switching between gvs-on and gvs-off; removing `node_modules` and reinstalling from scratch.",
        exit_code: None,
    },
    // Settings / config validation
    CodeMeta {
        name: WARN_AUBE_INVALID_CONCURRENCY,
        category: category::SETTINGS_CONFIG,
        description: "`network-concurrency` or `link-concurrency` was 0 (must be ≥ 1).",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_TRUST_POLICY,
        category: category::SETTINGS_CONFIG,
        description: "A `trustPolicyExclude` entry was malformed and skipped.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_OVERRIDE_MISSING_DEP,
        category: category::SETTINGS_CONFIG,
        description: "An `overrides` `$ref` pointed at a package not in any of the importer's dependency lists.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_PEER_PATTERN,
        category: category::SETTINGS_CONFIG,
        description: "A `peerDependencyRules` pattern was unparseable and skipped.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_SAVE_PREFIX,
        category: category::SETTINGS_CONFIG,
        description: "`save-prefix` was something other than `^`, `~`, or empty. Falling back to `^`.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_CONCURRENCY_ENV_INVALID,
        category: category::SETTINGS_CONFIG,
        description: "The `AUBE_CONCURRENCY` env var was outside the `[floor, ceiling]` range or non-numeric.",
        exit_code: None,
    },
    // Update / prerelease
    CodeMeta {
        name: WARN_AUBE_PRERELEASE_CHECK_SKIPPED,
        category: category::UPDATE_PRERELEASE,
        description: "`aube update` couldn't fetch the packument or got a non-semver `latest` tag; preserved-prerelease check skipped for that package.",
        exit_code: None,
    },
    // Audit / npmrc
    CodeMeta {
        name: WARN_AUBE_AUDIT_FETCH_FAILED,
        category: category::AUDIT_NPMRC,
        description: "`aube audit --ignore-unfixable` couldn't fetch a packument; advisories for that package are kept verbatim.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_TOKEN_CHMOD_FAILED,
        category: category::AUDIT_NPMRC,
        description: "`chmod 0600` on the auth token file failed; the file may be world-readable.",
        exit_code: None,
    },
    // Registry config (trust gates)
    CodeMeta {
        name: WARN_AUBE_UNTRUSTED_PROXY,
        category: category::REGISTRY_CONFIG,
        description: "A `*-proxy` setting came from a source aube doesn't trust (committed `.npmrc` can't set proxies).",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_UNTRUSTED_STRICT_SSL_DISABLE,
        category: category::REGISTRY_CONFIG,
        description: "`strict-ssl=false` came from a source aube doesn't trust. TLS validation stays on.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_LOCAL_ADDRESS,
        category: category::REGISTRY_CONFIG,
        description: "`local-address` setting wasn't a valid IP.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_MAXSOCKETS,
        category: category::REGISTRY_CONFIG,
        description: "`maxsockets` was 0 or non-numeric.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_UNTRUSTED_TOKEN_HELPER,
        category: category::REGISTRY_CONFIG,
        description: "`tokenHelper` came from an untrusted source (CVE-2025-69262 class).",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_TOKEN_HELPER,
        category: category::REGISTRY_CONFIG,
        description: "`tokenHelper` value wasn't a sanitized absolute path.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_TOKEN_HELPER_SPAWN_FAILED,
        category: category::REGISTRY_CONFIG,
        description: "`tokenHelper` couldn't be spawned.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_TOKEN_HELPER_NON_ZERO_EXIT,
        category: category::REGISTRY_CONFIG,
        description: "`tokenHelper` exited non-zero.",
        exit_code: None,
    },
    // Registry HTTP retries
    CodeMeta {
        name: WARN_AUBE_HTTP_RETRY_TRANSIENT,
        category: category::HTTP_RETRIES,
        description: "Retrying after a transient HTTP status (429, 5xx).",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HTTP_RETRY_TRANSPORT,
        category: category::HTTP_RETRIES,
        description: "Retrying after a transport / connection error.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HTTP_RETRY_BODY_READ,
        category: category::HTTP_RETRIES,
        description: "Retrying after a response-body read error (timeout, partial body).",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_HTTP_RETRY_BODY_DECODE,
        category: category::HTTP_RETRIES,
        description: "Retrying after a JSON decode error on the response body.",
        exit_code: None,
    },
    // Registry caching / perf
    CodeMeta {
        name: WARN_AUBE_PACKUMENT_CACHE_WRITE,
        category: category::REGISTRY_PERF,
        description: "Couldn't write a packument to the on-disk cache after a successful fetch. Non-fatal; next install will refetch.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_SLOW_METADATA,
        category: category::REGISTRY_PERF,
        description: "A packument fetch exceeded `fetchWarnTimeoutMs`.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_SLOW_TARBALL,
        category: category::REGISTRY_PERF,
        description: "A tarball download fell below `fetchMinSpeedKiBps`.",
        exit_code: None,
    },
    // Registry TLS / proxy
    CodeMeta {
        name: WARN_AUBE_INVALID_HTTPS_PROXY,
        category: category::REGISTRY_TLS,
        description: "`https-proxy` URL didn't parse.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_HTTP_PROXY,
        category: category::REGISTRY_TLS,
        description: "`http-proxy` URL didn't parse.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_CA,
        category: category::REGISTRY_TLS,
        description: "Per-registry CA PEM didn't parse.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_CAFILE,
        category: category::REGISTRY_TLS,
        description: "`cafile` couldn't be parsed as a PEM bundle.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_UNREADABLE_CAFILE,
        category: category::REGISTRY_TLS,
        description: "`cafile` couldn't be read from disk.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_INVALID_CLIENT_CERT,
        category: category::REGISTRY_TLS,
        description: "Per-registry client `cert`/`key` PEM pair didn't parse.",
        exit_code: None,
    },
    // Resolver
    CodeMeta {
        name: WARN_AUBE_UNSUPPORTED_PLATFORM_INSTALL,
        category: category::RESOLVER,
        description: "A required (non-optional) dep declared a platform aube doesn't satisfy; installing anyway.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_EXOTIC_SUBDEP_SKIPPED,
        category: category::RESOLVER,
        description: "An optional or peer dep used an exotic specifier and was skipped under `blockExoticSubdeps=true`.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_PEER_DEDUPE_COLLISION,
        category: category::RESOLVER,
        description: "`dedupe-peers=true` would have collapsed a distinct peer-variant; preserved the longer form to avoid dropping it.",
        exit_code: None,
    },
    // Lockfile
    CodeMeta {
        name: WARN_AUBE_LOCKFILE_MERGE_CONFLICT,
        category: category::LOCKFILE,
        description: "Branch-lockfile merge had conflicting entries for the same dep_path; one was chosen.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_LOCKFILE_MERGE_CLEANUP_FAILED,
        category: category::LOCKFILE,
        description: "After a successful merge, removing one of the merged branch lockfiles failed.",
        exit_code: None,
    },
    CodeMeta {
        name: WARN_AUBE_YARN_BERRY_UNSUPPORTED,
        category: category::LOCKFILE,
        description: "A Yarn Berry `patch:` / `portal:` / `exec:` protocol — or any unrecognized protocol — was found in `yarn.lock`. Entry was skipped.",
        exit_code: None,
    },
];
