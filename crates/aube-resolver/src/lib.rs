pub mod override_rule;
pub mod platform;

pub use platform::{SupportedArchitectures, is_supported};

use aube_lockfile::{DepType, DirectDep, LocalSource, LockedPackage, LockfileGraph};
use aube_manifest::PackageJson;
use aube_registry::Packument;
use aube_registry::client::RegistryClient;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Hook invoked once per resolved package, right after its version has
/// been picked from the packument and before its dependency set is
/// enqueued. Implementations may mutate `dependencies`,
/// `optionalDependencies`, `peerDependencies`, and
/// `peerDependenciesMeta`; every other field is ignored on the way
/// back, matching how pnpm's `readPackage` hook is used in the wild.
///
/// The trait is deliberately shaped to let a single long-lived node
/// subprocess implement it — `&mut self` so the impl can own stdin /
/// stdout halves of the child without interior mutability, and a boxed
/// future because `async fn` in dyn-compatible traits still requires
/// third-party crates we haven't pulled in.
pub trait ReadPackageHook: Send {
    fn read_package<'a>(
        &'a mut self,
        pkg: aube_registry::VersionMetadata,
    ) -> Pin<Box<dyn Future<Output = Result<aube_registry::VersionMetadata, String>> + Send + 'a>>;
}

/// Supply-chain mitigation: forbid versions younger than `min_age` for
/// every package whose name isn't in `exclude`. Mirrors pnpm's
/// `minimumReleaseAge` / `minimumReleaseAgeExclude` /
/// `minimumReleaseAgeStrict` triplet. Constructed by the install
/// command, threaded into [`Resolver::with_minimum_release_age`].
#[derive(Debug, Clone, Default)]
pub struct MinimumReleaseAge {
    /// Minutes a version must have aged in the registry. `0` disables.
    pub minutes: u64,
    /// Package names skipped by the cutoff filter entirely.
    pub exclude: HashSet<String>,
    /// When true, fail the install if no version satisfies the range
    /// without violating the cutoff. When false (the pnpm default), the
    /// resolver falls back to the lowest satisfying version, ignoring
    /// the cutoff for that pick only.
    pub strict: bool,
}

#[derive(Debug, Clone)]
pub struct DependencyPolicy {
    pub package_extensions: Vec<PackageExtension>,
    pub allowed_deprecated_versions: BTreeMap<String, String>,
    pub trust_policy: TrustPolicy,
    pub trust_policy_exclude: BTreeSet<String>,
    pub trust_policy_ignore_after: Option<u64>,
    pub block_exotic_subdeps: bool,
}

impl Default for DependencyPolicy {
    fn default() -> Self {
        Self {
            package_extensions: Vec::new(),
            allowed_deprecated_versions: BTreeMap::new(),
            trust_policy: TrustPolicy::default(),
            trust_policy_exclude: BTreeSet::new(),
            trust_policy_ignore_after: None,
            block_exotic_subdeps: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageExtension {
    pub selector: String,
    pub dependencies: BTreeMap<String, String>,
    pub optional_dependencies: BTreeMap<String, String>,
    pub peer_dependencies: BTreeMap<String, String>,
    pub peer_dependencies_meta: BTreeMap<String, aube_registry::PeerDepMeta>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrustPolicy {
    NoDowngrade,
    #[default]
    Off,
}

impl MinimumReleaseAge {
    /// Compute the absolute ISO-8601 UTC cutoff string. Returns `None`
    /// when the feature is disabled (`minutes == 0`). Format matches
    /// the npm registry's `time` map so a lexicographic compare on the
    /// raw strings doubles as an instant compare.
    pub fn cutoff(&self) -> Option<String> {
        if self.minutes == 0 {
            return None;
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        let cutoff_secs = now.saturating_sub(self.minutes * 60);
        Some(format_iso8601_utc(cutoff_secs))
    }
}

/// Format a Unix epoch second count as an ISO-8601 UTC `Z` string. The
/// resolver only ever compares these against npm registry timestamps,
/// which are emitted in this exact shape — so we can ship our own
/// formatter and skip pulling in `chrono`/`time`. Algorithm adapted
/// from the days-from-epoch trick used by `time` and `civil` crates.
///
/// `aube/src/commands/sbom.rs` carries a near-identical formatter
/// for the SPDX/CycloneDX writers; that one emits seconds-only
/// (`...:00Z`) since SBOM consumers don't expect millis. Don't merge
/// without checking which format each caller needs — the npm registry
/// `time` map always uses `.000Z`, lex compare relies on it.
fn format_iso8601_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.000Z")
}

/// Convert a day count from the Unix epoch (1970-01-01) to a
/// proleptic Gregorian (year, month, day). Lifted from Howard Hinnant's
/// `civil_from_days` paper, which the `time` crate uses.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// A resolved package emitted during resolution, allowing the caller
/// to start fetching tarballs before resolution is fully complete.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub dep_path: String,
    pub name: String,
    pub version: String,
    pub integrity: Option<String>,
    /// Exact tarball URL reported by the packument's `dist.tarball`
    /// field, or preserved from an existing lockfile. Most npm
    /// packages can re-derive this from name + version, but JSR's
    /// npm-compatible registry uses opaque tarball paths, so fetchers
    /// must prefer this when it is available.
    pub tarball_url: Option<String>,
    /// Real registry name when this package is an npm-alias
    /// (`"h3-v2": "npm:h3@..."`). `name` is the alias (`h3-v2` — the
    /// folder in `node_modules/`), `alias_of` is what the streaming
    /// fetch client uses to derive the tarball URL and store-index
    /// key. `None` for non-aliased packages, in which case `name`
    /// already matches the registry.
    pub alias_of: Option<String>,
    /// Set for non-registry packages (`file:` / `link:`). Downstream
    /// fetchers short-circuit the tarball path and materialize from
    /// disk instead.
    pub local_source: Option<LocalSource>,
}

impl ResolvedPackage {
    /// Registry lookup name — `alias_of` when set, otherwise `name`.
    /// Every tarball URL + store index site routes through this
    /// accessor so aliased packages resolve to the real registry
    /// entry without leaking the alias-qualified name into network
    /// requests (where it would 404).
    pub fn registry_name(&self) -> &str {
        self.alias_of.as_deref().unwrap_or(&self.name)
    }
}

/// Which version-picking strategy the resolver uses for a workspace.
/// Mirrors pnpm's `resolution-mode` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResolutionMode {
    /// Classic pnpm behavior: every dep resolves to the highest version
    /// satisfying its range.
    #[default]
    Highest,
    /// Pick the lowest version that satisfies each direct-dep range,
    /// then constrain transitive picks to versions published on or
    /// before a cutoff date derived from the max publish time of
    /// already-locked packages. Matches pnpm's `time-based` mode.
    TimeBased,
}

/// BFS dependency resolver.
pub struct Resolver {
    client: Arc<RegistryClient>,
    cache: HashMap<String, Packument>,
    /// Optional channel to stream resolved packages as they're discovered.
    resolved_tx: Option<mpsc::UnboundedSender<ResolvedPackage>>,
    /// Optional disk cache directory for packuments (with ETag revalidation).
    packument_cache_dir: Option<std::path::PathBuf>,
    /// Separate disk cache for full (non-corgi) packuments; only used
    /// when `resolution_mode` is `TimeBased` (which needs the `time:`
    /// map). Defaults to the sibling `packuments-full-v1/` directory
    /// next to `packument_cache_dir`.
    packument_full_cache_dir: Option<std::path::PathBuf>,
    /// When true (pnpm's default), a package's declared `peerDependencies`
    /// are enqueued like regular transitives and — if not already
    /// satisfied by the importer — hoisted to the importer's direct deps.
    /// When false, peers neither get auto-installed as transitives nor
    /// hoisted; unmet peers still surface as warnings via
    /// `detect_unmet_peers`, but the user is on the hook for adding them
    /// explicitly to `package.json`.
    auto_install_peers: bool,
    /// pnpm's `exclude-links-from-lockfile`. Round-tripped through the
    /// lockfile's `settings:` header; when true, the pnpm writer omits
    /// `link:` deps from the importer `dependencies:` maps so a
    /// sibling symlink change doesn't churn the lockfile. Defaults to
    /// false (pnpm's default). Does not affect resolution itself, only
    /// the `canonical.settings.exclude_links_from_lockfile` flag the
    /// writer reads.
    exclude_links_from_lockfile: bool,
    /// User-declared override for the host platform triple, used when
    /// deciding whether an optional dep's `os`/`cpu`/`libc` constraints
    /// are satisfied. Empty fields fall back to the host.
    supported_architectures: SupportedArchitectures,
    /// Raw dependency override map from the manifest (selector key →
    /// replacement spec). Round-tripped verbatim through the lockfile
    /// for drift detection; the compiled form in `override_rules` is
    /// what the resolver hot loop actually consults.
    overrides: BTreeMap<String, String>,
    /// Compiled view of `overrides`. Built by `with_overrides`.
    /// Unparseable selector keys are dropped at compile time so the
    /// matcher never has to think about them.
    override_rules: Vec<override_rule::OverrideRule>,
    /// Names listed in the root manifest's `pnpm.ignoredOptionalDependencies`.
    /// Any optional dep (root or transitive) whose name is in this set is
    /// dropped before enqueueing — the resolver never fetches or locks it.
    /// Mirrors pnpm's `createOptionalDependenciesRemover` read-package hook.
    ignored_optional_dependencies: BTreeSet<String>,
    /// pnpm's `resolution-mode` — `Highest` (default) or `TimeBased`.
    resolution_mode: ResolutionMode,
    /// Project root used to resolve `file:` / `link:` paths to the
    /// target directory. Defaults to the current working directory;
    /// callers set it via `with_project_root`.
    project_root: PathBuf,
    /// pnpm v11's `minimumReleaseAge` triplet. `None` disables the
    /// supply-chain age gate entirely (matching `minimumReleaseAge: 0`).
    minimum_release_age: Option<MinimumReleaseAge>,
    /// Workspace catalog ranges. Outer key is the catalog name
    /// (`default` for the unnamed `catalog:` field in
    /// `pnpm-workspace.yaml`); inner key is the package name; value is
    /// the version range. When the resolver encounters a `catalog:` or
    /// `catalog:<name>` task range, it rewrites the task in place to
    /// the matching range *before* the override / npm-alias passes,
    /// while preserving the original `catalog:...` text in
    /// `original_specifier` so the lockfile importer keeps the
    /// reference verbatim.
    catalogs: BTreeMap<String, BTreeMap<String, String>>,
    /// Optional `readPackage` hook, invoked once per resolved package
    /// before its transitive deps are enqueued. See [`ReadPackageHook`].
    /// Wired up by `aube` when a `.pnpmfile.cjs` is detected and
    /// `--ignore-pnpmfile` was not set.
    read_package_hook: Option<Box<dyn ReadPackageHook>>,
    dependency_policy: DependencyPolicy,
    /// Hosts for which aube performs shallow git clones, mirroring
    /// pnpm's `git-shallow-hosts`. When a git dep's URL host is in
    /// this list, the store attempts `git fetch --depth 1 origin
    /// <sha>` (falling back to a full fetch if the server refuses);
    /// otherwise it goes straight to a full fetch. Defaults to an
    /// empty list — `aube` populates it from the generated
    /// `aube_settings::resolved::git_shallow_hosts` accessor (which
    /// carries the pnpm-compat default list baked in from
    /// `settings.toml`) via [`Self::with_git_shallow_hosts`]. Library
    /// callers who construct a `Resolver` directly must set it
    /// explicitly if they want the pnpm list; keeping the list in
    /// one place (`settings.toml`) avoids drift.
    git_shallow_hosts: Vec<String>,
    /// pnpm's `peersSuffixMaxLength`. When the peer-ID suffix on a
    /// `dep_path` (the `(name@version)(…)` portion) would exceed this
    /// many bytes, the post-pass replaces the whole suffix with
    /// `_<hex>` where `<hex>` is the first 10 chars of SHA-256 of the
    /// full suffix. Matches pnpm's lockfile format. Default 1000.
    peers_suffix_max_length: usize,
    /// pnpm's `dedupe-peer-dependents`. When true (pnpm's default),
    /// the peer-context post-pass collapses multiple dep_path variants
    /// of the same canonical package into a single entry when their
    /// peer resolutions are pairwise-equivalent. When false, every
    /// distinct ancestor scope gets its own variant — useful for
    /// debugging peer-context divergence or mimicking pnpm v6/v7
    /// behavior.
    dedupe_peer_dependents: bool,
    /// pnpm's `dedupe-peers`. When true, peer suffixes in the lockfile
    /// emit just the resolved version — `(18.2.0)` — instead of the
    /// full `(react@18.2.0)` form. Shorter dep_paths at the cost of
    /// peer-name fidelity in the snapshot. Defaults to false.
    dedupe_peers: bool,
    /// pnpm's `resolve-peers-from-workspace-root`. When true (pnpm's
    /// default), an importer's unresolved peer can be satisfied by a
    /// dependency declared in the root importer's `package.json`, even
    /// when no ancestor scope carries that dep. Common monorepo knob:
    /// the workspace root pins shared peers like `react`, and every
    /// subpackage can peer on it without hoisting the version into
    /// every sibling.
    resolve_peers_from_workspace_root: bool,
    /// pnpm's `registry-supports-time-field`. When true, the resolver
    /// trusts the abbreviated (corgi) packument to carry the `time:`
    /// map and keeps using the cheap `fetch_packument_cached` path
    /// even under time-aware resolution (`TimeBased` or
    /// `minimumReleaseAge`). Defaults to false — the same assumption
    /// pnpm and npmjs.org ship with — so the resolver falls back to
    /// the full-packument fetch to get `time:` reliably. No effect
    /// when neither time-based resolution nor `minimumReleaseAge` is
    /// active, since the abbreviated path is already the only one
    /// running.
    registry_supports_time_field: bool,
}

struct ResolveTask {
    name: String,
    range: String,
    dep_type: DepType,
    is_root: bool,
    /// The parent dep_path, for wiring up transitive dep references
    parent: Option<String>,
    /// Which importer this task belongs to (e.g., "." or "packages/app")
    importer: String,
    /// The original specifier from package.json before any rewrites
    /// (e.g. `"npm:real-pkg@^2.0.0"` for an alias, or `"^4.17.0"` for a normal range).
    /// Only set for root deps; recorded into the lockfile for drift detection.
    original_specifier: Option<String>,
    /// Real registry package name for npm-alias tasks.
    ///
    /// When a task arrives with `range` like `"npm:h3@2.0.1-rc.20"`,
    /// the preprocessing loop strips the prefix and sets this field to
    /// the real package name (`"h3"`) while *keeping* `name` as the
    /// user-facing alias (`"h3-v2"`, the key the package.json used).
    /// Every identity-facing site — dep_path formation, direct-dep
    /// records, parent `dependencies` wiring, the resolved-versions
    /// dedupe map — uses `name`, so the alias survives all the way
    /// to the linker and ends up as `node_modules/<alias>/` with
    /// `LockedPackage.alias_of = Some(real_name)`. Only registry
    /// I/O (packument fetch, tarball URL derivation) consults this
    /// field.
    ///
    /// `None` for ordinary (non-aliased) tasks — `name` is already
    /// the registry name and nothing downstream needs to distinguish.
    real_name: Option<String>,
    /// Outermost-first chain of `(name, version)` ancestors above this
    /// task in the dependency graph, used by `parent>child` override
    /// selectors. Empty for root/importer deps. Each child-enqueue
    /// site is responsible for extending its parent's chain with the
    /// parent's own `(name, version)` frame.
    ancestors: Vec<(String, String)>,
}

impl ResolveTask {
    /// Name to use for registry operations (packument fetch, tarball
    /// URL). Returns `real_name` for aliased tasks and `name`
    /// otherwise. Every call site that talks to the registry goes
    /// through this accessor so alias handling stays localized.
    fn registry_name(&self) -> &str {
        self.real_name.as_deref().unwrap_or(&self.name)
    }
}

impl Resolver {
    pub fn new(client: Arc<RegistryClient>) -> Self {
        Self {
            client,
            cache: HashMap::new(),
            resolved_tx: None,
            packument_cache_dir: None,
            packument_full_cache_dir: None,
            auto_install_peers: true,
            exclude_links_from_lockfile: false,
            supported_architectures: SupportedArchitectures::default(),
            overrides: BTreeMap::new(),
            override_rules: Vec::new(),
            ignored_optional_dependencies: BTreeSet::new(),
            resolution_mode: ResolutionMode::Highest,
            project_root: PathBuf::from("."),
            minimum_release_age: None,
            catalogs: BTreeMap::new(),
            read_package_hook: None,
            dependency_policy: DependencyPolicy::default(),
            git_shallow_hosts: Vec::new(),
            peers_suffix_max_length: 1000,
            dedupe_peer_dependents: true,
            dedupe_peers: false,
            resolve_peers_from_workspace_root: true,
            registry_supports_time_field: false,
        }
    }

    /// Create a resolver that streams resolved packages through a channel.
    /// Returns `(resolver, receiver)`. The receiver yields packages as they're
    /// discovered, allowing tarball fetches to start during resolution.
    pub fn with_stream(
        client: Arc<RegistryClient>,
    ) -> (Self, mpsc::UnboundedReceiver<ResolvedPackage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                client,
                cache: HashMap::new(),
                resolved_tx: Some(tx),
                packument_cache_dir: None,
                packument_full_cache_dir: None,
                auto_install_peers: true,
                exclude_links_from_lockfile: false,
                supported_architectures: SupportedArchitectures::default(),
                overrides: BTreeMap::new(),
                override_rules: Vec::new(),
                ignored_optional_dependencies: BTreeSet::new(),
                resolution_mode: ResolutionMode::Highest,
                project_root: PathBuf::from("."),
                minimum_release_age: None,
                catalogs: BTreeMap::new(),
                read_package_hook: None,
                dependency_policy: DependencyPolicy::default(),
                git_shallow_hosts: Vec::new(),
                peers_suffix_max_length: 1000,
                dedupe_peer_dependents: true,
                dedupe_peers: false,
                resolve_peers_from_workspace_root: true,
                registry_supports_time_field: false,
            },
            rx,
        )
    }

    /// Enable disk-backed packument caching with ETag/Last-Modified revalidation.
    pub fn with_packument_cache(mut self, cache_dir: std::path::PathBuf) -> Self {
        self.packument_cache_dir = Some(cache_dir);
        self
    }

    /// Disk cache for full (non-corgi) packuments, used in
    /// `ResolutionMode::TimeBased` so we can read the `time:` map.
    pub fn with_packument_full_cache(mut self, cache_dir: std::path::PathBuf) -> Self {
        self.packument_full_cache_dir = Some(cache_dir);
        self
    }

    /// Set the resolution mode. Defaults to `Highest` (pnpm's classic
    /// behavior). `TimeBased` switches direct deps to lowest-satisfying
    /// and constrains transitives by a publish-date cutoff.
    pub fn with_resolution_mode(mut self, mode: ResolutionMode) -> Self {
        self.resolution_mode = mode;
        self
    }

    /// Configure pnpm v11's `minimumReleaseAge` family of settings.
    /// Pass `None` (or a config with `minutes == 0`) to disable.
    pub fn with_minimum_release_age(mut self, mra: Option<MinimumReleaseAge>) -> Self {
        self.minimum_release_age = mra.filter(|m| m.minutes > 0);
        self
    }

    /// Override the default `auto-install-peers=true` behavior. pnpm reads
    /// this from `.npmrc` or `pnpm-workspace.yaml`; aube's install command
    /// plumbs the resolved value through here before running resolution.
    pub fn with_auto_install_peers(mut self, auto_install_peers: bool) -> Self {
        self.auto_install_peers = auto_install_peers;
        self
    }

    /// Configure pnpm's `peersSuffixMaxLength`. When the peer suffix on a
    /// `dep_path` would exceed this many bytes, the post-pass replaces it
    /// with `_<10-char-sha256-hex>`. Default 1000 (pnpm's default).
    pub fn with_peers_suffix_max_length(mut self, max_length: usize) -> Self {
        self.peers_suffix_max_length = max_length;
        self
    }

    /// Override the default `dedupe-peer-dependents=true` behavior. When
    /// false, the peer-context pass keeps every distinct ancestor-scope
    /// variant of a package instead of collapsing peer-equivalent ones
    /// into a single dep_path. Plumbed from `.npmrc` /
    /// `pnpm-workspace.yaml` via the install command.
    pub fn with_dedupe_peer_dependents(mut self, value: bool) -> Self {
        self.dedupe_peer_dependents = value;
        self
    }

    /// Override the default `dedupe-peers=false` behavior. When true,
    /// peer suffixes in the lockfile drop the peer name and emit only
    /// the resolved version — `(18.2.0)` instead of `(react@18.2.0)`.
    /// Plumbed from `.npmrc` / `pnpm-workspace.yaml` via the install
    /// command.
    pub fn with_dedupe_peers(mut self, value: bool) -> Self {
        self.dedupe_peers = value;
        self
    }

    /// Override the default `resolve-peers-from-workspace-root=true`
    /// behavior. When false, peer resolution stops at the importer's
    /// own scope + BFS-auto-installed transitives instead of consulting
    /// the workspace root's direct deps as a fallback tier. Plumbed
    /// from `.npmrc` / `pnpm-workspace.yaml` via the install command.
    pub fn with_resolve_peers_from_workspace_root(mut self, value: bool) -> Self {
        self.resolve_peers_from_workspace_root = value;
        self
    }

    /// Configure pnpm's `registry-supports-time-field`. When true,
    /// the resolver keeps using the abbreviated (corgi) packument
    /// path even when `time:` is needed, saving one full-packument
    /// fetch per distinct package. Safe for registries that embed
    /// `time` in their abbreviated responses (Verdaccio 5.15.1+, JSR,
    /// most in-house mirrors); leave at the default `false` for
    /// npmjs.org.
    pub fn with_registry_supports_time_field(mut self, value: bool) -> Self {
        self.registry_supports_time_field = value;
        self
    }

    /// Configure pnpm's `exclude-links-from-lockfile` setting. Only
    /// affects lockfile serialization — the resolver still builds the
    /// same graph either way, but the value is stamped into
    /// `LockfileGraph::settings` so the pnpm writer can filter `link:`
    /// importer entries on write.
    pub fn with_exclude_links_from_lockfile(mut self, value: bool) -> Self {
        self.exclude_links_from_lockfile = value;
        self
    }

    /// Override the host platform triple used when filtering optional
    /// dependencies. See [`platform::SupportedArchitectures`].
    pub fn with_supported_architectures(mut self, value: SupportedArchitectures) -> Self {
        self.supported_architectures = value;
        self
    }

    /// Provide dependency overrides. The map's keys are selector
    /// strings — bare name, `parent>child`, `foo@<2`, `**/foo`, or any
    /// combination thereof — and values are version specifiers (or
    /// `npm:` aliases). Keys are compiled into `override_rule`
    /// structures; unparseable keys are dropped. Whenever the resolver
    /// encounters a task matching a rule (by name + ancestor chain +
    /// optional version constraints), the requested range is replaced
    /// with the rule's replacement before any packument fetch or
    /// version pick. Workspace + manifest sources are merged by the
    /// caller.
    pub fn with_overrides(mut self, overrides: BTreeMap<String, String>) -> Self {
        self.override_rules = override_rule::compile(&overrides);
        self.overrides = overrides;
        self
    }

    /// Provide workspace catalog ranges. Outer key is the catalog name
    /// (`default` for the unnamed `catalog:` field in
    /// `pnpm-workspace.yaml`); inner key is the package name. The
    /// resolver rewrites `catalog:` and `catalog:<name>` task ranges
    /// against this map before the override / npm-alias passes, and
    /// records the picks in the output graph's `catalogs` field.
    pub fn with_catalogs(mut self, catalogs: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        self.catalogs = catalogs;
        self
    }

    /// Set the project root used to resolve `file:` / `link:` paths.
    /// `file:./vendor/foo` resolves against this directory, and a
    /// matching directory / tarball is read to drive resolution of the
    /// local package's transitive deps.
    pub fn with_project_root(mut self, project_root: PathBuf) -> Self {
        self.project_root = project_root;
        self
    }

    /// Names to strip from every `optionalDependencies` map before
    /// enqueueing (pnpm's `pnpm.ignoredOptionalDependencies`). Applied
    /// to both root and transitive optional deps. Empty by default.
    pub fn with_ignored_optional_dependencies(mut self, ignored: BTreeSet<String>) -> Self {
        self.ignored_optional_dependencies = ignored;
        self
    }

    /// Install a `readPackage` hook. The resolver calls it once per
    /// version-picked packument before enqueueing transitives; see
    /// [`ReadPackageHook`] for what mutations are honored.
    pub fn with_read_package_hook(mut self, hook: Box<dyn ReadPackageHook>) -> Self {
        self.read_package_hook = Some(hook);
        self
    }

    /// Configure dependency resolution policy settings such as
    /// `packageExtensions`, `allowedDeprecatedVersions`, `trustPolicy*`,
    /// and `blockExoticSubdeps`.
    pub fn with_dependency_policy(mut self, policy: DependencyPolicy) -> Self {
        self.dependency_policy = policy;
        self
    }

    /// Set the `git-shallow-hosts` list used when cloning git deps.
    /// When a git URL's host matches an entry here (exact match,
    /// same as pnpm), aube attempts a shallow fetch by SHA; other
    /// hosts get a plain `git fetch origin`. An empty list forces
    /// every git dep through the full-fetch path.
    pub fn with_git_shallow_hosts(mut self, hosts: Vec<String>) -> Self {
        self.git_shallow_hosts = hosts;
        self
    }

    /// Resolve all dependencies from a package.json.
    ///
    /// Uses batch-parallel BFS: each "wave" drains the queue, identifies
    /// uncached package names, fetches their packuments concurrently, then
    /// processes the entire batch before starting the next wave.
    pub async fn resolve(
        &mut self,
        manifest: &PackageJson,
        existing: Option<&LockfileGraph>,
    ) -> Result<LockfileGraph, Error> {
        self.resolve_workspace(
            &[(".".to_string(), manifest.clone())],
            existing,
            &HashMap::new(),
        )
        .await
    }

    /// Resolve all dependencies for a workspace (multiple importers).
    ///
    /// `manifests` is a list of (importer_path, PackageJson) — e.g. (".", root), ("packages/app", app).
    /// `workspace_packages` maps package name → version for workspace: protocol resolution.
    pub async fn resolve_workspace(
        &mut self,
        manifests: &[(String, PackageJson)],
        existing: Option<&LockfileGraph>,
        workspace_packages: &HashMap<String, String>,
    ) -> Result<LockfileGraph, Error> {
        let resolve_start = std::time::Instant::now();
        let mut packument_fetch_count = 0u32;
        let mut packument_fetch_time = std::time::Duration::ZERO;
        let mut lockfile_reuse_count = 0u32;
        let mut resolved: BTreeMap<String, LockedPackage> = BTreeMap::new();
        let mut resolved_versions: HashMap<String, Vec<String>> = HashMap::new();
        let mut importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
        let mut queue: VecDeque<ResolveTask> = VecDeque::new();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Round-tripped to the lockfile's top-level `time:` block so
        // subsequent installs can reuse them for the cutoff computation.
        // Populated opportunistically from whatever packuments we fetch:
        // empty when the metadata omits `time` (corgi from npmjs.org in
        // default mode), filled when it doesn't (Verdaccio, or the
        // full-packument path taken for time-based resolution and
        // `minimumReleaseAge`). This matches pnpm's `publishedAt` wiring.
        let mut resolved_times: BTreeMap<String, String> = BTreeMap::new();
        // Per-importer record of optionals the resolver intentionally
        // dropped on this run — either filtered by os/cpu/libc or
        // named in `pnpm.ignoredOptionalDependencies`. Round-tripped
        // through the lockfile so drift detection on subsequent
        // installs can distinguish "previously skipped" from "newly
        // added by the user".
        let mut skipped_optional_dependencies: BTreeMap<String, BTreeMap<String, String>> =
            BTreeMap::new();
        // Catalog picks gathered as the BFS rewrites `catalog:` task
        // ranges. Outer key: catalog name. Inner: package name → spec.
        // Resolved versions are filled in post-resolution by walking
        // `resolved_versions` for the spec, since the picked version is
        // an output the BFS doesn't know until version_satisfies fires.
        let mut catalog_picks: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        let importer_declared_dep_names: BTreeMap<String, BTreeSet<String>> = manifests
            .iter()
            .map(|(importer_path, manifest)| {
                let names = manifest
                    .dependencies
                    .keys()
                    .chain(manifest.dev_dependencies.keys())
                    .chain(manifest.optional_dependencies.keys())
                    .cloned()
                    .collect();
                (importer_path.clone(), names)
            })
            .collect();
        // ISO-8601 UTC cutoff string. npm's registry `time` map uses
        // `Z`-suffixed UTC timestamps throughout, which sort
        // lexicographically — so a raw `String` doubles as a
        // comparable instant without pulling in a date library.
        //
        // Two independent features feed this cutoff:
        //   - `minimum_release_age` (pnpm v11 default, supply-chain
        //     mitigation): seeded *before* wave 0 so even direct deps
        //     are filtered. The exclude list and strict-mode behavior
        //     are scoped per-package by `pick_version` below.
        //   - `resolution-mode=time-based`: derived from the max
        //     publish time across direct deps once wave 0 finishes,
        //     then constrains transitives only.
        // When both are configured, the resolver carries both cutoffs
        // and the picker takes the more restrictive (earlier) one.
        let mut published_by: Option<String> =
            self.minimum_release_age.as_ref().and_then(|m| m.cutoff());
        if let Some(c) = published_by.as_deref() {
            tracing::debug!("minimumReleaseAge cutoff: {}", c);
        }

        // Seed queue with direct deps from all importers
        for (importer_path, manifest) in manifests {
            importers.insert(importer_path.clone(), Vec::new());

            for (name, range) in &manifest.dependencies {
                queue.push_back(ResolveTask {
                    name: name.clone(),
                    range: range.clone(),
                    dep_type: DepType::Production,
                    is_root: true,
                    parent: None,
                    importer: importer_path.clone(),
                    original_specifier: Some(range.clone()),
                    real_name: None,
                    ancestors: Vec::new(),
                });
            }
            for (name, range) in &manifest.dev_dependencies {
                queue.push_back(ResolveTask {
                    name: name.clone(),
                    range: range.clone(),
                    dep_type: DepType::Dev,
                    is_root: true,
                    parent: None,
                    importer: importer_path.clone(),
                    original_specifier: Some(range.clone()),
                    real_name: None,
                    ancestors: Vec::new(),
                });
            }
            for (name, range) in &manifest.optional_dependencies {
                if self.ignored_optional_dependencies.contains(name) {
                    tracing::debug!(
                        "ignoring optional dependency {name} (pnpm.ignoredOptionalDependencies)"
                    );
                    continue;
                }
                queue.push_back(ResolveTask {
                    name: name.clone(),
                    range: range.clone(),
                    dep_type: DepType::Optional,
                    is_root: true,
                    parent: None,
                    importer: importer_path.clone(),
                    original_specifier: Some(range.clone()),
                    real_name: None,
                    ancestors: Vec::new(),
                });
            }
        }

        // Pipelined resolver state. The resolver is strictly serial in
        // its *processing* order (tasks are popped and version-picked
        // in seed/BFS order, which is what keeps the output lockfile
        // byte-deterministic across runs) but fetches run freely in
        // the background via `in_flight`. When a popped task's
        // packument isn't in the cache, the main loop waits inline on
        // `in_flight.join_next()` — harvesting whatever other fetches
        // happen to land in the meantime — until this task's
        // packument is available. Because `ensure_fetch!` is called
        // speculatively at every enqueue site, by the time a task is
        // popped its packument is usually already cached, so the
        // wait is short.
        let shared_semaphore = Arc::new(tokio::sync::Semaphore::new(64));
        // Time-based mode and `minimumReleaseAge` both need the
        // packument's `time:` map. The abbreviated (corgi) response
        // omits `time` by default, so we normally fall back to the
        // full packument. `registry-supports-time-field=true` flips
        // that: the user is asserting the configured registry ships
        // `time` in corgi too (Verdaccio 5.15.1+, JSR, etc.), so the
        // cheaper abbreviated path stays on the hot path and we save
        // one full-packument fetch per distinct package.
        let needs_time = (self.resolution_mode == ResolutionMode::TimeBased
            || self.minimum_release_age.is_some())
            && !self.registry_supports_time_field;
        // In-flight packument fetches. The spawned task returns the
        // `(name, packument)` tuple so `join_next` gives us back the
        // identity of whichever fetch landed next without a side
        // table lookup.
        #[allow(clippy::type_complexity)]
        let mut in_flight: tokio::task::JoinSet<Result<(String, Packument), Error>> =
            tokio::task::JoinSet::new();
        // Names whose fetch has been spawned but not yet harvested.
        // Dedupes spawn calls when multiple tasks discover the same
        // transitive before any of them has been processed.
        let mut in_flight_names: HashSet<String> = HashSet::new();
        // TimeBased wave-0 gate: the publish-time cutoff is derived
        // from the direct deps' resolved versions, so transitives
        // that reach the version-pick step before all directs have
        // completed must wait. Populated only when
        // `cutoff_pending == true` (TimeBased mode); `Highest` mode
        // leaves these at their defaults and the gate is a no-op.
        let mut direct_deps_pending: usize = queue.len();
        let mut cutoff_pending = self.resolution_mode == ResolutionMode::TimeBased;
        let mut deferred_transitives: Vec<ResolveTask> = Vec::new();

        // Set of names present in the existing lockfile. Used as a
        // prefetch gate: names the lockfile already covers will hit
        // the lockfile-reuse path and don't need their packuments
        // fetched, so prefetching them is wasted tokio-spawn
        // overhead. Load-bearing for `aube add` and
        // frozen-lockfile-install scenarios where most tasks go
        // through lockfile-reuse.
        //
        // This is strictly a *prefetch* gate, not a correctness
        // gate: a task that fails sibling dedupe AND lockfile reuse
        // (because its range doesn't match any of the lockfile's
        // versions for that name) still needs a fresh fetch, and
        // the wait-for-fetch loop below calls `ensure_fetch!`
        // without consulting `existing_names`.
        let existing_names: std::collections::HashSet<String> = existing
            .map(|g| g.packages.values().map(|p| p.name.clone()).collect())
            .unwrap_or_default();

        // Spawn a packument fetch into `in_flight` if one isn't
        // already running for `name` and the packument isn't
        // already cached. Gated *only* on in-flight + cache —
        // callers that want to skip prefetching names already
        // covered by the lockfile check `existing_names` explicitly
        // before invoking the macro.
        macro_rules! ensure_fetch {
            ($name:expr) => {{
                let name: &str = $name;
                if !in_flight_names.contains(name) && !self.cache.contains_key(name) {
                    in_flight_names.insert(name.to_string());
                    let name_owned = name.to_string();
                    let client = self.client.clone();
                    let cache_dir = self.packument_cache_dir.clone();
                    let full_cache_dir = self.packument_full_cache_dir.clone();
                    let sem = shared_semaphore.clone();
                    in_flight.spawn(async move {
                        let _permit = sem
                            .acquire_owned()
                            .await
                            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
                        let packument = if needs_time {
                            match full_cache_dir.as_ref() {
                                Some(dir) => {
                                    client
                                        .fetch_packument_with_time_cached(&name_owned, dir)
                                        .await
                                }
                                None => client.fetch_packument(&name_owned).await,
                            }
                        } else if let Some(ref dir) = cache_dir {
                            client.fetch_packument_cached(&name_owned, dir).await
                        } else {
                            client.fetch_packument(&name_owned).await
                        }
                        .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
                        Ok::<_, Error>((name_owned, packument))
                    });
                }
            }};
        }

        // Decrement the pending-directs counter when a root task
        // reaches a terminal state. Used by the TimeBased cutoff
        // trigger at the top of the outer loop.
        macro_rules! note_root_done {
            () => {
                if direct_deps_pending > 0 {
                    direct_deps_pending -= 1;
                }
            };
        }

        // `(name, range)` is safe to speculatively prefetch against
        // the registry when:
        //
        //   - The range isn't a protocol we rewrite in preprocessing
        //     (`workspace:` / `catalog:` / `npm:` alias) — for those
        //     we don't know the real package name yet, so fetching
        //     the raw task name is either useless (preprocessing
        //     won't go through the registry at all) or wrong (we'd
        //     fetch the alias key instead of the real package).
        //   - The range isn't a `file:` / `link:` / `git:` /
        //     remote-tarball spec (covered by
        //     `is_non_registry_specifier`).
        //   - The name isn't in the overrides map — an override can
        //     rewrite the range into any of the above, and we can't
        //     cheaply tell whether it will, so be conservative.
        //
        // Called both from the upfront prefetch loop over seeded
        // root deps *and* from the three transitive-enqueue sites
        // inside the version-pick body, where the same class of
        // unsafe specs can arrive via a published package's
        // `dependencies` / `optionalDependencies` / `peerDependencies`
        // maps (real-world case: a package whose dependency entry
        // is an npm alias).
        macro_rules! prefetchable {
            ($name:expr, $range:expr) => {{
                let r: &str = $range;
                let n: &str = $name;
                !r.starts_with("workspace:")
                    && !r.starts_with("catalog:")
                    && !r.starts_with("npm:")
                    && !r.starts_with("jsr:")
                    && !is_non_registry_specifier(r)
                    && !self.overrides.contains_key(n)
            }};
        }

        // Fire prefetches for every seeded root dep up front, so
        // their packuments are already in flight by the time the
        // first task is popped. Skip lockfile-covered names —
        // they'll hit the lockfile-reuse path and never need their
        // packuments — and anything `prefetchable!` rejects.
        for task in queue.iter() {
            if !prefetchable!(task.name.as_str(), task.range.as_str()) {
                continue;
            }
            if existing_names.contains(task.name.as_str()) {
                continue;
            }
            ensure_fetch!(&task.name);
        }

        'outer: loop {
            // TimeBased cutoff trigger. Fires the first time
            // `direct_deps_pending` hits zero with the cutoff still
            // pending — at which point every direct dep has been
            // version-picked (or terminated in preprocessing),
            // `resolved_times` holds their publish times, and we can
            // derive the max to seed `published_by` for the
            // transitives we deferred.
            if cutoff_pending && direct_deps_pending == 0 {
                let direct_dep_paths: std::collections::HashSet<&String> = importers
                    .values()
                    .flat_map(|deps| deps.iter().map(|d| &d.dep_path))
                    .collect();
                let mut max_time: Option<&String> = None;
                for (dep_path, t) in resolved_times.iter() {
                    if !direct_dep_paths.contains(dep_path) {
                        continue;
                    }
                    if max_time.map(|m| t > m).unwrap_or(true) {
                        max_time = Some(t);
                    }
                }
                if let Some(existing_graph) = existing {
                    for (dep_path, t) in &existing_graph.times {
                        if !direct_dep_paths.contains(dep_path) {
                            continue;
                        }
                        if max_time.map(|m| t > m).unwrap_or(true) {
                            max_time = Some(t);
                        }
                    }
                }
                if let Some(m) = max_time {
                    tracing::debug!("time-based resolution cutoff: {}", m);
                    published_by = Some(match published_by.take() {
                        Some(existing) if existing.as_str() < m.as_str() => existing,
                        _ => m.clone(),
                    });
                }
                cutoff_pending = false;
                queue.extend(deferred_transitives.drain(..));
            }

            let Some(mut task) = queue.pop_front() else {
                if !deferred_transitives.is_empty() {
                    return Err(Error::Registry(
                        "(resolver)".to_string(),
                        format!(
                            "{} transitives still deferred when resolve completed",
                            deferred_transitives.len()
                        ),
                    ));
                }
                break 'outer;
            };

            // Body of the former per-task preprocessing loop.
            // The old wave-based code split this into a
            // preprocessing pass and a post-fetch version-pick
            // pass with a fetch barrier between them. Here both
            // passes run inline for a single task: preprocess →
            // sibling dedupe → lockfile reuse → wait on this
            // task's packument → version-pick → enqueue
            // transitives. The bare block keeps the original
            // indentation so the diff stays readable against the
            // prior shape; `continue` inside it still continues
            // the 'outer loop because a bare block is not itself
            // a loop.
            {
                // Apply bare-name overrides + npm-alias rewrites in a
                // small fixed-point loop. Two interleavings need to
                // work simultaneously:
                //   1. The override *value* is itself a `npm:` alias
                //      (e.g. `"foo": "npm:bar@^2"`). The first override
                //      pass rewrites `task.range`; the alias pass then
                //      rewrites `task.name` to `bar`.
                //   2. The user's *declared dep* is an `npm:` alias
                //      (e.g. `"foo": "npm:bar@^1"`) and the override
                //      targets the real package (`"overrides":
                //      {"bar": "2.0.0"}`). The first override pass
                //      misses (`task.name` is still `foo`), the alias
                //      pass rewrites `task.name = "bar"`, and the
                //      second override pass catches it.
                // A two-iteration cap is enough — after one alias
                // rewrite the name is canonical, and an override that
                // points at a third package is itself constrained by
                // the same rule, so there's no infinite chain.
                //
                // We deliberately don't touch `original_specifier`,
                // since the lockfile/importer record should still
                // reflect what the user wrote in package.json —
                // overrides are a graph-shaping rule, not a rewrite of
                // the user's declared deps.
                // Catalog protocol: rewrite `catalog:` and
                // `catalog:<name>` to the workspace catalog's actual
                // range *before* the override loop, so overrides can
                // still target a catalog dep by bare name. The original
                // `catalog:...` text stays in `original_specifier` so
                // the lockfile importer keeps the catalog reference and
                // drift detection works.
                if let Some(catalog_name) = task.range.strip_prefix("catalog:").map(|n| {
                    if n.is_empty() {
                        "default".to_string()
                    } else {
                        n.to_string()
                    }
                }) {
                    match self.catalogs.get(&catalog_name) {
                        Some(catalog) => match catalog.get(&task.name) {
                            Some(real_range) => {
                                tracing::trace!(
                                    "catalog: {} {} -> {}",
                                    task.name,
                                    task.range,
                                    real_range
                                );
                                catalog_picks
                                    .entry(catalog_name.clone())
                                    .or_default()
                                    .insert(task.name.clone(), real_range.clone());
                                task.range = real_range.clone();
                            }
                            None => {
                                return Err(Error::UnknownCatalogEntry {
                                    name: task.name.clone(),
                                    spec: task.range.clone(),
                                    catalog: catalog_name,
                                });
                            }
                        },
                        None => {
                            return Err(Error::UnknownCatalog {
                                name: task.name.clone(),
                                spec: task.range.clone(),
                                catalog: catalog_name,
                            });
                        }
                    }
                }

                for _ in 0..2 {
                    let mut changed = false;
                    if let Some(override_spec) = pick_override_spec(
                        &self.override_rules,
                        &task.name,
                        &task.range,
                        &task.ancestors,
                    ) {
                        // An override may itself point at a catalog
                        // entry (e.g. `"overrides": {"foo": "catalog:"}`).
                        // The catalog pre-pass above already ran against
                        // the original range, so resolve the indirection
                        // here before assigning — otherwise `catalog:`
                        // leaks through to the registry resolver.
                        // Compute the resolved spec first; stash the
                        // catalog pick in a local so we only record it
                        // if the override actually moves `task.range`.
                        let (effective_spec, pending_pick) = if let Some(catalog_name) =
                            override_spec.strip_prefix("catalog:").map(|n| {
                                if n.is_empty() {
                                    "default".to_string()
                                } else {
                                    n.to_string()
                                }
                            }) {
                            match self.catalogs.get(&catalog_name) {
                                Some(catalog) => match catalog.get(&task.name) {
                                    Some(real_range) => (
                                        real_range.clone(),
                                        Some((catalog_name, real_range.clone())),
                                    ),
                                    None => {
                                        return Err(Error::UnknownCatalogEntry {
                                            name: task.name.clone(),
                                            spec: override_spec.clone(),
                                            catalog: catalog_name,
                                        });
                                    }
                                },
                                None => {
                                    return Err(Error::UnknownCatalog {
                                        name: task.name.clone(),
                                        spec: override_spec.clone(),
                                        catalog: catalog_name,
                                    });
                                }
                            }
                        } else {
                            (override_spec, None)
                        };
                        if task.range != effective_spec {
                            if let Some((catalog_name, real_range)) = pending_pick {
                                catalog_picks
                                    .entry(catalog_name)
                                    .or_default()
                                    .insert(task.name.clone(), real_range);
                            }
                            tracing::trace!(
                                "override: {}@{} -> {}",
                                task.name,
                                task.range,
                                effective_spec
                            );
                            task.range = effective_spec;
                            changed = true;
                        }
                    }
                    if let Some(rest) = task.range.strip_prefix("npm:")
                        && let Some(at_idx) = rest.rfind('@')
                    {
                        let real_name = rest[..at_idx].to_string();
                        let real_range = rest[at_idx + 1..].to_string();
                        // Keep `task.name` as the user-facing alias
                        // (the key the package.json used) and stash
                        // the registry name on `real_name` so every
                        // identity-facing site — dep_path formation,
                        // direct-dep records, parent wiring — sees
                        // the alias, while only packument/tarball
                        // fetch sites (via `task.registry_name()`)
                        // hit the real package. Overwriting
                        // `task.name` here would collapse
                        // `node_modules/h3-v2/` to `node_modules/h3/`
                        // and any `require("h3-v2")` would break.
                        if task.real_name.as_deref() != Some(real_name.as_str())
                            || real_range != task.range
                        {
                            tracing::trace!(
                                "npm alias: {} -> {}@{}",
                                task.name,
                                real_name,
                                real_range
                            );
                            task.real_name = Some(real_name);
                            task.range = real_range;
                            changed = true;
                        }
                    }
                    // `jsr:<range>` and `jsr:<@scope/name>[@<range>]` both
                    // land here. JSR's npm-compat endpoint serves every
                    // package under `@jsr/<scope>__<name>`, but the
                    // user-facing dependency name stays the JSR name (or
                    // explicit alias) from package.json. Keep `task.name`
                    // unchanged for dep_path/importer/link identity and
                    // stash the npm-compat name in `real_name`, matching
                    // the npm-alias path above. Only registry IO should
                    // see `@jsr/...`.
                    if let Some(rest) = task.range.strip_prefix("jsr:") {
                        let (jsr_name_raw, jsr_range) = if let Some(body) = rest.strip_prefix('@') {
                            match body.rfind('@') {
                                Some(rel_at) => {
                                    // Indices are relative to `body`; add 1 for
                                    // the `@` we just stripped so we can slice
                                    // against the original `rest`.
                                    let at_idx = rel_at + 1;
                                    (rest[..at_idx].to_string(), rest[at_idx + 1..].to_string())
                                }
                                None => (rest.to_string(), "latest".to_string()),
                            }
                        } else {
                            // Bare range form — the manifest key carries the
                            // JSR name (e.g. `"@std/collections": "jsr:^1"`).
                            (task.name.clone(), rest.to_string())
                        };
                        match aube_registry::jsr::jsr_to_npm_name(&jsr_name_raw) {
                            Some(npm_name) => {
                                if task.real_name.as_deref() != Some(npm_name.as_str())
                                    || jsr_range != task.range
                                {
                                    tracing::trace!(
                                        "jsr: {} -> {}@{}",
                                        task.name,
                                        npm_name,
                                        jsr_range,
                                    );
                                    task.real_name = Some(npm_name);
                                    task.range = jsr_range;
                                    changed = true;
                                }
                            }
                            None => {
                                return Err(Error::Registry(
                                    task.name.clone(),
                                    format!(
                                        "invalid jsr: spec `{}` — expected `jsr:@scope/name[@range]`",
                                        task.range,
                                    ),
                                ));
                            }
                        }
                    }
                    if !changed {
                        break;
                    }
                }

                // Handle file: / link: / git: protocols — the dep points
                // at a path on disk or a remote git repo rather than a
                // registry package. Only valid on root deps; a nested
                // package.json that declares its own `file:` dep silently
                // falls through to the normal resolver path and fails
                // loudly there.
                if is_non_registry_specifier(&task.range) {
                    if !task.is_root && self.dependency_policy.block_exotic_subdeps {
                        return Err(Error::BlockedExoticSubdep {
                            name: task.name.clone(),
                            spec: task.range.clone(),
                            parent: task
                                .parent
                                .clone()
                                .unwrap_or_else(|| "<unknown>".to_string()),
                        });
                    }
                    let importer_root = if task.importer == "." {
                        self.project_root.clone()
                    } else {
                        self.project_root.join(&task.importer)
                    };
                    let Some(raw_local) = LocalSource::parse(&task.range, &importer_root) else {
                        return Err(Error::Registry(
                            task.name.clone(),
                            format!("unparseable local specifier: {}", task.range),
                        ));
                    };
                    // For git sources we have to talk to the remote
                    // right now so the resolver can (a) pin the
                    // committish to a full SHA for the lockfile and
                    // (b) read the cloned repo's `package.json` for
                    // transitive deps. `resolve_git_source` does the
                    // `ls-remote` + shallow clone dance and returns a
                    // `LocalSource::Git` with `resolved` populated,
                    // plus the manifest tuple the rest of the branch
                    // already expects.
                    if !task.is_root
                        && !matches!(
                            raw_local,
                            LocalSource::Git(_) | LocalSource::RemoteTarball(_)
                        )
                    {
                        return Err(Error::Registry(
                            task.name.clone(),
                            format!(
                                "transitive local specifier {} cannot be resolved without the parent package source root",
                                task.range
                            ),
                        ));
                    }
                    let (local, real_version, target_deps) = if let LocalSource::Git(ref g) =
                        raw_local
                    {
                        let shallow = aube_store::git_host_in_list(&g.url, &self.git_shallow_hosts);
                        let (resolved_local, version, deps) =
                            resolve_git_source(&task.name, g, shallow)
                                .await
                                .map_err(|e| {
                                    Error::Registry(
                                        task.name.clone(),
                                        format!("git resolve {}: {e}", task.range),
                                    )
                                })?;
                        (resolved_local, version, deps)
                    } else if let LocalSource::RemoteTarball(ref t) = raw_local {
                        let (resolved_local, version, deps) =
                            resolve_remote_tarball(&task.name, t, self.client.as_ref())
                                .await
                                .map_err(|e| {
                                    Error::Registry(
                                        task.name.clone(),
                                        format!("remote tarball {}: {e}", task.range),
                                    )
                                })?;
                        (resolved_local, version, deps)
                    } else {
                        // Rewrite the path to be relative to the
                        // project root so every downstream consumer
                        // can resolve it with a single
                        // `project_root.join(rel)`.
                        let local = rebase_local(&raw_local, &importer_root, &self.project_root);
                        let (_target_name, version, deps) =
                            read_local_manifest(&raw_local, &importer_root).unwrap_or_else(|_| {
                                (task.name.clone(), "0.0.0".to_string(), BTreeMap::new())
                            });
                        (local, version, deps)
                    };
                    let dep_path = local.dep_path(&task.name);
                    let linked_name = task.name.clone();

                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }

                    if !visited.contains(&dep_path) {
                        visited.insert(dep_path.clone());
                        resolved.insert(
                            dep_path.clone(),
                            LockedPackage {
                                name: linked_name.clone(),
                                version: real_version.clone(),
                                dep_path: dep_path.clone(),
                                local_source: Some(local.clone()),
                                ..Default::default()
                            },
                        );
                        if let Some(ref tx) = self.resolved_tx {
                            let _ = tx.send(ResolvedPackage {
                                dep_path: dep_path.clone(),
                                name: linked_name.clone(),
                                version: real_version.clone(),
                                integrity: None,
                                tarball_url: None,
                                // local_source deps aren't aliased —
                                // `file:`/`link:` specifiers go
                                // through the local-source branch,
                                // not the `npm:` rewrite.
                                alias_of: None,
                                local_source: Some(local.clone()),
                            });
                        }
                        // Enqueue transitive deps of the local package
                        // (directories + tarballs only — `link:` deps
                        // are fully the target's responsibility).
                        if !matches!(local, LocalSource::Link(_)) {
                            let mut child_ancestors = task.ancestors.clone();
                            child_ancestors.push((linked_name.clone(), real_version.clone()));
                            for (child_name, child_range) in target_deps {
                                queue.push_back(ResolveTask {
                                    name: child_name,
                                    range: child_range,
                                    dep_type: DepType::Production,
                                    is_root: false,
                                    parent: Some(dep_path.clone()),
                                    importer: task.importer.clone(),
                                    original_specifier: None,
                                    real_name: None,
                                    ancestors: child_ancestors.clone(),
                                });
                            }
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Handle workspace: protocol — resolve to workspace package version
                if task.range.starts_with("workspace:")
                    && let Some(ws_version) = workspace_packages.get(&task.name)
                {
                    let dep_path = dep_path_for(&task.name, ws_version);
                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }
                    if let Some(ref parent_dp) = task.parent
                        && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                    {
                        parent_pkg
                            .dependencies
                            .insert(task.name.clone(), ws_version.clone());
                        if task.dep_type == DepType::Optional {
                            parent_pkg
                                .optional_dependencies
                                .insert(task.name.clone(), ws_version.clone());
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Sibling dedupe. If another task for this same name
                // has already settled on a version that satisfies
                // this task's range, wire up to that resolution and
                // short-circuit. In the old wave code this check
                // lived in the post-fetch loop as `existing_match`;
                // in the pipelined loop we run it up front so
                // dedupable tasks never block on a fetch or a
                // lockfile scan.
                if let Some(matched_ver) = resolved_versions.get(&task.name).and_then(|versions| {
                    versions
                        .iter()
                        .find(|v| version_satisfies(v, &task.range))
                        .cloned()
                }) {
                    let dep_path = dep_path_for(&task.name, &matched_ver);
                    if task.is_root
                        && let Some(deps) = importers.get_mut(&task.importer)
                    {
                        deps.push(DirectDep {
                            name: task.name.clone(),
                            dep_path: dep_path.clone(),
                            dep_type: task.dep_type,
                            specifier: task.original_specifier.clone(),
                        });
                    }
                    if let Some(ref parent_dp) = task.parent
                        && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                    {
                        parent_pkg
                            .dependencies
                            .insert(task.name.clone(), matched_ver.clone());
                        if task.dep_type == DepType::Optional {
                            parent_pkg
                                .optional_dependencies
                                .insert(task.name.clone(), matched_ver);
                        }
                    }
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }

                // Lockfile reuse. Runs unconditionally after sibling
                // dedupe fails — the old code gated this behind a
                // `cache.contains_key` check, but in the pipelined
                // loop the cache is populated incrementally and the
                // gate was a false optimization.
                {
                    if let Some(locked_pkg) = existing.and_then(|g| {
                        g.packages.values().find(|p| {
                            p.name == task.name && version_satisfies(&p.version, &task.range)
                        })
                    }) {
                        // Drop optional deps whose platform constraints
                        // don't match the active host / supported set.
                        // This is the path that handles frozen/lockfile
                        // installs on a different machine than the one
                        // that wrote the lockfile.
                        if task.dep_type == DepType::Optional
                            && !is_supported(
                                &locked_pkg.os,
                                &locked_pkg.cpu,
                                &locked_pkg.libc,
                                &self.supported_architectures,
                            )
                        {
                            tracing::debug!(
                                "skipping optional dep {}@{}: platform mismatch",
                                task.name,
                                locked_pkg.version
                            );
                            if task.is_root
                                && let Some(spec) = task.original_specifier.as_ref()
                            {
                                skipped_optional_dependencies
                                    .entry(task.importer.clone())
                                    .or_default()
                                    .insert(task.name.clone(), spec.clone());
                            }
                            if task.is_root {
                                note_root_done!();
                            }
                            continue;
                        }
                        let version = locked_pkg.version.clone();
                        let dep_path = dep_path_for(&task.name, &version);

                        if task.is_root
                            && let Some(deps) = importers.get_mut(&task.importer)
                        {
                            deps.push(DirectDep {
                                name: task.name.clone(),
                                dep_path: dep_path.clone(),
                                dep_type: task.dep_type,
                                specifier: task.original_specifier.clone(),
                            });
                        }
                        if let Some(ref parent_dp) = task.parent
                            && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                        {
                            parent_pkg
                                .dependencies
                                .insert(task.name.clone(), version.clone());
                            if task.dep_type == DepType::Optional {
                                parent_pkg
                                    .optional_dependencies
                                    .insert(task.name.clone(), version.clone());
                            }
                        }
                        if !visited.contains(&dep_path) {
                            visited.insert(dep_path.clone());
                            resolved_versions
                                .entry(task.name.clone())
                                .or_default()
                                .push(version.clone());

                            // Carry any round-tripped publish time
                            // forward so (a) the cutoff computation at
                            // the end of wave 0 can see reused directs
                            // alongside freshly-resolved ones and
                            // (b) the next lockfile write preserves the
                            // existing `time:` entry even when this
                            // install reuses the locked version without
                            // re-fetching a packument.
                            if let Some(g) = existing
                                && let Some(t) = g.times.get(&dep_path)
                            {
                                resolved_times.insert(dep_path.clone(), t.clone());
                            }

                            if let Some(ref tx) = self.resolved_tx {
                                let _ = tx.send(ResolvedPackage {
                                    dep_path: dep_path.clone(),
                                    name: task.name.clone(),
                                    version: version.clone(),
                                    integrity: locked_pkg.integrity.clone(),
                                    tarball_url: locked_pkg.tarball_url.clone(),
                                    // Carry the alias identity
                                    // through the reuse path — the
                                    // existing `locked_pkg` already
                                    // records it if the lockfile held
                                    // an aliased entry, so the
                                    // streaming fetch still hits the
                                    // real registry name.
                                    alias_of: locked_pkg.alias_of.clone(),
                                    local_source: locked_pkg.local_source.clone(),
                                });
                            }

                            // Carry declared peer deps forward from the
                            // existing lockfile so subsequent peer-context
                            // computation sees them without a re-fetch.
                            resolved.insert(
                                dep_path.clone(),
                                LockedPackage {
                                    name: task.name.clone(),
                                    version: version.clone(),
                                    integrity: locked_pkg.integrity.clone(),
                                    dependencies: BTreeMap::new(),
                                    optional_dependencies: BTreeMap::new(),
                                    peer_dependencies: locked_pkg.peer_dependencies.clone(),
                                    peer_dependencies_meta: locked_pkg
                                        .peer_dependencies_meta
                                        .clone(),
                                    dep_path: dep_path.clone(),
                                    local_source: locked_pkg.local_source.clone(),
                                    os: locked_pkg.os.clone(),
                                    cpu: locked_pkg.cpu.clone(),
                                    libc: locked_pkg.libc.clone(),
                                    bundled_dependencies: locked_pkg.bundled_dependencies.clone(),
                                    tarball_url: locked_pkg.tarball_url.clone(),
                                    alias_of: locked_pkg.alias_of.clone(),
                                    yarn_checksum: locked_pkg.yarn_checksum.clone(),
                                },
                            );

                            // Enqueue transitive deps from the locked package.
                            // Strip any peer-context suffix off the version
                            // before treating it as a semver range — a
                            // locked `"18.2.0(react@18.2.0)"` tail should
                            // match against packuments as just `18.2.0`.
                            // The lockfile already omitted bundled dep
                            // edges on write, so iterating
                            // `locked_pkg.dependencies` naturally skips them.
                            let mut child_ancestors = task.ancestors.clone();
                            child_ancestors.push((task.name.clone(), version.clone()));
                            for (dep_name, dep_version) in &locked_pkg.dependencies {
                                let canonical_version = dep_version
                                    .split('(')
                                    .next()
                                    .unwrap_or(dep_version)
                                    .to_string();
                                let dep_type =
                                    if locked_pkg.optional_dependencies.contains_key(dep_name) {
                                        DepType::Optional
                                    } else {
                                        DepType::Production
                                    };
                                queue.push_back(ResolveTask {
                                    name: dep_name.clone(),
                                    range: canonical_version,
                                    dep_type,
                                    is_root: false,
                                    parent: Some(dep_path.clone()),
                                    importer: task.importer.clone(),
                                    original_specifier: None,
                                    real_name: None,
                                    ancestors: child_ancestors.clone(),
                                });
                            }
                        }
                        lockfile_reuse_count += 1;
                        if task.is_root {
                            note_root_done!();
                        }
                        continue;
                    }
                }

                // Packument not in cache. Spawn its fetch if one
                // isn't already running, then wait for packument
                // fetches to land until this task's packument is
                // available. Other fetches that happen to complete
                // while we're waiting get cached opportunistically,
                // which is exactly what lets the pipeline overlap
                // network and CPU: by the time a later task is
                // popped its packument is usually already sitting
                // in the cache because it landed while an earlier
                // task was being waited on.
                let wait_start = std::time::Instant::now();
                // Cache is keyed by the *registry* name — for aliased
                // tasks `task.name` is the user-facing alias (e.g.
                // `h3-v2`), which would never hit. `registry_name()`
                // returns the alias-resolved target (`h3`) on
                // aliased tasks and `task.name` otherwise.
                let fetch_name = task.registry_name().to_string();
                while !self.cache.contains_key(&fetch_name) {
                    ensure_fetch!(&fetch_name);
                    match in_flight.join_next().await {
                        Some(Ok(Ok((name, packument)))) => {
                            in_flight_names.remove(&name);
                            self.cache.insert(name, packument);
                            packument_fetch_count += 1;
                        }
                        Some(Ok(Err(e))) => return Err(e),
                        Some(Err(join_err)) => {
                            return Err(Error::Registry(
                                "(join)".to_string(),
                                join_err.to_string(),
                            ));
                        }
                        None => {
                            // ensure_fetch! guarantees something is
                            // in flight if the cache still doesn't
                            // hold this name, so a None here means
                            // the spawn failed silently. Surface it.
                            return Err(Error::Registry(
                                fetch_name.clone(),
                                "packument fetch disappeared before completing".to_string(),
                            ));
                        }
                    }
                }
                packument_fetch_time += wait_start.elapsed();

                // TimeBased wave-0 gate. Transitives that reach
                // the version-pick step while the cutoff is still
                // unknown must wait until the direct deps have
                // been picked and the cutoff has been derived;
                // otherwise they'd pick against a `None` cutoff
                // and miss the filter. In `Highest` mode (the
                // default), `cutoff_pending` starts false and this
                // is a no-op.
                if cutoff_pending && !task.is_root {
                    deferred_transitives.push(task);
                    continue;
                }

                // Version-pick + transitive enqueue. Was a separate
                // sub-loop over `processed_batch` in the old wave
                // code; here it's inline as the tail of the per-task
                // pipeline now that we know the packument is in
                // cache. `registry_name()` is the cache key for
                // aliased tasks (cache is populated under the real
                // registry name), so use the same accessor here.
                let packument = self.cache.get(task.registry_name()).ok_or_else(|| {
                    Error::Registry(
                        task.registry_name().to_string(),
                        "packument not in cache".to_string(),
                    )
                })?;

                // Find locked version
                let locked_version = existing.and_then(|g| {
                    g.packages
                        .values()
                        .find(|p| p.name == task.name && version_satisfies(&p.version, &task.range))
                        .map(|p| p.version.as_str())
                });

                // Direct deps in time-based mode pick the lowest
                // satisfying version; everything else (transitives,
                // and all picks in Highest mode) picks highest.
                let pick_lowest = self.resolution_mode == ResolutionMode::TimeBased && task.is_root;
                // Apply the cutoff unless this package is on the
                // minimumReleaseAge exclude list. The exclude list only
                // suppresses the *minimumReleaseAge* leg, not the
                // time-based-mode leg — but since we collapse both
                // into the same `published_by` string at this point,
                // we have to skip the cutoff entirely for excluded
                // names. Acceptable: time-based mode and exclude
                // lists aren't expected to coexist in the wild.
                let cutoff_for_pkg = match self.minimum_release_age.as_ref() {
                    Some(mra) if mra.exclude.contains(&task.name) => None,
                    _ => published_by.as_deref(),
                };
                // Strict semantics in two cases:
                //   - `minimumReleaseAgeStrict=true` (the user opted in
                //     to hard failures), or
                //   - the cutoff comes from `--resolution-mode=time-based`
                //     alone, with no `minimumReleaseAge` configured. The
                //     time-based cutoff is intended as a hard wall — if
                //     no version fits, the *correct* fix is for the user
                //     to update the lockfile, not for the resolver to
                //     silently pick a different version.
                let strict = match self.minimum_release_age.as_ref() {
                    Some(m) => m.strict,
                    None => true,
                };
                let pick = pick_version(
                    packument,
                    &task.range,
                    locked_version,
                    pick_lowest,
                    cutoff_for_pkg,
                    strict,
                );
                let picked_ref = match pick {
                    PickResult::Found(meta) => meta,
                    // Only surface `AgeGate` when the cutoff actually
                    // came from `minimumReleaseAge`. When it came from
                    // `--resolution-mode=time-based` alone, the user
                    // never opted into the supply-chain age gate, so
                    // the failure should report as a plain no-match
                    // instead of a misleading "older than 0 minutes".
                    PickResult::AgeGated => match self.minimum_release_age.as_ref() {
                        Some(mra) => {
                            return Err(Error::AgeGate {
                                name: task.name.clone(),
                                range: task.range.clone(),
                                minutes: mra.minutes,
                            });
                        }
                        None => {
                            return Err(Error::NoMatch(task.name.clone(), task.range.clone()));
                        }
                    },
                    PickResult::NoMatch => {
                        return Err(Error::NoMatch(task.name.clone(), task.range.clone()));
                    }
                };
                // Clone the picked metadata into an owned value so we can
                // both run the `readPackage` hook (which needs a
                // disjoint `&mut self` borrow) and, later, mutate the
                // resolver's own caches without holding a borrow into
                // `self.cache`. Also grab the publish-time entry now,
                // for the same reason.
                let mut picked_owned = picked_ref.clone();
                let picked_publish_time = packument.time.get(&picked_ref.version).cloned();
                // Skip the readPackage hook entirely for a `(name, version)`
                // pair we've already fully processed via a prior task. The
                // mutated dep maps only drive the transitive enqueue below,
                // and that block is short-circuited by the `visited` guard
                // later in this iteration — so running the hook here would
                // just burn an IPC round-trip whose result is discarded.
                let prehook_dep_path = dep_path_for(&task.name, &picked_ref.version);
                let already_visited = visited.contains(&prehook_dep_path);

                if !already_visited {
                    apply_package_extensions(
                        &mut picked_owned,
                        &self.dependency_policy.package_extensions,
                    );
                }

                // readPackage hook. Runs at most once per version-picked
                // package, before transitive enqueue. We honor edits to
                // the four dep maps and warn on (then discard) edits to
                // name/version/dist/platform/`hasInstallScript` — pnpm
                // tolerates readPackage returning a hollowed-out
                // object, so we restore those fields from the original
                // packument entry after the call.
                if !already_visited && let Some(hook) = self.read_package_hook.as_mut() {
                    let before_name = picked_owned.name.clone();
                    let before_version = picked_owned.version.clone();
                    let before_dist = picked_owned.dist.clone();
                    let before_os = picked_owned.os.clone();
                    let before_cpu = picked_owned.cpu.clone();
                    let before_libc = picked_owned.libc.clone();
                    let before_bundled = picked_owned.bundled_dependencies.clone();
                    let before_has_install_script = picked_owned.has_install_script;
                    let before_deprecated = picked_owned.deprecated.clone();
                    let input = picked_owned.clone();
                    let mut after = hook.read_package(input).await.map_err(|e| {
                        Error::Registry(before_name.clone(), format!("readPackage hook: {e}"))
                    })?;
                    if after.name != before_name || after.version != before_version {
                        tracing::warn!(
                            "[pnpmfile] readPackage rewrote {}@{} identity to {}@{}; \
                             aube ignores identity edits",
                            before_name,
                            before_version,
                            after.name,
                            after.version,
                        );
                    }
                    after.name = before_name;
                    after.version = before_version;
                    after.dist = before_dist;
                    after.os = before_os;
                    after.cpu = before_cpu;
                    after.libc = before_libc;
                    after.bundled_dependencies = before_bundled;
                    after.has_install_script = before_has_install_script;
                    after.deprecated = before_deprecated;
                    picked_owned = after;
                }
                let version_meta = &picked_owned;

                // Optional deps that don't match the host platform get
                // silently dropped — pnpm parity. Required deps with a
                // bad platform still get installed; the warning matches
                // pnpm's `packageIsInstallable` behavior.
                let platform_ok = is_supported(
                    &version_meta.os,
                    &version_meta.cpu,
                    &version_meta.libc,
                    &self.supported_architectures,
                );
                if !platform_ok {
                    if task.dep_type == DepType::Optional {
                        tracing::debug!(
                            "skipping optional dep {}@{}: unsupported platform (os={:?} cpu={:?} libc={:?})",
                            task.name,
                            version_meta.version,
                            version_meta.os,
                            version_meta.cpu,
                            version_meta.libc
                        );
                        if task.is_root
                            && let Some(spec) = task.original_specifier.as_ref()
                        {
                            skipped_optional_dependencies
                                .entry(task.importer.clone())
                                .or_default()
                                .insert(task.name.clone(), spec.clone());
                        }
                        if task.is_root {
                            note_root_done!();
                        }
                        continue;
                    }
                    tracing::warn!(
                        "required dep {}@{} declares unsupported platform (os={:?} cpu={:?} libc={:?}); installing anyway",
                        task.name,
                        version_meta.version,
                        version_meta.os,
                        version_meta.cpu,
                        version_meta.libc
                    );
                }

                let version = version_meta.version.clone();
                let dep_path = dep_path_for(&task.name, &version);

                // Record publish time for the cutoff / `time:` block
                // whenever the packument carries one — matches pnpm,
                // which populates `publishedAt` opportunistically via
                // `meta.time?.[version]` regardless of resolution mode.
                // Corgi packuments from npmjs.org omit `time`, so in
                // Highest mode this is usually a no-op; Verdaccio
                // (v5.15.1+) and full-packument fetches do include it,
                // and then we round-trip it into the lockfile just like
                // pnpm does.
                if let Some(t) = picked_publish_time.as_ref() {
                    resolved_times.insert(dep_path.clone(), t.clone());
                }

                // Record root dep
                if task.is_root
                    && let Some(deps) = importers.get_mut(&task.importer)
                {
                    deps.push(DirectDep {
                        name: task.name.clone(),
                        dep_path: dep_path.clone(),
                        dep_type: task.dep_type,
                        specifier: task.original_specifier.clone(),
                    });
                }

                // Wire parent
                if let Some(ref parent_dp) = task.parent
                    && let Some(parent_pkg) = resolved.get_mut(parent_dp)
                {
                    parent_pkg
                        .dependencies
                        .insert(task.name.clone(), version.clone());
                    if task.dep_type == DepType::Optional {
                        parent_pkg
                            .optional_dependencies
                            .insert(task.name.clone(), version.clone());
                    }
                }

                // Skip if already fully processed this exact version
                if visited.contains(&dep_path) {
                    if task.is_root {
                        note_root_done!();
                    }
                    continue;
                }
                visited.insert(dep_path.clone());

                tracing::trace!("resolved {}@{}", task.name, version);

                // Warn about deprecated versions unless suppressed
                if let Some(ref msg) = version_meta.deprecated {
                    let suppressed = self
                        .dependency_policy
                        .allowed_deprecated_versions
                        .get(&task.name)
                        .is_some_and(|range| {
                            node_semver::Range::parse(range).ok().is_some_and(|r| {
                                node_semver::Version::parse(&version)
                                    .ok()
                                    .is_some_and(|v| r.satisfies(&v))
                            })
                        });
                    if !suppressed {
                        tracing::warn!("{}@{} is deprecated: {}", task.name, version, msg);
                    }
                }

                // Track this version
                resolved_versions
                    .entry(task.name.clone())
                    .or_default()
                    .push(version.clone());

                let registry_name = task.registry_name();
                let integrity = version_meta.dist.as_ref().and_then(|d| d.integrity.clone());
                let tarball_url = version_meta.dist.as_ref().and_then(|d| {
                    registry_name
                        .starts_with("@jsr/")
                        .then(|| d.tarball.clone())
                });

                // Stream this resolved package for early tarball fetching.
                // `alias_of` mirrors what the LockedPackage below
                // will carry — the streaming fetch consumer in
                // install.rs uses it to derive the real tarball URL
                // for aliased packages where `name` alone (`h3-v2`)
                // would 404.
                if let Some(ref tx) = self.resolved_tx {
                    let _ = tx.send(ResolvedPackage {
                        dep_path: dep_path.clone(),
                        name: task.name.clone(),
                        version: version.clone(),
                        integrity: integrity.clone(),
                        tarball_url: tarball_url.clone(),
                        alias_of: task.real_name.clone(),
                        local_source: None,
                    });
                }

                // Capture the declared peer deps now so the post-pass can
                // compute each consumer's peer context without re-reading
                // the packument.
                let peer_deps = version_meta.peer_dependencies.clone();
                let peer_meta: BTreeMap<String, aube_lockfile::PeerDepMeta> = version_meta
                    .peer_dependencies_meta
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            aube_lockfile::PeerDepMeta {
                                optional: v.optional,
                            },
                        )
                    })
                    .collect();
                // `bundledDependencies` names are shipped inside the
                // tarball itself and must not be resolved from the
                // registry. If we did enqueue them, we'd fetch a
                // (possibly different) version and plant a sibling
                // symlink inside `.aube/<parent>@ver/node_modules/`
                // that would shadow the bundled copy during Node's
                // directory walk. Compute the skip set once here and
                // store the names on the LockedPackage so restore
                // (from lockfile, skipping this code path) also
                // knows to avoid the sibling symlinks — see the
                // `.dependencies` write-through downstream.
                let bundled_names: std::collections::HashSet<String> = version_meta
                    .bundled_dependencies
                    .as_ref()
                    .map(|b| {
                        b.names(&version_meta.dependencies)
                            .into_iter()
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();

                resolved.insert(
                    dep_path.clone(),
                    LockedPackage {
                        name: task.name.clone(),
                        version: version.clone(),
                        integrity,
                        dependencies: BTreeMap::new(),
                        optional_dependencies: BTreeMap::new(),
                        peer_dependencies: peer_deps,
                        peer_dependencies_meta: peer_meta,
                        dep_path: dep_path.clone(),
                        local_source: None,
                        os: version_meta.os.clone(),
                        cpu: version_meta.cpu.clone(),
                        libc: version_meta.libc.clone(),
                        bundled_dependencies: {
                            let mut v: Vec<String> = bundled_names.iter().cloned().collect();
                            v.sort();
                            v
                        },
                        tarball_url,
                        // `name` is the alias for npm-aliased tasks
                        // (`"h3-v2": "npm:h3@..."` → name = "h3-v2"),
                        // so stash the real registry name here. The
                        // lockfile writer + installer consult
                        // `alias_of` whenever they need to hit the
                        // registry, matching how the npm-lockfile
                        // reader populates this field.
                        alias_of: task.real_name.clone(),
                        yarn_checksum: None,
                    },
                );

                // Enqueue transitive deps. Kick off a background
                // packument fetch the instant we discover the dep
                // name — so by the time the task is popped off the
                // queue below, its packument is usually already in
                // flight (and often already in cache). This is where
                // the pipeline overlaps fetches with CPU work without
                // any explicit wave barrier.
                //
                // Compute the child ancestor chain once — the same
                // frame (this package's name + resolved version)
                // applies to every dep / optionalDep / peer we enqueue
                // below.
                let mut child_ancestors = task.ancestors.clone();
                child_ancestors.push((task.name.clone(), version.clone()));

                for (dep_name, dep_range) in &version_meta.dependencies {
                    if bundled_names.contains(dep_name) {
                        continue;
                    }
                    if self.dependency_policy.block_exotic_subdeps
                        && is_non_registry_specifier(dep_range)
                    {
                        return Err(Error::Registry(
                            dep_name.clone(),
                            format!(
                                "uses exotic specifier \"{dep_range}\" which is blocked \
                                 by blockExoticSubdeps (declared by {})",
                                task.name
                            ),
                        ));
                    }
                    if !existing_names.contains(dep_name.as_str())
                        && prefetchable!(dep_name.as_str(), dep_range.as_str())
                    {
                        ensure_fetch!(dep_name);
                    }
                    queue.push_back(ResolveTask {
                        name: dep_name.clone(),
                        range: dep_range.clone(),
                        dep_type: DepType::Production,
                        is_root: false,
                        parent: Some(dep_path.clone()),
                        importer: task.importer.clone(),
                        original_specifier: None,
                        real_name: None,
                        ancestors: child_ancestors.clone(),
                    });
                }

                for (dep_name, dep_range) in &version_meta.optional_dependencies {
                    if bundled_names.contains(dep_name) {
                        continue;
                    }
                    if self.ignored_optional_dependencies.contains(dep_name) {
                        continue;
                    }
                    if self.dependency_policy.block_exotic_subdeps
                        && is_non_registry_specifier(dep_range)
                    {
                        tracing::warn!(
                            "skipping optional dependency {dep_name} of {} — \
                             exotic specifier \"{dep_range}\" blocked by blockExoticSubdeps",
                            task.name
                        );
                        continue;
                    }
                    if !existing_names.contains(dep_name.as_str())
                        && prefetchable!(dep_name.as_str(), dep_range.as_str())
                    {
                        ensure_fetch!(dep_name);
                    }
                    queue.push_back(ResolveTask {
                        name: dep_name.clone(),
                        range: dep_range.clone(),
                        dep_type: DepType::Optional,
                        is_root: false,
                        parent: Some(dep_path.clone()),
                        importer: task.importer.clone(),
                        original_specifier: None,
                        real_name: None,
                        ancestors: child_ancestors.clone(),
                    });
                }

                // Peer dependencies: enqueue only required peers that
                // are truly missing from the importer/root scope. The
                // post-pass below (`apply_peer_contexts`) computes
                // which version each consumer sees, via ancestor
                // scope, and assigns peer-suffixed dep_paths.
                //
                // pnpm's `auto-install-peers=true` fills in missing
                // required peers, but it does not install optional peer
                // alternatives that the user did not ask for, and it
                // does not install a second compatible peer when the
                // importer already declares that peer name at an
                // incompatible version. In the latter case pnpm keeps
                // the user's direct dependency and reports an unmet
                // peer warning.
                //
                // When `auto-install-peers=false`, we skip enqueueing
                // peers entirely. Users are on the hook for adding
                // them to `package.json` themselves. Unmet peers still
                // surface as warnings via `detect_unmet_peers` after
                // resolve — in fact more so, since nothing gets
                // auto-installed.
                //
                // Skip peers that are already declared as regular or
                // optional deps of the same package — those already have a
                // task queued via the loops above, and duplicating would
                // just burn a queue slot.
                if self.auto_install_peers {
                    for (dep_name, dep_range) in &version_meta.peer_dependencies {
                        let peer_optional = version_meta
                            .peer_dependencies_meta
                            .get(dep_name)
                            .map(|m| m.optional)
                            .unwrap_or(false);
                        // Optional peers are opt-in integrations, not
                        // auto-install candidates. Users who need one must
                        // declare it in their own manifest so the normal dep
                        // loops above resolve it explicitly.
                        if peer_optional {
                            continue;
                        }
                        let importer_declares_peer = importer_declared_dep_names
                            .get(&task.importer)
                            .is_some_and(|names| names.contains(dep_name));
                        let root_declares_peer = self.resolve_peers_from_workspace_root
                            && task.importer != "."
                            && importer_declared_dep_names
                                .get(".")
                                .is_some_and(|names| names.contains(dep_name));
                        let peer_dep_is_ancestor =
                            task.ancestors.iter().any(|(name, _)| name == dep_name);
                        if importer_declares_peer || root_declares_peer || peer_dep_is_ancestor {
                            continue;
                        }
                        if version_meta.dependencies.contains_key(dep_name)
                            || version_meta.optional_dependencies.contains_key(dep_name)
                            || bundled_names.contains(dep_name)
                        {
                            continue;
                        }
                        if self.dependency_policy.block_exotic_subdeps
                            && is_non_registry_specifier(dep_range)
                        {
                            tracing::warn!(
                                "skipping peer dependency {dep_name} of {} — \
                                 exotic specifier \"{dep_range}\" blocked \
                                 by blockExoticSubdeps",
                                task.name
                            );
                            continue;
                        }
                        if !existing_names.contains(dep_name.as_str())
                            && prefetchable!(dep_name.as_str(), dep_range.as_str())
                        {
                            ensure_fetch!(dep_name);
                        }
                        queue.push_back(ResolveTask {
                            name: dep_name.clone(),
                            range: dep_range.clone(),
                            dep_type: DepType::Production,
                            is_root: false,
                            parent: Some(dep_path.clone()),
                            importer: task.importer.clone(),
                            original_specifier: None,
                            real_name: None,
                            ancestors: child_ancestors.clone(),
                        });
                    }
                }

                // Root task just completed its full version-pick
                // path. Decrement the pending-directs counter so
                // the TimeBased cutoff trigger at the top of the
                // outer loop can fire once wave 0 is resolved.
                if task.is_root {
                    note_root_done!();
                }
            }
        }

        // Drain any remaining in-flight fetches so their tasks get
        // cleanly joined. Normally the main loop has harvested every
        // spawned fetch by the time the queue drains, but a few may
        // still be pending if the resolver short-circuited via
        // sibling dedupe or lockfile reuse after ensure_fetch! had
        // already spawned them.
        while in_flight.join_next().await.is_some() {}

        let resolve_elapsed = resolve_start.elapsed();
        tracing::debug!(
            "resolver: {:.1?} total, {} packuments fetched ({:.1?} wall), {} reused from lockfile, {} packages resolved",
            resolve_elapsed,
            packument_fetch_count,
            packument_fetch_time,
            lockfile_reuse_count,
            resolved.len()
        );

        // Materialize catalog picks into the output graph. The version
        // for each pick comes from `resolved_versions` — normally the
        // satisfying pick, but when an override redirects a catalog
        // dep the only resolved version may not satisfy the original
        // catalog range. In that case fall back to whatever version
        // the BFS *did* lock (the override target), since that's the
        // version actually installed for this importer. The pure
        // string fallback (`spec.clone()`) is reserved for the
        // unreachable case where the BFS didn't lock the package at
        // all — keeping it avoids a panic on a malformed run.
        let mut resolved_catalogs: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        for (cat_name, entries) in catalog_picks {
            let mut out: BTreeMap<String, aube_lockfile::CatalogEntry> = BTreeMap::new();
            for (pkg, spec) in entries {
                let resolved_for_pkg = resolved_versions.get(&pkg);
                let version = resolved_for_pkg
                    .and_then(|vs| vs.iter().find(|v| version_satisfies(v, &spec)).cloned())
                    .or_else(|| resolved_for_pkg.and_then(|vs| vs.first().cloned()))
                    .unwrap_or_else(|| spec.clone());
                out.insert(
                    pkg,
                    aube_lockfile::CatalogEntry {
                        specifier: spec,
                        version,
                    },
                );
            }
            if !out.is_empty() {
                resolved_catalogs.insert(cat_name, out);
            }
        }

        let canonical = LockfileGraph {
            importers,
            packages: resolved,
            settings: aube_lockfile::LockfileSettings {
                auto_install_peers: self.auto_install_peers,
                exclude_links_from_lockfile: self.exclude_links_from_lockfile,
                // Tarball-URL recording is a lockfile-writer concern; the
                // resolver never populates URLs itself. Install flips this
                // on after the graph is built when the setting is active.
                lockfile_include_tarball_url: false,
            },
            // Stamp the resolver's overrides into the output graph so the
            // lockfile writer can round-trip them and the next install's
            // drift check can compare them against the manifest.
            overrides: self.overrides.clone(),
            ignored_optional_dependencies: self.ignored_optional_dependencies.clone(),
            times: resolved_times,
            skipped_optional_dependencies,
            catalogs: resolved_catalogs,
        };

        // Second pass: hoist every auto-installed peer to its importer's
        // direct deps so pnpm-style `node_modules/<peer>` top-level
        // symlinks get created and the lockfile's `importers.` section
        // lists them the way pnpm does with `auto-install-peers=true`.
        // Skipped entirely when the setting is off — matches pnpm, which
        // leaves the importer's `dependencies` untouched in that mode.
        let hoisted = if self.auto_install_peers {
            hoist_auto_installed_peers(canonical)
        } else {
            canonical
        };

        // Third pass: compute peer-context suffixes for every reachable
        // package. See `apply_peer_contexts` for the details.
        let peer_options = PeerContextOptions {
            dedupe_peer_dependents: self.dedupe_peer_dependents,
            dedupe_peers: self.dedupe_peers,
            resolve_from_workspace_root: self.resolve_peers_from_workspace_root,
            peers_suffix_max_length: self.peers_suffix_max_length,
        };
        let contextualized = apply_peer_contexts(hoisted, &peer_options);
        tracing::debug!(
            "peer-context pass produced {} contextualized packages",
            contextualized.packages.len()
        );
        Ok(contextualized)
    }
}

/// A peer dependency whose declared range doesn't match the version the
/// tree actually ends up providing. Emitted as a warning by `aube install`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnmetPeer {
    /// dep_path of the package that declared the peer.
    pub from_dep_path: String,
    /// Human-friendly package name (pre-context) for display.
    pub from_name: String,
    /// Name of the peer being declared (e.g. `"react"`).
    pub peer_name: String,
    /// The declared peer range from the package's packument
    /// (e.g. `"^16.8.0 || ^17.0.0 || ^18.0.0"`).
    pub declared: String,
    /// What the tree actually provides, if anything. `None` means the
    /// peer is completely missing — rare in practice because the BFS
    /// auto-install path usually drags *some* version in, but it can
    /// happen for corner cases.
    pub found: Option<String>,
}

/// Scan the resolved graph and return every declared required peer whose
/// resolved version doesn't satisfy its declared range. Optional peers
/// (`peerDependenciesMeta.optional = true`) are skipped — pnpm treats
/// those as "warn suppressed" with `auto-install-peers=true`. The result
/// is purely informational; aube never fails an install on unmet peers,
/// matching pnpm.
///
/// The "found" version for each package comes from its own
/// `dependencies` map — the peer-context pass writes the resolved peer
/// tail there, so we don't have to re-walk ancestors. Any peer suffix on
/// the stored tail is stripped before the semver check so `18.2.0(foo@1)`
/// is treated as `18.2.0`.
pub fn detect_unmet_peers(graph: &LockfileGraph) -> Vec<UnmetPeer> {
    let mut unmet = Vec::new();
    for pkg in graph.packages.values() {
        for (peer_name, declared_range) in &pkg.peer_dependencies {
            let optional = pkg
                .peer_dependencies_meta
                .get(peer_name)
                .map(|m| m.optional)
                .unwrap_or(false);
            if optional {
                continue;
            }

            let found_tail = pkg.dependencies.get(peer_name);
            let found_version = found_tail.map(|t| t.split('(').next().unwrap_or(t).to_string());

            let satisfied = match &found_version {
                Some(v) => version_satisfies(v, declared_range),
                None => false,
            };
            if satisfied {
                continue;
            }

            unmet.push(UnmetPeer {
                from_dep_path: pkg.dep_path.clone(),
                from_name: pkg.name.clone(),
                peer_name: peer_name.clone(),
                declared: declared_range.clone(),
                found: found_version,
            });
        }
    }
    // Stable order for deterministic test output and readable warnings.
    unmet.sort_by(|a, b| {
        (a.from_dep_path.as_str(), a.peer_name.as_str())
            .cmp(&(b.from_dep_path.as_str(), b.peer_name.as_str()))
    });
    unmet
}

/// Promote unmet peers to importer direct deps.
///
/// Walks every resolved package's declared peer deps and hoists any
/// peer that isn't already a direct dep of the importer up to the
/// importer's `dependencies` list — what pnpm's
/// `auto-install-peers=true` produces in its v9 lockfile. If you
/// depend on a package whose `peerDependencies` declares `react` and
/// you don't list `react` yourself, pnpm (and now aube) adds it to
/// your importer's dependencies with the declared peer range as the
/// specifier, and the linker creates a top-level
/// `node_modules/react` symlink you can import from your own code.
///
/// Public so lockfile-driven installs that need to re-derive peer
/// wiring (npm/yarn/bun formats, which don't record peer contexts)
/// can run this before [`apply_peer_contexts`] to match fresh-resolve
/// behavior. Idempotent in the npm case: npm v7+ already hoists
/// auto-installed peers into root's `dependencies`, so they arrive
/// pre-`satisfied` and no additions are emitted.
///
/// Algorithm:
///   1. For each importer, collect the set of names already in its
///      direct deps. Those are "satisfied" and need no hoist.
///   2. DFS the reachable graph from the importer, visiting each package
///      and examining its `peer_dependencies` declarations. For each
///      declared peer not already satisfied by the importer, find a
///      resolved version somewhere in the graph and synthesize a
///      `DirectDep` entry. Mark it as satisfied so a second encounter
///      doesn't add a duplicate.
///   3. Stable: we walk in-order and take the first declared peer range
///      encountered per name as the specifier. Conflicting ranges across
///      the tree are not reconciled — first one wins. This matches pnpm
///      for the simple case; the complex case is deferred.
///
/// Leaves everything else about the graph untouched — no packages are
/// added or removed, only importer entries grow.
pub fn hoist_auto_installed_peers(mut graph: LockfileGraph) -> LockfileGraph {
    let importer_paths: Vec<String> = graph.importers.keys().cloned().collect();
    for importer_path in importer_paths {
        let Some(direct_deps) = graph.importers.get(&importer_path) else {
            continue;
        };
        let mut satisfied: std::collections::HashSet<String> =
            direct_deps.iter().map(|d| d.name.clone()).collect();

        let mut queue: std::collections::VecDeque<String> =
            direct_deps.iter().map(|d| d.dep_path.clone()).collect();
        let mut walked: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Additions are gathered into a separate vec so we don't mutate
        // the importer's direct-dep list while still borrowing from it.
        let mut additions: Vec<DirectDep> = Vec::new();

        while let Some(dep_path) = queue.pop_front() {
            if !walked.insert(dep_path.clone()) {
                continue;
            }
            let Some(pkg) = graph.packages.get(&dep_path) else {
                continue;
            };

            // Collect unmet peer declarations from this package.
            for (peer_name, peer_range) in &pkg.peer_dependencies {
                if satisfied.contains(peer_name) {
                    continue;
                }
                // Find any resolved version in the graph for this peer.
                // Prefer the one the package already wired via its own
                // dependencies map (the BFS auto-install result), and
                // fall back to scanning `graph.packages` for a name
                // match. If nothing matches, we quietly drop the peer —
                // that's the only path where aube stays stricter than
                // pnpm today; a future PR will emit an unmet warning.
                //
                // Fallback takes the semver-max version rather than
                // whatever `BTreeMap` iteration order surfaces first —
                // otherwise two resolved `react` entries like `18.0.0`
                // and `18.3.1` would pick the lexicographically-earlier
                // (older) one.
                let resolved_via_pkg_deps = pkg.dependencies.contains_key(peer_name);
                let resolved_version = pkg.dependencies.get(peer_name).cloned().or_else(|| {
                    // Filter to parseable semver versions *before* the
                    // max_by — returning `Equal` on parse failure makes
                    // the comparator non-transitive, so an unparseable
                    // entry sitting between two valid ones would cause
                    // `max_by` to pick an iteration-order-dependent
                    // result instead of the true maximum.
                    graph
                        .packages
                        .values()
                        .filter(|p| p.name == *peer_name)
                        .filter_map(|p| {
                            node_semver::Version::parse(&p.version)
                                .ok()
                                .map(|v| (v, p.version.clone()))
                        })
                        .max_by(|a, b| a.0.cmp(&b.0))
                        .map(|(_, s)| s)
                });
                let Some(version) = resolved_version else {
                    continue;
                };
                let canonical_version = version.split('(').next().unwrap_or(&version).to_string();
                let synth_dep_path = format!("{peer_name}@{canonical_version}");
                if !graph.packages.contains_key(&synth_dep_path) {
                    // The peer version the package wired didn't match an
                    // actual package entry — bail out for this peer
                    // rather than writing a dangling DirectDep.
                    continue;
                }
                satisfied.insert(peer_name.clone());
                // Peer reached via the fallback path isn't in
                // `pkg.dependencies`, so the normal "walk pkg's deps"
                // loop at the bottom of the while block would skip it.
                // Push it onto the queue directly so its own declared
                // peers get hoisted too.
                if !resolved_via_pkg_deps {
                    queue.push_back(synth_dep_path.clone());
                }
                additions.push(DirectDep {
                    name: peer_name.clone(),
                    dep_path: synth_dep_path,
                    // Peers auto-hoisted to the root are in the prod
                    // graph by convention — matches what pnpm writes.
                    dep_type: DepType::Production,
                    specifier: Some(peer_range.clone()),
                });
            }

            // Queue the package's own resolved deps for further walking.
            for (child_name, child_version_tail) in &pkg.dependencies {
                let canonical = child_version_tail
                    .split('(')
                    .next()
                    .unwrap_or(child_version_tail);
                queue.push_back(format!("{child_name}@{canonical}"));
            }
        }

        if !additions.is_empty() {
            tracing::debug!(
                "hoisted {} auto-installed peer(s) into importer {}",
                additions.len(),
                importer_path
            );
            if let Some(deps) = graph.importers.get_mut(&importer_path) {
                deps.extend(additions);
                deps.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
    }
    graph
}

/// Walk the resolved graph top-down from each importer and compute a
/// peer-dependency context for every package, producing a new graph whose
/// dep_paths carry pnpm-style `(peer@ver)` suffixes.
///
/// The goal is parity with pnpm's v9 lockfile output: the same
/// `name@version` can appear multiple times — once per distinct set of peer
/// resolutions — so different subtrees that pin incompatible peers get
/// isolated virtual-store entries and truly different sibling-symlink
/// neighborhoods.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue above auto-installed it as a transitive, which matches
///     pnpm's `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` (rewritten to point at contextualized
///     children) and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
///
/// Nested peer suffixes: pnpm writes `(react-dom@18.2.0(react@18.2.0))`
/// when a declared peer has its own resolved peers. A single top-down
/// DFS pass can't produce that form, because when a parent P records
/// a peer version in its children's scope, it only knows the canonical
/// tail — the peer's OWN suffix is computed later when the peer itself
/// gets visited. We solve this by running `apply_peer_contexts_once` in
/// a fixed-point loop: the second iteration's input has Pass 1's
/// contextualized tails in every `pkg.dependencies` map, so when a
/// descendant looks a peer up in ancestor scope it sees the full
/// nested tail and serializes it as such. Most peer chains converge in
/// 2–3 iterations; we cap at 16 as a safety belt.
///
/// Limitations (documented as follow-ups in the README):
///   - No per-peer range satisfaction — we take whatever the ancestor has,
///     even if it technically doesn't match P's declared peer range.
///
/// Knobs controlling the peer-context pass. Plumbed from four
/// pnpm-compatible settings (`dedupe-peer-dependents`, `dedupe-peers`,
/// `resolve-peers-from-workspace-root`, `peers-suffix-max-length`)
/// through the `Resolver`'s `with_*` setters.
#[derive(Debug, Clone, Copy)]
pub struct PeerContextOptions {
    /// When true, run the cross-subtree peer-variant collapse pass
    /// after every iteration of the fixed-point loop. Matches pnpm's
    /// default.
    pub dedupe_peer_dependents: bool,
    /// When true, emit suffixes as `(version)` instead of
    /// `(name@version)`. Affects both the package key, the reference
    /// tails stored in `dependencies`, and the cycle-break form of
    /// `contains_canonical_back_ref`.
    pub dedupe_peers: bool,
    /// When true, unresolved peers can be satisfied by a dep declared
    /// at the root importer (`"."`) even if no ancestor scope carries
    /// the peer. Runs between own-deps and graph-wide scan in the
    /// peer-context visitor — see `visit_peer_context` in this
    /// module for the owning implementation (intentionally crate-
    /// private; the public API here is the option flag itself).
    pub resolve_from_workspace_root: bool,
    /// Byte cap on the peer-ID suffix after which the entire suffix
    /// is hashed to `_<10-char-sha256-hex>`. pnpm's default is 1000.
    pub peers_suffix_max_length: usize,
}

impl Default for PeerContextOptions {
    fn default() -> Self {
        Self {
            dedupe_peer_dependents: true,
            dedupe_peers: false,
            resolve_from_workspace_root: true,
            peers_suffix_max_length: 1000,
        }
    }
}

/// Compute peer-context suffixes over an already-resolved graph.
///
/// Takes a *canonical* graph — one `LockedPackage` per `(name,
/// version)` with `peer_dependencies` populated — and produces a
/// *contextualized* graph whose keys and transitive references carry
/// `(peer@ver)` suffixes when packages resolve peers differently in
/// different subtrees. Drives the sibling-symlink wiring in
/// `aube-linker` for peers, so every fetch/materialize site sees a
/// per-context identity for any package whose peers disambiguate.
///
/// Public so lockfile-driven installs can run the pass over graphs
/// parsed from npm/yarn/bun lockfiles (which emit canonical form —
/// no peer suffixes — and would otherwise leave peer-dependent
/// packages without their peers as `.aube/<pkg>/node_modules/<peer>`
/// siblings). Fresh resolves call it internally from
/// `Resolver::resolve`.
pub fn apply_peer_contexts(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    const MAX_ITERATIONS: usize = 16;
    let mut current = canonical;
    let mut previous_keys: Option<std::collections::BTreeSet<String>> = None;
    let mut converged = false;
    for i in 0..MAX_ITERATIONS {
        let after_once = apply_peer_contexts_once(current, options);
        let next = if options.dedupe_peer_dependents {
            dedupe_peer_variants(after_once)
        } else {
            after_once
        };
        let next_keys: std::collections::BTreeSet<String> = next.packages.keys().cloned().collect();
        if previous_keys.as_ref() == Some(&next_keys) {
            tracing::debug!("peer-context pass converged after {i} iteration(s)");
            current = next;
            converged = true;
            break;
        }
        previous_keys = Some(next_keys);
        current = next;
    }
    if !converged {
        tracing::warn!(
            "peer-context pass hit MAX_ITERATIONS={MAX_ITERATIONS} without converging — \
             lockfile may not be byte-identical to pnpm's nested form"
        );
    }
    // `dedupe-peers=true` rewrites the parenthesized peer suffix to
    // drop the `name@` prefix. Done as a post-pass rather than inline
    // so cycle detection during the fixed-point loop keeps the full
    // `name@version` form (otherwise unrelated same-version packages
    // would false-positive as back-references).
    if options.dedupe_peers {
        dedupe_peer_suffixes(current)
    } else {
        current
    }
}

/// Cross-subtree peer-variant dedupe. When `dedupe-peer-dependents` is
/// on, packages that landed at different contextualized dep_paths but
/// resolved every declared peer to the *same* version (ignoring the
/// nested peer suffix on each peer tail) collapse into a single
/// canonical variant — chosen as the lexicographically smallest key in
/// the equivalence class. References in every surviving
/// `LockedPackage.dependencies` map and every `importers[*]` direct
/// dep get rewritten through the old→canonical map, and the
/// non-canonical entries are dropped from `packages`.
///
/// Packages whose `peer_dependencies` map is empty — i.e. the canonical
/// base already has only one variant — are skipped.
fn dedupe_peer_variants(graph: LockfileGraph) -> LockfileGraph {
    let canonical_base = |key: &str| -> String { key.split('(').next().unwrap_or(key).to_string() };
    // Only the peer-bearing part of the resolved peer tail is
    // comparable across subtrees — the nested suffix could differ even
    // for peer-equivalent variants on mid-iterations of the outer
    // fixed-point loop.
    let peer_base = |tail: &str| -> String { tail.split('(').next().unwrap_or(tail).to_string() };

    // Group dep_paths by their peer-free base name.
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in graph.packages.keys() {
        groups
            .entry(canonical_base(key))
            .or_default()
            .push(key.clone());
    }

    let mut rewrite: BTreeMap<String, String> = BTreeMap::new();
    for (_base, mut keys) in groups {
        if keys.len() < 2 {
            continue;
        }
        // Deterministic order for canonical selection + stable hashing.
        keys.sort();
        // Union-find over equivalence classes. Two variants are
        // equivalent when each declared peer name resolves to the same
        // peer base in both (or is missing from both).
        let mut parent: Vec<usize> = (0..keys.len()).collect();
        fn find(parent: &mut [usize], i: usize) -> usize {
            if parent[i] == i {
                i
            } else {
                let r = find(parent, parent[i]);
                parent[i] = r;
                r
            }
        }
        for i in 0..keys.len() {
            for j in (i + 1)..keys.len() {
                let pa = &graph.packages[&keys[i]];
                let pb = &graph.packages[&keys[j]];
                // Same canonical version is required — packages with
                // different versions but the same name would share no
                // canonical_base only if the name-without-version
                // collided, which doesn't happen (version is in the
                // base). Still, belt-and-suspenders.
                if pa.version != pb.version {
                    continue;
                }
                let peer_names: BTreeSet<&String> = pa
                    .peer_dependencies
                    .keys()
                    .chain(pb.peer_dependencies.keys())
                    .collect();
                let equivalent = peer_names.iter().all(|name| {
                    match (
                        pa.dependencies.get(name.as_str()),
                        pb.dependencies.get(name.as_str()),
                    ) {
                        (Some(va), Some(vb)) => peer_base(va) == peer_base(vb),
                        (None, None) => true,
                        _ => false,
                    }
                });
                if equivalent {
                    let ri = find(&mut parent, i);
                    let rj = find(&mut parent, j);
                    if ri != rj {
                        parent[ri] = rj;
                    }
                }
            }
        }
        // Build class → canonical (smallest key) mapping. Using
        // index-based iteration here because `find` takes a mutable
        // reference into `parent`, so holding an immutable borrow
        // from `keys.iter()` at the same time would double-borrow.
        #[allow(clippy::needless_range_loop)]
        {
            let mut class_rep: BTreeMap<usize, String> = BTreeMap::new();
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                class_rep
                    .entry(root)
                    .and_modify(|cur| {
                        if keys[i] < *cur {
                            *cur = keys[i].clone();
                        }
                    })
                    .or_insert_with(|| keys[i].clone());
            }
            for i in 0..keys.len() {
                let root = find(&mut parent, i);
                let canonical = class_rep[&root].clone();
                if keys[i] != canonical {
                    rewrite.insert(keys[i].clone(), canonical);
                }
            }
        }
    }

    if rewrite.is_empty() {
        return graph;
    }

    // Rewrite package dependency tails and keep only canonicals.
    let LockfileGraph {
        importers,
        packages,
        settings,
        overrides,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
    } = graph;

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (key, mut pkg) in packages {
        if rewrite.contains_key(&key) {
            continue;
        }
        for (dep_name, dep_tail) in pkg.dependencies.iter_mut() {
            let dep_key = format!("{dep_name}@{dep_tail}");
            if let Some(canonical) = rewrite.get(&dep_key) {
                let new_tail = canonical
                    .strip_prefix(&format!("{dep_name}@"))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| canonical.clone());
                *dep_tail = new_tail;
            }
        }
        new_packages.insert(key, pkg);
    }

    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();
    for (importer_path, deps) in importers {
        let mut new_deps = Vec::with_capacity(deps.len());
        for mut dep in deps {
            if let Some(canonical) = rewrite.get(&dep.dep_path) {
                dep.dep_path = canonical.clone();
            }
            new_deps.push(dep);
        }
        new_importers.insert(importer_path, new_deps);
    }

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings,
        overrides,
        ignored_optional_dependencies,
        times,
        skipped_optional_dependencies,
        catalogs,
    }
}

/// Single pass of the peer-context computation. See `apply_peer_contexts`
/// for the wrapping fixed-point loop.
///
/// Algorithm per visited package P, reached at some point in a DFS from an
/// importer with `ancestor_scope: name -> dep_path_tail`:
///
///  1. For each peer name declared by P, look it up in `ancestor_scope`
///     (nearest-ancestor-wins, since the scope is rebuilt per recursion).
///     If missing, fall back to P's own entry in `dependencies` — the BFS
///     enqueue auto-installed it as a transitive, matching pnpm's
///     `auto-install-peers=true` default.
///  2. Sort the (peer_name, resolution) pairs and serialize as
///     `(n1@v1)(n2@v2)…` for the suffix.
///  3. Produce a contextualized dep_path `name@version{suffix}`. If that
///     key is already in `out_packages` (or currently on the DFS stack via
///     `visiting`), short-circuit — we've already emitted this variant.
///  4. Build a new scope for P's children by merging the ancestor scope
///     with P's own `dependencies` and the resolved peer map. Recurse.
///  5. Emit the contextualized LockedPackage.
///
/// Cycles: protected by `visiting` — if a package is re-entered via a
/// dependency cycle, we return the already-computed dep_path without
/// recursing again. The peer context is fixed at first visit; any cycle
/// traversal uses whatever context was live at that first visit.
fn apply_peer_contexts_once(
    canonical: LockfileGraph,
    options: &PeerContextOptions,
) -> LockfileGraph {
    let mut out_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    let mut new_importers: BTreeMap<String, Vec<DirectDep>> = BTreeMap::new();

    // Root-importer scope used by `resolve-peers-from-workspace-root`.
    // Computed once from the canonical input so it reflects the
    // contextualized state of every root dep on fixed-point iterations
    // 2+ — same logic as per-importer `importer_scope` below.
    let root_scope: BTreeMap<String, String> = canonical
        .importers
        .get(".")
        .map(|deps| {
            deps.iter()
                .map(|d| {
                    let tail = d
                        .dep_path
                        .strip_prefix(&format!("{}@", d.name))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| d.dep_path.clone());
                    (d.name.clone(), tail)
                })
                .collect()
        })
        .unwrap_or_default();

    for (importer_path, direct_deps) in &canonical.importers {
        // An importer's own direct deps are in scope for its children's
        // peer resolution — this is how pnpm's "auto-install at the root"
        // path gets peer links that point at root-level packages.
        //
        // Use the *full contextualized tail* off each DirectDep rather
        // than the package's plain version. On Pass 1 of the fixed-point
        // loop the tail is canonical and equal to `p.version`; on Pass 2+
        // it's already contextualized, and passing the plain version
        // would make descendants look up keys that don't exist in the
        // (now-nested) graph.
        let importer_scope: BTreeMap<String, String> = direct_deps
            .iter()
            .map(|d| {
                let tail = d
                    .dep_path
                    .strip_prefix(&format!("{}@", d.name))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| d.dep_path.clone());
                (d.name.clone(), tail)
            })
            .collect();

        let mut new_deps = Vec::with_capacity(direct_deps.len());
        for dep in direct_deps {
            // `visiting` is the DFS stack guard for this particular descent
            // — reset per direct dep so we don't incorrectly flag a package
            // as a cycle when it's reached again from a sibling subtree.
            // The shared `out_packages` still dedupes across siblings since
            // the second visit hits the `contains_key` short-circuit below.
            //
            // Invariant (see `visit_peer_context` for the detailed handling):
            // a dep_path returned from the cycle-break branch may not yet
            // be present in `out_packages` at the moment of return, because
            // the package is still being assembled up the call stack. The
            // parent that records the returned tail will complete its own
            // insertion before the recursion unwinds, so by the time
            // anything reads the graph, every referenced dep_path exists.
            let mut visiting: std::collections::HashSet<String> = std::collections::HashSet::new();
            let new_dep_path = visit_peer_context(
                &dep.dep_path,
                &canonical,
                &importer_scope,
                &root_scope,
                &mut out_packages,
                &mut visiting,
                options,
            )
            .unwrap_or_else(|| dep.dep_path.clone());
            new_deps.push(DirectDep {
                name: dep.name.clone(),
                dep_path: new_dep_path,
                dep_type: dep.dep_type,
                specifier: dep.specifier.clone(),
            });
        }
        new_importers.insert(importer_path.clone(), new_deps);
    }

    // Any canonical package that was never reached by the DFS (orphaned
    // from every importer) is dropped — that matches the filter_deps
    // semantics and avoids emitting dead entries into the lockfile.

    LockfileGraph {
        importers: new_importers,
        packages: out_packages,
        // The post-pass is pure — settings + overrides carry through
        // from the input graph untouched.
        settings: canonical.settings,
        overrides: canonical.overrides,
        ignored_optional_dependencies: canonical.ignored_optional_dependencies,
        times: canonical.times,
        skipped_optional_dependencies: canonical.skipped_optional_dependencies,
        catalogs: canonical.catalogs,
    }
}

/// DFS helper for `apply_peer_contexts`. Returns the peer-contextualized
/// dep_path of the visited package, or `None` if the canonical package is
/// missing (shouldn't happen in practice but we degrade gracefully).
/// Does `value` contain a peer-suffix reference to `canonical` as a
/// proper name@version boundary (i.e. preceded by `(` and followed by
/// `(` / `)` / end-of-string)? Used by the peer-context pass to detect
/// when a nested tail loops back to the current package so it can
/// short-circuit the chain instead of growing the suffix forever.
/// If `s` ends with `_<10 lowercase hex>` (the marker written by
/// `hash_peer_suffix`), strip it and return the prefix. Otherwise
/// return `s` unchanged.
///
/// Safe against false positives: `s` here is always a post-split
/// `name@version` base, and semver forbids `_` inside a version, so
/// an underscore 10 chars from the end of `name@version` can only be
/// our marker.
fn strip_hashed_peer_suffix(s: &str) -> &str {
    const MARKER_LEN: usize = 11; // `_` + 10 hex chars
    if s.len() < MARKER_LEN {
        return s;
    }
    let tail = &s[s.len() - MARKER_LEN..];
    if !tail.starts_with('_') {
        return s;
    }
    if tail[1..]
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        &s[..s.len() - MARKER_LEN]
    } else {
        s
    }
}

/// Hash a peer-ID suffix with SHA-256 and return `_<10-char-hex>`.
/// Used by the peer-context pass when the raw suffix length exceeds
/// `peersSuffixMaxLength`. Matches pnpm's format so lockfile dep_path
/// keys stay portable.
fn hash_peer_suffix(suffix: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(suffix.as_bytes());
    let mut out = String::with_capacity(11);
    out.push('_');
    for byte in digest.iter().take(5) {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn contains_canonical_back_ref(value: &str, canonical: &str) -> bool {
    let bytes = value.as_bytes();
    let target = canonical.as_bytes();
    if target.is_empty() || target.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i + target.len() <= bytes.len() {
        if &bytes[i..i + target.len()] == target {
            let before = if i == 0 { b'\0' } else { bytes[i - 1] };
            let after = bytes.get(i + target.len()).copied().unwrap_or(b'\0');
            let before_ok = before == b'(';
            let after_ok = after == b'(' || after == b')' || after == b'\0';
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Dedupe-peers post-pass: strip the `name@` prefix from every
/// parenthesized peer segment in every dep_path key and reference,
/// turning `react-dom@18.2.0(react@18.2.0)` into
/// `react-dom@18.2.0(18.2.0)`. Nested segments get the same treatment
/// so `a@1(b@2(c@3))` becomes `a@1(2(3))`.
///
/// Running this as a final post-pass (instead of inline during suffix
/// assembly in `visit_peer_context`) keeps cycle detection correct:
/// the detection path works against the full `name@version` form
/// throughout the fixed-point loop, and only the serialized output
/// gets the shorter form. A version-only inline approach would
/// false-positive on unrelated packages that coincidentally share a
/// version with the current package's canonical base.
///
/// Pure: no-op when `dedupe_peers` is off (caller gates the call);
/// otherwise rewrites every package key, every `LockedPackage.dep_path`
/// and `LockedPackage.dependencies` value, and every `importers[*]`
/// DirectDep `dep_path` through the same `apply_dedupe_peers_to_tail`
/// helper. Package bodies (integrity, metadata, etc.) are cloned
/// verbatim.
fn dedupe_peer_suffixes(graph: LockfileGraph) -> LockfileGraph {
    // Pass 1: compute the intended deduped key for each package and
    // tally how many distinct full-form keys map to it. Stripping
    // `name@` from suffix segments is lossy — two variants whose peer
    // *names* differ but whose peer *versions* coincide would collapse
    // onto the same deduped key (e.g. `consumer@1.0.0(foo@1.0.0)` and
    // `consumer@1.0.0(bar@1.0.0)` both → `consumer@1.0.0(1.0.0)`).
    // `dedupe_peer_variants` already merged the peer-equivalent
    // duplicates, so any remaining collision here represents genuinely
    // distinct variants — losing one would silently drop its
    // dependency wiring. We detect those collisions and keep both
    // sides in full form.
    let mut target_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut intended: BTreeMap<String, String> = BTreeMap::new();
    for key in graph.packages.keys() {
        let new_key = apply_dedupe_peers_to_key(key);
        *target_counts.entry(new_key.clone()).or_insert(0) += 1;
        intended.insert(key.clone(), new_key);
    }
    let rewrite: BTreeMap<String, String> = intended
        .into_iter()
        .map(|(old, new)| {
            if target_counts.get(&new).copied().unwrap_or(0) > 1 {
                tracing::warn!(
                    "dedupe-peers: collision on {new} — keeping {old} in full form to avoid \
                     dropping a distinct peer-variant"
                );
                (old.clone(), old)
            } else {
                (old, new)
            }
        })
        .collect();

    // Rewrite a `(child_name, tail)` reference by reconstructing the
    // target's full-form key, looking up its effective rewrite, and
    // stripping `child_name@` off the result to recover the tail.
    // Tails always follow their target package's rewrite decision,
    // so references stay consistent when a collision forces a target
    // back to full form.
    let rewrite_tail = |child_name: &str, tail: &str| -> String {
        let old_key = format!("{child_name}@{tail}");
        match rewrite.get(&old_key) {
            Some(new_key) => new_key
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| tail.to_string()),
            None => apply_dedupe_peers_to_tail(tail),
        }
    };

    let mut new_packages: BTreeMap<String, LockedPackage> = BTreeMap::new();
    for (old_key, pkg) in graph.packages {
        let new_key = rewrite
            .get(&old_key)
            .cloned()
            .unwrap_or_else(|| old_key.clone());
        let new_dependencies: BTreeMap<String, String> = pkg
            .dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        let new_optional_dependencies: BTreeMap<String, String> = pkg
            .optional_dependencies
            .into_iter()
            .map(|(n, v)| {
                let new_v = rewrite_tail(&n, &v);
                (n, new_v)
            })
            .collect();
        new_packages.insert(
            new_key.clone(),
            LockedPackage {
                name: pkg.name,
                version: pkg.version,
                integrity: pkg.integrity,
                dependencies: new_dependencies,
                optional_dependencies: new_optional_dependencies,
                peer_dependencies: pkg.peer_dependencies,
                peer_dependencies_meta: pkg.peer_dependencies_meta,
                dep_path: new_key,
                local_source: pkg.local_source,
                os: pkg.os,
                cpu: pkg.cpu,
                libc: pkg.libc,
                bundled_dependencies: pkg.bundled_dependencies,
                tarball_url: pkg.tarball_url,
                alias_of: pkg.alias_of,
                yarn_checksum: pkg.yarn_checksum,
            },
        );
    }

    let new_importers: BTreeMap<String, Vec<DirectDep>> = graph
        .importers
        .into_iter()
        .map(|(path, deps)| {
            let rewritten = deps
                .into_iter()
                .map(|d| {
                    let new_dep_path = rewrite
                        .get(&d.dep_path)
                        .cloned()
                        .unwrap_or_else(|| apply_dedupe_peers_to_key(&d.dep_path));
                    DirectDep {
                        name: d.name,
                        dep_path: new_dep_path,
                        dep_type: d.dep_type,
                        specifier: d.specifier,
                    }
                })
                .collect();
            (path, rewritten)
        })
        .collect();

    LockfileGraph {
        importers: new_importers,
        packages: new_packages,
        settings: graph.settings,
        overrides: graph.overrides,
        ignored_optional_dependencies: graph.ignored_optional_dependencies,
        times: graph.times,
        skipped_optional_dependencies: graph.skipped_optional_dependencies,
        catalogs: graph.catalogs,
    }
}

/// Strip `name@` from inside every parenthesized segment of a full
/// dep_path key (e.g. `react-dom@18.2.0(react@18.2.0)` →
/// `react-dom@18.2.0(18.2.0)`). The first `name@version` outside any
/// parens is preserved verbatim — that's the canonical head of the
/// dep_path and `dedupe-peers` only affects the peer suffix.
fn apply_dedupe_peers_to_key(key: &str) -> String {
    let mut parts = key.split('(');
    let Some(first) = parts.next() else {
        return key.to_string();
    };
    let mut out = String::with_capacity(key.len());
    out.push_str(first);
    for part in parts {
        out.push('(');
        // In a well-formed key, `part` looks like `name@version)` /
        // `name@version` / `version)` / ... We strip everything up to
        // and including the LAST `@` (scoped packages like
        // `@types/react@18.2.0` contain two `@`s; the separator is the
        // rightmost one). We only strip if that `@` comes before the
        // first `)` or `(` (i.e. the segment actually starts with
        // `name@`, not the outer parens closing with no name inside).
        if let Some(at_idx) = part.rfind('@') {
            let close_idx = part.find([')', '(']).unwrap_or(usize::MAX);
            if at_idx < close_idx {
                out.push_str(&part[at_idx + 1..]);
                continue;
            }
        }
        out.push_str(part);
    }
    out
}

/// Same as [`apply_dedupe_peers_to_key`] but for dep-tail values
/// stored in `LockedPackage.dependencies` (e.g. `18.2.0(react@18.2.0)`
/// → `18.2.0(18.2.0)`). Tails differ from keys only by lacking the
/// leading `name@` prefix — both use the same parens-based suffix
/// shape, so the algorithm is identical.
fn apply_dedupe_peers_to_tail(tail: &str) -> String {
    apply_dedupe_peers_to_key(tail)
}

fn visit_peer_context(
    input_dep_path: &str,
    graph: &LockfileGraph,
    ancestor_scope: &BTreeMap<String, String>,
    root_scope: &BTreeMap<String, String>,
    out_packages: &mut BTreeMap<String, LockedPackage>,
    visiting: &mut std::collections::HashSet<String>,
    options: &PeerContextOptions,
) -> Option<String> {
    let pkg = graph.packages.get(input_dep_path)?;

    // The input key may already carry a peer suffix (fixed-point loop
    // Pass 2+). Drop it before we build a new one — otherwise we'd
    // append the new suffix on top of the old and grow unboundedly
    // across iterations (classic mutual-peer-cycle blow-up).
    //
    // Two suffix forms can be present from a prior pass:
    //   1. `(name@version)(…)` — the normal nested peer suffix. Stripped
    //      by splitting on the first `(`.
    //   2. `_<10-char-sha256-hex>` — the hashed form produced when the
    //      normal suffix exceeded `peersSuffixMaxLength`. Must also be
    //      stripped; otherwise each pass re-hashes the already-hashed
    //      key and appends another marker (exposed by the
    //      `peer_suffix_is_hashed_when_exceeding_cap` unit test).
    let canonical_base = input_dep_path.split('(').next().unwrap_or(input_dep_path);
    let canonical_base = strip_hashed_peer_suffix(canonical_base).to_string();

    // Compute peer context: walk declared peers, resolve from ancestors
    // (nearest wins — the scope is rebuilt as we recurse) or from the
    // package's own dependency map as the auto-install fallback. Both
    // sides may produce nested tails on the second and later iterations
    // of the fixed-point loop.
    // Resolution source priority for each declared peer:
    //   1. Ancestor scope — if the ancestor's version actually
    //      satisfies the declared peer range. Different subtrees can
    //      pin different versions of the same peer name (classic
    //      `lib-a peers on react@^17`, `lib-b peers on react@^18`),
    //      and silently reusing the ancestor's version regardless of
    //      the declared range would force both libs onto the same
    //      version — exactly the behavior we want to fix here.
    //   2. The current package's own `pkg.dependencies` entry — the
    //      BFS peer-walk enqueued this peer with the declared range,
    //      so whatever got picked there is guaranteed to satisfy.
    //   3. A graph-wide scan as a last resort: any package whose name
    //      matches and whose version satisfies the declared range.
    //      This keeps nested-context callers from losing their peer
    //      resolution when neither ancestor nor own-deps has it.
    //   4. If no satisfying version exists, fall back to the nearest
    //      incompatible ancestor/root/pkg dependency. pnpm still wires
    //      that user-declared version into the peer context and then
    //      reports the semver mismatch; omitting it would produce a
    //      weaker "missing peer" warning and an unsuffixed snapshot.
    //
    // If nothing in the graph satisfies, the peer is left out of the
    // context entirely — `detect_unmet_peers` will surface it as a
    // warning after the pass.
    let mut peer_context: Vec<(String, String)> = Vec::new();
    for (peer_name, declared_range) in &pkg.peer_dependencies {
        let satisfies_declared = |v: &str| -> bool {
            // The tail may carry a nested peer suffix on fixed-point
            // iterations 2+; strip it before checking the semver.
            let canonical = v.split('(').next().unwrap_or(v);
            version_satisfies(canonical, declared_range)
        };

        let from_ancestor = ancestor_scope
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_ancestor_incompatible = ancestor_scope.get(peer_name).cloned();

        let from_pkg_deps = pkg
            .dependencies
            .get(peer_name)
            .filter(|v| satisfies_declared(v))
            .cloned();
        let from_pkg_deps_incompatible = pkg.dependencies.get(peer_name).cloned();

        // `resolve-peers-from-workspace-root`: fall back to the root
        // importer's direct deps before the graph-wide scan. Common in
        // monorepos where the workspace root pins shared peers (e.g.
        // `react`) that leaf packages peer on without declaring them
        // in their own subtree. Skipped when the setting is off —
        // matches pnpm's `resolve-peers-from-workspace-root=false`.
        let from_root = if options.resolve_from_workspace_root {
            root_scope
                .get(peer_name)
                .filter(|v| satisfies_declared(v))
                .cloned()
        } else {
            None
        };
        let from_root_incompatible = if options.resolve_from_workspace_root {
            root_scope.get(peer_name).cloned()
        } else {
            None
        };

        // Return the full dep_path TAIL (the part after `name@`), not
        // just `p.version`. On fixed-point iteration 2+, the input
        // graph's keys are contextualized — e.g. `react-dom` lives at
        // `react-dom@18.2.0(react@18.2.0)`. Downstream code
        // reconstructs the child lookup key with
        // `format!("{child_name}@{tail}")` and needs the tail to
        // match whatever the graph has keyed it under, otherwise the
        // lookup returns None and the peer gets silently dropped
        // from `new_dependencies`. The semver check is against the
        // package's canonical `version` field, not the tail, because
        // the tail may carry a peer suffix that isn't valid semver.
        let from_graph_scan = || {
            graph
                .packages
                .values()
                .filter(|p| p.name == *peer_name)
                .filter(|p| version_satisfies(&p.version, declared_range))
                .filter_map(|p| {
                    let tail = p
                        .dep_path
                        .strip_prefix(&format!("{}@", p.name))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| p.version.clone());
                    node_semver::Version::parse(&p.version)
                        .ok()
                        .map(|ver| (ver, tail))
                })
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, tail)| tail)
        };

        if let Some(version) = from_ancestor
            .or(from_pkg_deps)
            .or(from_root)
            .or_else(from_graph_scan)
            .or(from_ancestor_incompatible)
            .or(from_pkg_deps_incompatible)
            .or(from_root_incompatible)
        {
            peer_context.push((peer_name.clone(), version));
        }
    }
    peer_context.sort_by(|a, b| a.0.cmp(&b.0));

    // For the SUFFIX we build a cycle-broken copy: any peer value that
    // nests a reference back to the current package's canonical base
    // gets stripped to its plain version. Without this, mutual peer
    // cycles (a peers on b, b peers on a) grow the suffix one level
    // per iteration of the fixed-point loop and never converge.
    //
    // The non-cycle paths are untouched, so a regular nested chain
    // like `(react-dom@18.2.0(react@18.2.0))` still serializes fully.
    // We deliberately keep the full nested tails in `peer_context` for
    // downstream scope propagation and child lookups — suffix cycle-
    // breaking is cosmetic and should not change what packages exist
    // or which snapshot entries reference each other.
    //
    // Cycle detection is always done against the full `name@version`
    // canonical base — even when `dedupe-peers=true` is on, because
    // the version-only form is ambiguous (two unrelated packages at
    // the same version would false-positive). `dedupe-peers` is
    // applied as a post-pass over the final graph in
    // `dedupe_peer_suffixes` after cycle detection is done.
    let suffix: String = peer_context
        .iter()
        .map(|(n, v)| {
            let cycles_back = contains_canonical_back_ref(v, &canonical_base);
            let display_v = if cycles_back {
                v.split('(').next().unwrap_or(v).to_string()
            } else {
                v.clone()
            };
            format!("({n}@{display_v})")
        })
        .collect();
    // pnpm's `peersSuffixMaxLength`: when the built suffix exceeds the
    // cap, replace the entire suffix with `_<10-char-sha256-hex>` so the
    // lockfile key stays bounded. Matches pnpm's lockfile format, so
    // lockfiles shared between aube and pnpm stay comparable.
    let effective_suffix = if suffix.len() > options.peers_suffix_max_length {
        hash_peer_suffix(&suffix)
    } else {
        suffix
    };
    let contextualized = format!("{canonical_base}{effective_suffix}");

    if out_packages.contains_key(&contextualized) || visiting.contains(&contextualized) {
        return Some(contextualized);
    }
    visiting.insert(contextualized.clone());

    // Build the scope for P's children. This is ancestor_scope, overlaid
    // with P's own dependencies and its resolved peer map. Children see
    // their grandparents too — this mirrors pnpm's all-the-way-up peer
    // walk.
    //
    // We deliberately do NOT strip any existing peer-context suffix
    // off the tails we put into the scope. On the first pass the
    // values are plain (BFS output has no suffixes), so preserving
    // them is a no-op; on subsequent passes (see the fixed-point loop
    // in `apply_peer_contexts`) the input graph already carries
    // contextualized tails, and keeping them in scope is exactly how
    // nested peer suffixes propagate down to consumers — a package
    // that peers on `react-dom` and reaches it through a parent whose
    // `react-dom` entry is already `18.2.0(react@18.2.0)` will see
    // that nested tail in its own scope, and its own suffix will
    // serialize as `(react-dom@18.2.0(react@18.2.0))`. That's the
    // nested form pnpm writes.
    let mut child_scope = ancestor_scope.clone();
    for (name, version) in &pkg.dependencies {
        child_scope.insert(name.clone(), version.clone());
    }
    for (name, version) in &peer_context {
        child_scope.insert(name.clone(), version.clone());
    }

    // Recurse into each child, rewriting its dependency map entry to
    // point at the contextualized dep_path's tail. A child whose visit
    // fails (orphaned / missing) keeps its own tail.
    //
    // For declared peer names, the peer context (filled from the
    // ancestor scope) is authoritative — we override whatever the BFS
    // peer walk auto-installed. Otherwise the snapshot suffix and the
    // actual wired `dependencies[peer]` could disagree, which made the
    // sibling symlink target inconsistent with the peer-context claim.
    // When the ancestor's version doesn't satisfy the declared range,
    // `detect_unmet_peers` will flag it as a warning after the pass.
    let peer_context_versions: BTreeMap<String, String> = peer_context.iter().cloned().collect();

    let mut new_dependencies: BTreeMap<String, String> = BTreeMap::new();
    let mut visited_dep_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (child_name, child_version_tail) in &pkg.dependencies {
        // If this child is a declared peer, its tail comes from the
        // peer context (which may be nested). Otherwise we use the
        // tail we already have — also possibly nested on a 2nd pass.
        let lookup_tail = match peer_context_versions.get(child_name) {
            Some(v) => v.clone(),
            None => child_version_tail.clone(),
        };
        let child_canonical_dep_path = format!("{child_name}@{lookup_tail}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        let new_tail = match child_new {
            Some(new_dep_path) => new_dep_path
                .strip_prefix(&format!("{child_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| lookup_tail.clone()),
            None => lookup_tail.clone(),
        };
        new_dependencies.insert(child_name.clone(), new_tail);
        visited_dep_names.insert(child_name.clone());
    }

    // Peers that were satisfied purely from the ancestor scope may not
    // have been in `pkg.dependencies` at all (no auto-install needed).
    // Wire them as deps now so the linker creates the sibling symlink
    // and the lockfile snapshot records them.
    for (peer_name, peer_version) in &peer_context {
        if visited_dep_names.contains(peer_name) {
            continue;
        }
        let child_canonical_dep_path = format!("{peer_name}@{peer_version}");
        let child_new = visit_peer_context(
            &child_canonical_dep_path,
            graph,
            &child_scope,
            root_scope,
            out_packages,
            visiting,
            options,
        );
        if let Some(new_dep_path) = child_new {
            let new_tail = new_dep_path
                .strip_prefix(&format!("{peer_name}@"))
                .map(|s| s.to_string())
                .unwrap_or_else(|| peer_version.clone());
            new_dependencies.insert(peer_name.clone(), new_tail);
        }
    }

    visiting.remove(&contextualized);
    let new_optional_dependencies: BTreeMap<String, String> = pkg
        .optional_dependencies
        .keys()
        .filter_map(|name| {
            new_dependencies
                .get(name)
                .map(|tail| (name.clone(), tail.clone()))
        })
        .collect();

    out_packages.insert(
        contextualized.clone(),
        LockedPackage {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            integrity: pkg.integrity.clone(),
            dependencies: new_dependencies,
            optional_dependencies: new_optional_dependencies,
            peer_dependencies: pkg.peer_dependencies.clone(),
            peer_dependencies_meta: pkg.peer_dependencies_meta.clone(),
            dep_path: contextualized.clone(),
            local_source: pkg.local_source.clone(),
            os: pkg.os.clone(),
            cpu: pkg.cpu.clone(),
            libc: pkg.libc.clone(),
            bundled_dependencies: pkg.bundled_dependencies.clone(),
            tarball_url: pkg.tarball_url.clone(),
            alias_of: pkg.alias_of.clone(),
            yarn_checksum: pkg.yarn_checksum.clone(),
        },
    );
    Some(contextualized)
}

/// Outcome of [`pick_version`]. Distinguishes "nothing in the range
/// at all" from "the cutoff filtered every otherwise-satisfying
/// version" so the caller can surface a meaningful strict-mode error
/// instead of pretending the range itself was wrong.
#[derive(Debug)]
pub(crate) enum PickResult<'a> {
    Found(&'a aube_registry::VersionMetadata),
    NoMatch,
    /// Strict mode (or any caller treating the cutoff as a hard wall):
    /// at least one version satisfied the range, but all of them were
    /// filtered out by the cutoff.
    AgeGated,
}

#[cfg(test)]
impl<'a> PickResult<'a> {
    fn unwrap(self) -> &'a aube_registry::VersionMetadata {
        match self {
            PickResult::Found(m) => m,
            other => panic!("expected PickResult::Found, got {other:?}"),
        }
    }
}

/// Pick the best version from a packument that satisfies the given range.
///
/// `pick_lowest` flips the scan order — used by
/// `resolution-mode=time-based` for direct deps. `cutoff` filters out
/// versions whose registry publish time is later than the cutoff
/// (lexicographic compare on ISO-8601 UTC strings, which sort
/// correctly). When the packument has no `time` entry for a version
/// (e.g. abbreviated corgi payload in `Highest` mode), the cutoff is
/// ignored and the version stays eligible.
///
/// `strict` controls fallback when the cutoff filters out every
/// satisfying version: with `strict=true` we return `None` and the
/// caller errors out; with `strict=false` (the pnpm default) we make a
/// second pass that picks the *lowest* satisfying version ignoring the
/// cutoff. The lowest-satisfying fallback is pnpm's deliberate choice
/// — the oldest version in the range is least likely to be the freshly
/// pushed compromise that triggered the filter in the first place.
fn pick_version<'a>(
    packument: &'a Packument,
    range_str: &str,
    locked: Option<&str>,
    pick_lowest: bool,
    cutoff: Option<&str>,
    strict: bool,
) -> PickResult<'a> {
    // Handle dist-tag references
    let effective_range = if let Some(tagged_version) = packument.dist_tags.get(range_str) {
        tagged_version.clone()
    } else {
        range_str.to_string()
    };

    let range = match node_semver::Range::parse(&effective_range) {
        Ok(r) => r,
        Err(_) => return PickResult::NoMatch,
    };

    let passes_cutoff = |ver: &str| -> bool {
        let Some(c) = cutoff else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            // Missing time: keep it — we'd rather risk a slightly newer
            // transitive than fail to resolve the range entirely.
            None => true,
        }
    };

    // Prefer locked version if it satisfies and clears the cutoff.
    if let Some(locked_ver) = locked
        && let Ok(v) = node_semver::Version::parse(locked_ver)
        && v.satisfies(&range)
        && passes_cutoff(locked_ver)
        && let Some(meta) = packument.versions.get(locked_ver)
    {
        return PickResult::Found(meta);
    }

    // Track whether *any* version satisfied the range — if so but
    // every one was rejected by the cutoff, the failure is age-gate
    // related, not a real "no match in range".
    let mut had_satisfying_but_age_gated = false;

    let mut versions: Vec<(&String, &aube_registry::VersionMetadata)> =
        packument.versions.iter().collect();
    versions.sort_by(|(a, _), (b, _)| {
        let va = node_semver::Version::parse(a);
        let vb = node_semver::Version::parse(b);
        match (va, vb) {
            (Ok(va), Ok(vb)) => {
                if pick_lowest {
                    va.cmp(&vb)
                } else {
                    vb.cmp(&va)
                }
            }
            _ => std::cmp::Ordering::Equal,
        }
    });

    for (ver_str, meta) in &versions {
        if let Ok(v) = node_semver::Version::parse(ver_str)
            && v.satisfies(&range)
        {
            if passes_cutoff(ver_str) {
                return PickResult::Found(meta);
            }
            had_satisfying_but_age_gated = true;
        }
    }

    // Strict mode (or no cutoff active): give up. Distinguish age-gate
    // failures so the caller can surface a meaningful error instead of
    // pretending the range itself was wrong.
    if strict || cutoff.is_none() {
        return if had_satisfying_but_age_gated {
            PickResult::AgeGated
        } else {
            PickResult::NoMatch
        };
    }

    // Lenient fallback: pnpm's `pickPackageFromMetaUsingTime` ignores
    // the cutoff and picks the *lowest* satisfying version. We have to
    // re-sort because the primary scan above may have been
    // highest-first.
    let mut ascending: Vec<(&String, &aube_registry::VersionMetadata)> =
        packument.versions.iter().collect();
    ascending.sort_by(|(a, _), (b, _)| {
        let va = node_semver::Version::parse(a);
        let vb = node_semver::Version::parse(b);
        match (va, vb) {
            (Ok(va), Ok(vb)) => va.cmp(&vb),
            _ => std::cmp::Ordering::Equal,
        }
    });
    for (ver_str, meta) in ascending {
        if let Ok(v) = node_semver::Version::parse(ver_str)
            && v.satisfies(&range)
        {
            return PickResult::Found(meta);
        }
    }
    PickResult::NoMatch
}

/// Lexical path normalization — collapse `.` and `..` components
/// against earlier components without touching the filesystem. Unlike
/// `canonicalize`, this doesn't require the path to exist and doesn't
/// follow symlinks, which matters because `link:` deps deliberately
/// point at symlinks the user controls. Leading `..` that can't be
/// collapsed are preserved (e.g. `../foo` stays `../foo`).
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                // Pop the previous component if it was a plain name;
                // otherwise record the `..` literally so leading
                // ascents out of the base don't silently disappear.
                let prev_is_normal = out
                    .components()
                    .next_back()
                    .is_some_and(|c| matches!(c, Component::Normal(_)));
                if prev_is_normal {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Rewrite a `LocalSource` whose path is relative to `importer_root`
/// into one whose path is relative to `project_root`, so downstream
/// code (install.rs, linker) can resolve the target with a single
/// `project_root.join(rel)` regardless of which workspace importer
/// declared it.
///
/// Both the join-then-diff intermediate and the returned path are
/// lexically normalized — `Path::join` and `pathdiff::diff_paths`
/// leave `..` components in place, which means `packages/app` +
/// `../../vendor-dir` would otherwise produce
/// `packages/app/../../vendor-dir`. That non-canonical form fed into
/// `dep_path`'s hash would produce a different key for every
/// importer declaring the same target, and would also leak into the
/// lockfile's `version:` string.
fn rebase_local(
    local: &LocalSource,
    importer_root: &std::path::Path,
    project_root: &std::path::Path,
) -> LocalSource {
    // The fast path: importer_root == project_root. Root-importer
    // installs take this branch, which is also the single-project
    // case — no rewrite needed and we preserve the raw specifier
    // bytes for a byte-identical lockfile round-trip.
    if importer_root == project_root {
        return local.clone();
    }
    let Some(local_path) = local.path() else {
        // Non-path sources (git) have nothing to rebase.
        return local.clone();
    };
    let abs = normalize_path(&importer_root.join(local_path));
    let rebased = pathdiff::diff_paths(&abs, project_root).map_or(abs, |p| normalize_path(&p));
    match local {
        LocalSource::Directory(_) => LocalSource::Directory(rebased),
        LocalSource::Tarball(_) => LocalSource::Tarball(rebased),
        LocalSource::Link(_) => LocalSource::Link(rebased),
        LocalSource::Git(_) | LocalSource::RemoteTarball(_) => local.clone(),
    }
}

#[cfg(test)]
mod rebase_local_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn workspace_file_climbs_out_of_importer_to_root_sibling() {
        // packages/app importer declares `file:../../vendor-dir`.
        // Expected result: `vendor-dir` (workspace-root relative),
        // collapsed down from the intermediate
        // `packages/app/../../vendor-dir` form.
        let local = LocalSource::Directory(PathBuf::from("../../vendor-dir"));
        let rebased = rebase_local(&local, Path::new("packages/app"), Path::new(""));
        match rebased {
            LocalSource::Directory(p) => assert_eq!(p, PathBuf::from("vendor-dir")),
            other => panic!("expected Directory, got {other:?}"),
        }
    }

    #[test]
    fn two_importers_referencing_same_target_collide_on_dep_path() {
        // Both importers end up pointing at the same on-disk path —
        // the encoded dep_path must match so they de-dupe in the
        // lockfile.
        let a = rebase_local(
            &LocalSource::Directory(PathBuf::from("../../vendor-dir")),
            Path::new("packages/app"),
            Path::new(""),
        );
        let b = rebase_local(
            &LocalSource::Directory(PathBuf::from("../vendor-dir")),
            Path::new("packages"),
            Path::new(""),
        );
        assert_eq!(a.dep_path("vendor-dir"), b.dep_path("vendor-dir"));
    }

    #[test]
    fn normalize_preserves_unresolvable_leading_parent() {
        // `..` at the root of the project is still meaningful —
        // don't silently drop it.
        assert_eq!(
            normalize_path(Path::new("../vendor")),
            PathBuf::from("../vendor")
        );
    }

    #[test]
    fn dep_path_and_specifier_use_posix_separators() {
        // Backslash-separated input (as Windows would store) must
        // hash and render the same as a forward-slash equivalent so
        // a checked-in lockfile resolves identically on either OS.
        let win = LocalSource::Directory(PathBuf::from("vendor\\nested\\dir"));
        let unix = LocalSource::Directory(PathBuf::from("vendor/nested/dir"));
        assert_eq!(win.dep_path("foo"), unix.dep_path("foo"));
        assert_eq!(win.specifier(), "file:vendor/nested/dir");
        assert_eq!(unix.specifier(), "file:vendor/nested/dir");
    }
}

/// Walk a gzipped npm tarball once and return the raw bytes of its
/// top-level `package.json` entry. The wrapper directory name varies
/// (`package/`, but also e.g. GitHub's `owner-repo-<sha>/`), so we
/// match on the entry's basename plus a 2-component depth check
/// rather than a hardcoded prefix. Errors come back as plain
/// `String`s so each caller can wrap them with its own package
/// identity in whatever error type it prefers — used by both the
/// `file:` tarball path (`read_local_manifest`) and the remote
/// tarball resolver (`resolve_remote_tarball`).
fn read_tarball_package_json(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(gz);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let entry_path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        if entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "package.json")
            && entry_path.components().count() == 2
        {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            return Ok(buf);
        }
    }
    Err("tarball has no top-level package.json".to_string())
}

/// Read the `package.json` of a `file:` / `link:` target to discover
/// the real package name, version, and production dependencies.
///
/// For `LocalSource::Directory` and `LocalSource::Link` we read the
/// target dir's `package.json` directly. For `LocalSource::Tarball` we
/// open the `.tgz`, find the first `*/package.json` entry, and parse
/// its contents without extracting the rest of the archive.
fn read_local_manifest(
    local: &LocalSource,
    importer_root: &std::path::Path,
) -> Result<(String, String, BTreeMap<String, String>), Error> {
    let Some(local_path) = local.path() else {
        return Err(Error::Registry(
            local.specifier(),
            "read_local_manifest called on non-path source".to_string(),
        ));
    };
    let path = importer_root.join(local_path);

    let content = match local {
        LocalSource::Directory(_) | LocalSource::Link(_) => {
            std::fs::read(path.join("package.json"))
                .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?
        }
        LocalSource::Tarball(_) => {
            let bytes = std::fs::read(&path)
                .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?;
            read_tarball_package_json(&bytes).map_err(|e| Error::Registry(local.specifier(), e))?
        }
        LocalSource::Git(_) | LocalSource::RemoteTarball(_) => {
            return Err(Error::Registry(
                local.specifier(),
                "read_local_manifest: remote source handled separately".to_string(),
            ));
        }
    };

    let pj: aube_manifest::PackageJson = serde_json::from_slice(&content)
        .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?;
    Ok((
        pj.name.unwrap_or_default(),
        pj.version.unwrap_or_else(|| "0.0.0".to_string()),
        pj.dependencies,
    ))
}

fn dep_path_for(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// Match specifier prefixes that resolve to a non-registry source
/// (`file:`, `link:`, or a git URL form). Used by the resolver to
/// decide whether to dispatch the local/git branch instead of the
/// normal version-range lookup.
fn is_non_registry_specifier(s: &str) -> bool {
    if s.starts_with("link:") {
        return true;
    }
    // Remote tarball URL (`https://host/path/pkg.tgz`). Checked
    // before the git-spec match so a bare `https://` URL ending in
    // `.tgz` dispatches the tarball branch rather than falling
    // through to git.
    if aube_lockfile::LocalSource::looks_like_remote_tarball_url(s) {
        return true;
    }
    if aube_lockfile::parse_git_spec(s).is_some() {
        return true;
    }
    // `file:` is a local-path prefix only when it *isn't* also a git
    // URL form — parse_git_spec already matched `file://…/repo.git`
    // above, so anything that reaches here is treated as a path.
    s.starts_with("file:")
}

/// Turn a raw `GitSource` (committish parsed from the user's
/// specifier, empty `resolved`) into a fully-resolved one by running
/// `git ls-remote`, then shallow-cloning to read the package's own
/// `package.json` for version + transitive deps. The clone lives in
/// a commit-keyed temp directory; install-time materialization will
/// either reuse the same directory or re-run the shallow clone.
async fn resolve_git_source(
    name: &str,
    git: &aube_lockfile::GitSource,
    shallow: bool,
) -> Result<(LocalSource, String, BTreeMap<String, String>), Error> {
    // `git ls-remote` and the shallow clone both shell out and do
    // network I/O that can easily take multiple seconds. Running
    // them inline on the tokio worker thread would block any
    // concurrently-scheduled async work (registry HTTP calls,
    // other resolve tasks). Hand the whole sync sequence — which
    // has no borrows on the resolver's state — off to a blocking
    // thread via `spawn_blocking`.
    let url = git.url.clone();
    let committish = git.committish.clone();
    let name_owned = name.to_string();
    let (local, version, deps) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
        let resolved = aube_store::git_resolve_ref(&url, committish.as_deref())
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let clone_dir = aube_store::git_shallow_clone(&url, &resolved, shallow)
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let manifest_bytes = std::fs::read(clone_dir.join("package.json")).map_err(|e| {
            Error::Registry(
                name_owned.clone(),
                format!("read package.json in clone: {e}"),
            )
        })?;
        let pj: aube_manifest::PackageJson = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let version = pj.version.unwrap_or_else(|| "0.0.0".to_string());
        let deps = pj.dependencies;
        Ok((
            LocalSource::Git(aube_lockfile::GitSource {
                url,
                committish,
                resolved,
            }),
            version,
            deps,
        ))
    })
    .await
    .map_err(|e| Error::Registry(name.to_string(), format!("git task panicked: {e}")))??;
    Ok((local, version, deps))
}

/// Fetch a remote tarball URL, compute its sha512 integrity, and read
/// the enclosed `package.json` for version + transitive deps. Returns
/// a fully-populated `LocalSource::RemoteTarball` alongside the
/// manifest tuple the resolver's local-dep branch expects.
async fn resolve_remote_tarball(
    name: &str,
    tarball: &aube_lockfile::RemoteTarballSource,
    client: &RegistryClient,
) -> Result<(LocalSource, String, BTreeMap<String, String>), Error> {
    let bytes = client
        .fetch_tarball_bytes(&tarball.url)
        .await
        .map_err(|e| Error::Registry(name.to_string(), format!("fetch {}: {e}", tarball.url)))?;
    let name_owned = name.to_string();
    let url = tarball.url.clone();
    let (integrity, version, deps) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
        use sha2::{Digest, Sha512};
        let mut hasher = Sha512::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
        let integrity = format!("sha512-{b64}");

        // Walk the tarball once to pull out the top-level
        // `package.json` (wrapper name varies, so the helper looks
        // at the first path component's basename, not a hardcoded
        // `package/package.json`).
        let manifest_bytes = read_tarball_package_json(&bytes)
            .map_err(|e| Error::Registry(name_owned.clone(), format!("tarball {url}: {e}")))?;
        let pj: aube_manifest::PackageJson = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let version = pj.version.unwrap_or_else(|| "0.0.0".to_string());
        Ok((integrity, version, pj.dependencies))
    })
    .await
    .map_err(|e| Error::Registry(name.to_string(), format!("tarball task panicked: {e}")))??;
    Ok((
        LocalSource::RemoteTarball(aube_lockfile::RemoteTarballSource {
            url: tarball.url.clone(),
            integrity,
        }),
        version,
        deps,
    ))
}

/// Find the best-matching override rule for a task and return its
/// replacement spec (cloned). "Best" means most specific: we score
/// each matching rule by `non_wildcard_parents * 2 +
/// (target_version_req ? 1 : 0)` and take the max, so `a>b>c` beats
/// `b>c` beats `c`, and a version-qualified `c@<2` beats a bare `c`.
/// Wildcard `**` parent segments don't inflate the score — `**/foo`
/// is semantically equivalent to a bare `foo` and shouldn't
/// out-rank a more specific `foo@<2`. Ties break on rule insertion
/// order (stable `iter()` over a `Vec`), which reflects the
/// manifest's BTreeMap ordering after pnpm/yarn precedence merging.
fn pick_override_spec(
    rules: &[override_rule::OverrideRule],
    task_name: &str,
    task_range: &str,
    ancestors: &[(String, String)],
) -> Option<String> {
    let frames: Vec<override_rule::AncestorFrame<'_>> = ancestors
        .iter()
        .map(|(n, v)| override_rule::AncestorFrame {
            name: n,
            version: v,
        })
        .collect();
    rules
        .iter()
        .filter(|r| override_rule::matches(r, task_name, task_range, &frames))
        .max_by_key(|r| {
            let named_parents = r.parents.iter().filter(|p| !p.is_wildcard()).count();
            named_parents * 2 + usize::from(r.target.version_req.is_some())
        })
        .map(|r| r.replacement.clone())
}

fn version_satisfies(version: &str, range_str: &str) -> bool {
    let Ok(v) = node_semver::Version::parse(version) else {
        return false;
    };
    let Ok(r) = node_semver::Range::parse(range_str) else {
        return false;
    };
    v.satisfies(&r)
}

fn apply_package_extensions(
    pkg: &mut aube_registry::VersionMetadata,
    extensions: &[PackageExtension],
) {
    for extension in extensions {
        if !package_selector_matches(&extension.selector, &pkg.name, &pkg.version) {
            continue;
        }
        extend_missing(&mut pkg.dependencies, &extension.dependencies);
        extend_missing(
            &mut pkg.optional_dependencies,
            &extension.optional_dependencies,
        );
        extend_missing(&mut pkg.peer_dependencies, &extension.peer_dependencies);
        extend_missing(
            &mut pkg.peer_dependencies_meta,
            &extension.peer_dependencies_meta,
        );
    }
}

fn extend_missing<K, V>(target: &mut BTreeMap<K, V>, additions: &BTreeMap<K, V>)
where
    K: Ord + Clone,
    V: Clone,
{
    for (key, value) in additions {
        target.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn package_selector_matches(selector: &str, name: &str, version: &str) -> bool {
    let selector = selector.trim();
    if selector == name {
        return true;
    }
    let Some((selector_name, range)) = split_package_selector(selector) else {
        return false;
    };
    selector_name == name && version_satisfies(version, range)
}

fn split_package_selector(selector: &str) -> Option<(&str, &str)> {
    let at = selector.rfind('@')?;
    if at == 0 {
        return None;
    }
    if selector.starts_with('@') {
        let slash = selector.find('/')?;
        if at <= slash {
            return None;
        }
    }
    let (name, range) = selector.split_at(at);
    let range = &range[1..];
    (!name.is_empty() && !range.is_empty()).then_some((name, range))
}

#[cfg(test)]
fn is_deprecation_allowed(name: &str, version: &str, allowed: &BTreeMap<String, String>) -> bool {
    allowed
        .get(name)
        .is_some_and(|range| version_satisfies(version, range))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no version of {0} matches range {1}")]
    NoMatch(String, String),
    #[error(
        "no version of {name} matching {range} is older than {minutes} minute(s) (minimumReleaseAgeStrict=true)"
    )]
    AgeGate {
        name: String,
        range: String,
        minutes: u64,
    },
    #[error("registry error for {0}: {1}")]
    Registry(String, String),
    #[error(
        "{name}: catalog reference `{spec}` does not resolve — catalog `{catalog}` is not defined (add it to `catalog:` / `catalogs.{catalog}:` in pnpm-workspace.yaml, or under `workspaces.catalog` / `pnpm.catalog` in package.json)"
    )]
    UnknownCatalog {
        name: String,
        spec: String,
        catalog: String,
    },
    #[error(
        "{name}: catalog reference `{spec}` does not resolve — catalog `{catalog}` has no entry for `{name}`"
    )]
    UnknownCatalogEntry {
        name: String,
        spec: String,
        catalog: String,
    },
    #[error(
        "blocked exotic transitive dependency {name}@{spec} from {parent} (blockExoticSubdeps=true)"
    )]
    BlockedExoticSubdep {
        name: String,
        spec: String,
        parent: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_registry::{Dist, Packument, VersionMetadata};

    #[test]
    fn test_version_satisfies() {
        assert!(version_satisfies("4.17.21", "^4.17.0"));
        assert!(version_satisfies("4.17.21", "^4.0.0"));
        assert!(!version_satisfies("3.10.0", "^4.0.0"));
        assert!(version_satisfies("1.0.0", ">=1.0.0"));
        assert!(version_satisfies("2.0.0", ">=1.0.0 <3.0.0"));
    }

    #[test]
    fn test_version_satisfies_exact() {
        assert!(version_satisfies("1.0.0", "1.0.0"));
        assert!(!version_satisfies("1.0.1", "1.0.0"));
    }

    #[test]
    fn test_version_satisfies_tilde() {
        assert!(version_satisfies("1.2.3", "~1.2.0"));
        assert!(version_satisfies("1.2.9", "~1.2.0"));
        assert!(!version_satisfies("1.3.0", "~1.2.0"));
    }

    #[test]
    fn test_version_satisfies_star() {
        assert!(version_satisfies("1.0.0", "*"));
        assert!(version_satisfies("99.99.99", "*"));
    }

    #[test]
    fn test_version_satisfies_invalid() {
        assert!(!version_satisfies("notaversion", "^1.0.0"));
        assert!(!version_satisfies("1.0.0", "notarange"));
    }

    #[test]
    fn dependency_policy_default_blocks_exotic_subdeps() {
        assert!(DependencyPolicy::default().block_exotic_subdeps);
    }

    #[test]
    fn package_extension_selector_matches_scoped_and_versioned_names() {
        assert!(package_selector_matches(
            "@scope/pkg@^1",
            "@scope/pkg",
            "1.2.3"
        ));
        assert!(package_selector_matches("plain", "plain", "9.0.0"));
        assert!(!package_selector_matches(
            "@scope/pkg@^2",
            "@scope/pkg",
            "1.2.3"
        ));
    }

    #[test]
    fn package_extensions_merge_dependency_maps() {
        let mut pkg = make_version("host", "1.0.0");
        let extension = PackageExtension {
            selector: "host@1".to_string(),
            dependencies: [("missing".to_string(), "^2.0.0".to_string())]
                .into_iter()
                .collect(),
            optional_dependencies: BTreeMap::new(),
            peer_dependencies: [("peer".to_string(), "^3.0.0".to_string())]
                .into_iter()
                .collect(),
            peer_dependencies_meta: [(
                "peer".to_string(),
                aube_registry::PeerDepMeta { optional: true },
            )]
            .into_iter()
            .collect(),
        };

        apply_package_extensions(&mut pkg, &[extension]);

        assert_eq!(pkg.dependencies.get("missing").unwrap(), "^2.0.0");
        assert_eq!(pkg.peer_dependencies.get("peer").unwrap(), "^3.0.0");
        assert!(pkg.peer_dependencies_meta.get("peer").unwrap().optional);
    }

    #[test]
    fn package_extensions_do_not_overwrite_existing_dependency_maps() {
        let mut pkg = make_version("host", "1.0.0");
        pkg.dependencies
            .insert("dep".to_string(), "^1.0.0".to_string());
        pkg.optional_dependencies
            .insert("optional".to_string(), "^2.0.0".to_string());
        pkg.peer_dependencies
            .insert("peer".to_string(), "^3.0.0".to_string());
        pkg.peer_dependencies_meta.insert(
            "peer".to_string(),
            aube_registry::PeerDepMeta { optional: false },
        );

        let extension = PackageExtension {
            selector: "host".to_string(),
            dependencies: [
                ("dep".to_string(), "^9.0.0".to_string()),
                ("missing".to_string(), "^4.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            optional_dependencies: [
                ("optional".to_string(), "^9.0.0".to_string()),
                ("missing-optional".to_string(), "^5.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            peer_dependencies: [
                ("peer".to_string(), "^9.0.0".to_string()),
                ("missing-peer".to_string(), "^6.0.0".to_string()),
            ]
            .into_iter()
            .collect(),
            peer_dependencies_meta: [
                (
                    "peer".to_string(),
                    aube_registry::PeerDepMeta { optional: true },
                ),
                (
                    "missing-peer".to_string(),
                    aube_registry::PeerDepMeta { optional: true },
                ),
            ]
            .into_iter()
            .collect(),
        };

        apply_package_extensions(&mut pkg, &[extension]);

        assert_eq!(pkg.dependencies.get("dep").unwrap(), "^1.0.0");
        assert_eq!(pkg.dependencies.get("missing").unwrap(), "^4.0.0");
        assert_eq!(pkg.optional_dependencies.get("optional").unwrap(), "^2.0.0");
        assert_eq!(
            pkg.optional_dependencies.get("missing-optional").unwrap(),
            "^5.0.0"
        );
        assert_eq!(pkg.peer_dependencies.get("peer").unwrap(), "^3.0.0");
        assert_eq!(pkg.peer_dependencies.get("missing-peer").unwrap(), "^6.0.0");
        assert!(!pkg.peer_dependencies_meta.get("peer").unwrap().optional);
        assert!(
            pkg.peer_dependencies_meta
                .get("missing-peer")
                .unwrap()
                .optional
        );
    }

    #[test]
    fn allowed_deprecated_versions_match_package_ranges() {
        let allowed = [("old".to_string(), "<2".to_string())]
            .into_iter()
            .collect();

        assert!(is_deprecation_allowed("old", "1.9.0", &allowed));
        assert!(!is_deprecation_allowed("old", "2.0.0", &allowed));
        assert!(!is_deprecation_allowed("other", "1.0.0", &allowed));
    }

    #[test]
    fn test_dep_path_for() {
        assert_eq!(dep_path_for("lodash", "4.17.21"), "lodash@4.17.21");
        assert_eq!(dep_path_for("@babel/core", "7.24.0"), "@babel/core@7.24.0");
    }

    fn make_version(name: &str, version: &str) -> VersionMetadata {
        VersionMetadata {
            name: name.to_string(),
            version: version.to_string(),
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            peer_dependencies: BTreeMap::new(),
            peer_dependencies_meta: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
            bundled_dependencies: None,
            dist: Some(Dist {
                tarball: format!("https://registry.npmjs.org/{name}/-/{name}-{version}.tgz"),
                integrity: Some(format!("sha512-fake-{name}-{version}")),
                shasum: None,
            }),
            os: vec![],
            cpu: vec![],
            libc: vec![],
            has_install_script: false,
            deprecated: None,
        }
    }

    fn make_packument(name: &str, versions: &[&str], latest: &str) -> Packument {
        let mut ver_map = BTreeMap::new();
        for v in versions {
            ver_map.insert(v.to_string(), make_version(name, v));
        }
        let mut dist_tags = BTreeMap::new();
        dist_tags.insert("latest".to_string(), latest.to_string());
        Packument {
            name: name.to_string(),
            versions: ver_map,
            dist_tags,
            time: BTreeMap::new(),
        }
    }

    #[test]
    fn test_pick_version_highest_match() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0", "2.0.0"], "2.0.0");
        let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
        assert_eq!(result.version, "1.2.0");
    }

    #[test]
    fn test_pick_version_exact() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        let result = pick_version(&packument, "1.0.0", None, false, None, false).unwrap();
        assert_eq!(result.version, "1.0.0");
    }

    #[test]
    fn test_pick_version_no_match() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        let result = pick_version(&packument, "^2.0.0", None, false, None, false);
        assert!(matches!(result, PickResult::NoMatch));
    }

    #[test]
    fn test_pick_version_strict_distinguishes_age_gate_from_no_match() {
        // A version satisfies the range but is filtered by the cutoff.
        // Strict mode should report `AgeGated`, not `NoMatch`, so the
        // caller can surface a meaningful error message.
        let mut packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        packument
            .time
            .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
        let cutoff = "2020-01-01T00:00:00.000Z";
        let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), true);
        assert!(matches!(result, PickResult::AgeGated));

        // No version satisfies the range at all → still NoMatch even
        // in strict mode.
        let result = pick_version(&packument, "^9.0.0", None, false, Some(cutoff), true);
        assert!(matches!(result, PickResult::NoMatch));
    }

    #[test]
    fn test_pick_version_prefers_locked() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
        let result = pick_version(&packument, "^1.0.0", Some("1.1.0"), false, None, false).unwrap();
        assert_eq!(result.version, "1.1.0");
    }

    #[test]
    fn test_pick_version_locked_out_of_range() {
        let packument = make_packument("foo", &["1.0.0", "2.0.0"], "2.0.0");
        // Locked version doesn't satisfy range, should pick highest match
        let result = pick_version(&packument, "^2.0.0", Some("1.0.0"), false, None, false).unwrap();
        assert_eq!(result.version, "2.0.0");
    }

    #[test]
    fn test_pick_version_dist_tag() {
        let packument = make_packument("foo", &["1.0.0", "2.0.0-beta.1"], "1.0.0");
        let result = pick_version(&packument, "latest", None, false, None, false).unwrap();
        assert_eq!(result.version, "1.0.0");
    }

    #[test]
    fn test_pick_version_lowest_picks_smallest_satisfying() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0", "2.0.0"], "2.0.0");
        let result = pick_version(&packument, "^1.0.0", None, true, None, false).unwrap();
        assert_eq!(result.version, "1.0.0");
    }

    #[test]
    fn test_pick_version_cutoff_filters_future_versions() {
        let mut packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
        packument
            .time
            .insert("1.0.0".into(), "2020-01-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.1.0".into(), "2021-01-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.2.0".into(), "2023-01-01T00:00:00.000Z".into());
        // Highest pick, but cutoff forbids 1.2.0 → fall back to 1.1.0.
        let cutoff = "2022-06-01T00:00:00.000Z";
        let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
        assert_eq!(result.version, "1.1.0");
    }

    #[test]
    fn test_pick_version_lenient_falls_back_to_lowest_when_cutoff_excludes_all() {
        // Mirrors pnpm's lenient `pickPackageFromMetaUsingTime`: when
        // every satisfying version is younger than the cutoff, fall
        // back to the lowest satisfying version (ignoring the cutoff).
        let mut packument = make_packument("foo", &["1.0.0", "1.1.0", "1.2.0"], "1.2.0");
        packument
            .time
            .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.2.0".into(), "2025-01-01T00:00:00.000Z".into());
        let cutoff = "2020-01-01T00:00:00.000Z";
        let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
        assert_eq!(result.version, "1.0.0");
    }

    #[test]
    fn test_pick_version_strict_returns_age_gated_when_cutoff_excludes_all() {
        let mut packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        packument
            .time
            .insert("1.0.0".into(), "2024-01-01T00:00:00.000Z".into());
        packument
            .time
            .insert("1.1.0".into(), "2024-06-01T00:00:00.000Z".into());
        let cutoff = "2020-01-01T00:00:00.000Z";
        let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), true);
        assert!(matches!(result, PickResult::AgeGated));
    }

    #[test]
    fn test_minimum_release_age_cutoff_format() {
        let mra = MinimumReleaseAge {
            minutes: 60,
            ..Default::default()
        };
        let cutoff = mra.cutoff().expect("non-zero minutes produces a cutoff");
        // Sanity-check the shape; the actual instant depends on now().
        assert_eq!(cutoff.len(), 24, "ISO-8601 with millis is 24 chars");
        assert!(cutoff.ends_with("Z"));
        assert_eq!(&cutoff[4..5], "-");
        assert_eq!(&cutoff[10..11], "T");
    }

    #[test]
    fn test_minimum_release_age_zero_disables() {
        let mra = MinimumReleaseAge::default();
        assert!(mra.cutoff().is_none());
    }

    #[test]
    fn test_format_iso8601_known_epoch() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(
            format_iso8601_utc(1_704_067_200),
            "2024-01-01T00:00:00.000Z"
        );
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn test_pick_version_cutoff_allows_missing_time_entries() {
        let packument = make_packument("foo", &["1.0.0", "1.1.0"], "1.1.0");
        // Packument has no `time` entries at all — cutoff must not
        // remove every candidate, or the resolver can never make
        // progress on abbreviated-packument registries.
        let cutoff = "2000-01-01T00:00:00.000Z";
        let result = pick_version(&packument, "^1.0.0", None, false, Some(cutoff), false).unwrap();
        assert_eq!(result.version, "1.1.0");
    }

    #[test]
    fn test_pick_version_with_deps() {
        let mut packument = make_packument("foo", &["1.0.0"], "1.0.0");
        packument
            .versions
            .get_mut("1.0.0")
            .unwrap()
            .dependencies
            .insert("bar".to_string(), "^2.0.0".to_string());

        let result = pick_version(&packument, "^1.0.0", None, false, None, false).unwrap();
        assert_eq!(result.dependencies.get("bar").unwrap(), "^2.0.0");
    }

    fn mk_locked(
        name: &str,
        version: &str,
        deps: &[(&str, &str)],
        peer_deps: &[(&str, &str)],
    ) -> LockedPackage {
        let mut dependencies = BTreeMap::new();
        for (n, v) in deps {
            dependencies.insert((*n).to_string(), (*v).to_string());
        }
        let mut peer_dependencies = BTreeMap::new();
        for (n, r) in peer_deps {
            peer_dependencies.insert((*n).to_string(), (*r).to_string());
        }
        LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            integrity: None,
            dependencies,
            peer_dependencies,
            peer_dependencies_meta: BTreeMap::new(),
            dep_path: format!("{name}@{version}"),
            ..Default::default()
        }
    }

    fn graph_has_package(graph: &LockfileGraph, name: &str, version: &str) -> bool {
        graph
            .packages
            .values()
            .any(|pkg| pkg.name == name && pkg.version == version)
    }

    // Regression guard for the cycle-break branch in `visit_peer_context`
    // flagged by greptile on #40. Two packages peer-depend on each other:
    //
    //     a@1.0.0 -> dep=b@1.0.0, peer=b@^1
    //     b@1.0.0 -> dep=a@1.0.0, peer=a@^1
    //
    // Starting the DFS from importer root `a`, we should:
    //   1. Visit `a`, recurse into `b`
    //   2. Visit `b`, recurse into `a` (cycle hit — `visiting` guard fires)
    //   3. Cycle branch returns `a`'s contextualized dep_path WITHOUT
    //      waiting for the in-progress insertion to land
    //   4. `b` completes, gets inserted
    //   5. `a` completes, gets inserted
    //
    // By the time the function returns, every dep_path referenced from
    // any `dependencies` tail must exist as a key in `out_packages`.
    #[test]
    fn apply_peer_contexts_handles_mutual_peer_cycle() {
        let a = mk_locked("a", "1.0.0", &[("b", "1.0.0")], &[("b", "^1")]);
        let b = mk_locked("b", "1.0.0", &[("a", "1.0.0")], &[("a", "^1")]);

        let mut packages = BTreeMap::new();
        packages.insert("a@1.0.0".to_string(), a);
        packages.insert("b@1.0.0".to_string(), b);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "a".to_string(),
                dep_path: "a@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let canonical = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = apply_peer_contexts(canonical, &PeerContextOptions::default());

        // Both packages got contextualized dep_paths with each other's
        // resolved version baked in.
        let a_key = "a@1.0.0(b@1.0.0)";
        let b_key = "b@1.0.0(a@1.0.0)";
        assert!(
            out.packages.contains_key(a_key),
            "expected {a_key} in {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        assert!(
            out.packages.contains_key(b_key),
            "expected {b_key} in {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );

        // Every referenced dependency tail resolves to a real entry in
        // out_packages — the cycle-break branch didn't leak a dangling
        // reference.
        for pkg in out.packages.values() {
            for (child_name, child_tail) in &pkg.dependencies {
                let child_key = format!("{child_name}@{child_tail}");
                assert!(
                    out.packages.contains_key(&child_key),
                    "dangling dep_path {child_key} referenced from {}",
                    pkg.dep_path
                );
            }
        }

        // Importer's direct dep now points at the contextualized `a`.
        let root = out.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].dep_path, a_key);
    }

    // When a declared peer has its own resolved peers, the outer
    // package's suffix must carry the *nested* form — this is what
    // pnpm writes for React ecosystem projects where
    // `@testing-library/react` peers on both `react` and `react-dom`,
    // and `react-dom` itself peers on `react`. The expected snapshot
    // key is `@testing-library/react@14(react@18)(react-dom@18(react@18))`.
    //
    // This test uses a simplified three-package fixture
    // (consumer → adapter → core) where `core` is only a peer and
    // `adapter` peers on `core`. The `consumer` peers on both and
    // should serialize the `adapter` entry in its suffix with the
    // nested `(core@...)` tail.
    #[test]
    fn apply_peer_contexts_produces_nested_peer_suffixes() {
        // consumer declares peers [adapter, core]. adapter declares
        // peer [core]. core has no deps or peers.
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("adapter", "1.0.0"), ("core", "1.0.0")],
            &[("adapter", "^1"), ("core", "^1")],
        );
        consumer.dep_path = "consumer@1.0.0".to_string();

        let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
        adapter.dep_path = "adapter@1.0.0".to_string();

        let core = mk_locked("core", "1.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("adapter@1.0.0".to_string(), adapter);
        packages.insert("core@1.0.0".to_string(), core);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = apply_peer_contexts(graph, &PeerContextOptions::default());

        // adapter's standalone key should have just its own peer (core).
        assert!(
            out.packages.contains_key("adapter@1.0.0(core@1.0.0)"),
            "expected nested adapter variant: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );

        // consumer's key should reference adapter's NESTED tail, i.e.
        // `(adapter@1.0.0(core@1.0.0))(core@1.0.0)` — that's the pnpm
        // byte-identical shape.
        let consumer_key = "consumer@1.0.0(adapter@1.0.0(core@1.0.0))(core@1.0.0)";
        assert!(
            out.packages.contains_key(consumer_key),
            "expected nested consumer key {consumer_key} in {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );

        // Every referenced dependency tail must resolve to a real entry.
        for pkg in out.packages.values() {
            for (child_name, child_tail) in &pkg.dependencies {
                let child_key = format!("{child_name}@{child_tail}");
                assert!(
                    out.packages.contains_key(&child_key),
                    "dangling dep_path {child_key} referenced from {}",
                    pkg.dep_path
                );
            }
        }
    }

    // Per-peer-range cross-subtree satisfaction: two sibling packages
    // that declare peer react with INCOMPATIBLE ranges should each
    // end up pinned to the version satisfying their own range, even
    // if an ancestor scope carries the wrong version. This is pnpm's
    // "duplicate package per peer context" behavior.
    //
    // The fixture mirrors the real React/Testing-Library case: the
    // user pins `react@17` at the root (which is what the hoist
    // propagates into every child's ancestor scope), but a sibling
    // dep declares `peer react: ^18`. That sibling must resolve to
    // `react@18.x`, not `react@17`.
    #[test]
    fn apply_peer_contexts_per_range_satisfaction() {
        // consumer17 wants react@^17. consumer18 wants react@^18.
        // Both peer on react. The graph has BOTH versions in play
        // (the BFS resolver already emits both when the ranges
        // conflict — see `resolved_versions` dedupe logic).
        let mut consumer17 = mk_locked(
            "consumer17",
            "1.0.0",
            &[("react", "17.0.2")],
            &[("react", "^17")],
        );
        consumer17.dep_path = "consumer17@1.0.0".to_string();

        let mut consumer18 = mk_locked(
            "consumer18",
            "1.0.0",
            &[("react", "18.2.0")],
            &[("react", "^18")],
        );
        consumer18.dep_path = "consumer18@1.0.0".to_string();

        let react17 = mk_locked("react", "17.0.2", &[], &[]);
        let react18 = mk_locked("react", "18.2.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer17@1.0.0".to_string(), consumer17);
        packages.insert("consumer18@1.0.0".to_string(), consumer18);
        packages.insert("react@17.0.2".to_string(), react17);
        packages.insert("react@18.2.0".to_string(), react18);

        // Importer has BOTH consumers plus react@17 hoisted (the hoist
        // pass picks the first-encountered version, matching what
        // happens live when a user pins the older version).
        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "consumer17".to_string(),
                    dep_path: "consumer17@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                },
                DirectDep {
                    name: "consumer18".to_string(),
                    dep_path: "consumer18@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                },
                DirectDep {
                    name: "react".to_string(),
                    dep_path: "react@17.0.2".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^17".to_string()),
                },
            ],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = apply_peer_contexts(graph, &PeerContextOptions::default());

        // consumer17 should be suffixed with react@17 (satisfies ^17).
        assert!(
            out.packages.contains_key("consumer17@1.0.0(react@17.0.2)"),
            "consumer17 must pick react@17: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        // consumer18 must NOT reuse the root's react@17.0.2 — its own
        // declared range `^18` rejects it, so the peer-context pass
        // should fall back to the BFS-resolved react@18.2.0.
        assert!(
            out.packages.contains_key("consumer18@1.0.0(react@18.2.0)"),
            "consumer18 must fall back to react@18: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        // And specifically must NOT have been glued to react@17 just
        // because the ancestor scope happened to have it.
        assert!(
            !out.packages.contains_key("consumer18@1.0.0(react@17.0.2)"),
            "consumer18 was incorrectly pinned to react@17: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
    }

    // Regression for greptile feedback on #67: the `from_graph_scan`
    // fallback in `visit_peer_context` must return the full dep_path
    // TAIL, not just `p.version`. On Pass 2+ of the fixed-point loop
    // the input graph's keys carry peer suffixes — e.g. `react-dom`
    // lives at `react-dom@18.2.0(react@18.2.0)` — and downstream
    // lookups that reconstruct `format!("{name}@{tail}")` need the
    // tail to match the actual key. Returning `p.version` would give
    // `react-dom@18.2.0`, which Pass 2 lookups would miss, silently
    // dropping the peer from `new_dependencies`.
    //
    // The scenario: consumer peers on a package (helper) whose own
    // peer context already exists in the graph's suffixed form.
    // Neither ancestor scope nor the consumer's own `pkg.dependencies`
    // has helper (so the scan path is actually reached), forcing
    // `from_graph_scan` to be the resolution source. The resulting
    // `consumer` entry must reference the suffixed `helper` tail.
    #[test]
    fn from_graph_scan_returns_full_dep_path_tail() {
        // helper@1.0.0 has its own peer `core`. `consumer` peers on
        // helper but has no entry for it in its `pkg.dependencies`,
        // so the scan is the only resolution source.
        let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("helper", "^1")]);
        consumer.dep_path = "consumer@1.0.0".to_string();

        // `helper@1.0.0(core@1.0.0)` — already contextualized as it
        // would be after one iteration of the fixed-point loop.
        let mut helper = mk_locked("helper", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
        helper.dep_path = "helper@1.0.0(core@1.0.0)".to_string();

        let mut core = mk_locked("core", "1.0.0", &[], &[]);
        core.dep_path = "core@1.0.0".to_string();

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("helper@1.0.0(core@1.0.0)".to_string(), helper);
        packages.insert("core@1.0.0".to_string(), core);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = apply_peer_contexts(graph, &PeerContextOptions::default());

        // consumer's key must reference helper with its CONTEXTUALIZED
        // tail. Returning `p.version` would have produced
        // `consumer@1.0.0(helper@1.0.0)` and then silently dropped
        // `helper` from new_dependencies when the lookup missed.
        assert!(
            out.packages
                .contains_key("consumer@1.0.0(helper@1.0.0(core@1.0.0))"),
            "consumer must reference helper's contextualized tail: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );

        // And consumer.new_dependencies[helper] must be a non-dangling
        // reference into out_packages.
        let consumer_out = out
            .packages
            .get("consumer@1.0.0(helper@1.0.0(core@1.0.0))")
            .unwrap();
        let helper_tail = consumer_out
            .dependencies
            .get("helper")
            .expect("consumer must wire helper as a dep");
        assert_eq!(helper_tail, "1.0.0(core@1.0.0)");
        let helper_key = format!("helper@{helper_tail}");
        assert!(
            out.packages.contains_key(&helper_key),
            "consumer.dependencies[helper] must resolve to an existing package key"
        );
    }

    // `dedupe-peer-dependents=true` (the pnpm default) should collapse
    // two importer dependents that peer on the same name and resolve
    // to the same peer version into a single variant. Here two
    // consumers (consumer-a, consumer-b) both peer on react and both
    // end up with react@18.0.0 — the peer-context pass should emit a
    // single canonical consumer-a key and a single canonical
    // consumer-b key, but crucially when two *different ancestor
    // subtrees* pin the same peer version we still collapse to one
    // variant rather than keeping one per subtree.
    #[test]
    fn dedupe_peer_dependents_merges_equivalent_subtrees() {
        // Two sibling middle packages that each peer on react. The
        // importer has react@18.0.0 available, and the middle
        // packages' shared declared peer range (^18) would match.
        // Without dedupe-peer-dependents, the outer fixed-point loop
        // can emit duplicate variants for the same peer resolution
        // when the middle packages are reached via different sibling
        // paths. With the flag on, `dedupe_peer_variants` merges them.
        let mut consumer_a = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "18.0.0")],
            &[("react", "^18")],
        );
        consumer_a.dep_path = "consumer@1.0.0".to_string();
        let react = mk_locked("react", "18.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        // Seed two peer-suffixed keys manually to simulate mid-fixpoint
        // state where distinct subtrees produced the same peer
        // resolution. The dedupe pass should merge them.
        packages.insert(
            "consumer@1.0.0(react@18.0.0)".to_string(),
            LockedPackage {
                dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
                dependencies: {
                    let mut m = BTreeMap::new();
                    m.insert("react".to_string(), "18.0.0".to_string());
                    m
                },
                ..consumer_a.clone()
            },
        );
        // A second variant with identical peer resolution but a
        // different suffix encoding — simulating a stale subtree from
        // an earlier fixpoint iteration.
        let mut variant = consumer_a.clone();
        variant.dep_path = "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string();
        variant
            .dependencies
            .insert("react".to_string(), "18.0.0".to_string());
        packages.insert(
            "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
            variant,
        );
        packages.insert("react@18.0.0".to_string(), react);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = dedupe_peer_variants(graph);

        // Only one consumer variant should survive — the
        // lexicographically smallest key.
        let consumer_keys: Vec<_> = out
            .packages
            .keys()
            .filter(|k| k.starts_with("consumer@"))
            .collect();
        assert_eq!(
            consumer_keys.len(),
            1,
            "expected single canonical consumer variant after dedupe, got: {:?}",
            consumer_keys
        );
        assert_eq!(
            consumer_keys[0], "consumer@1.0.0(react@18.0.0)",
            "canonical should be lex-smallest key"
        );

        // Importer reference was rewritten to the canonical dep_path.
        let root = out.importers.get(".").unwrap();
        assert_eq!(root[0].dep_path, "consumer@1.0.0(react@18.0.0)");
    }

    // `dedupe-peer-dependents=false` should preserve every distinct
    // peer-suffixed variant, even when they would merge under the
    // default `true` setting. `apply_peer_contexts` is the only call
    // gated by the flag, so the meaningful assertion is that calling
    // `dedupe_peer_variants` explicitly merges the two variants, and
    // skipping the call (the flag-off codepath) leaves both intact.
    #[test]
    fn dedupe_peer_dependents_disabled_keeps_variants() {
        let consumer_a = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "18.0.0")],
            &[("react", "^18")],
        );
        let react = mk_locked("react", "18.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert(
            "consumer@1.0.0(react@18.0.0)".to_string(),
            LockedPackage {
                dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
                dependencies: {
                    let mut m = BTreeMap::new();
                    m.insert("react".to_string(), "18.0.0".to_string());
                    m
                },
                ..consumer_a.clone()
            },
        );
        let mut variant = consumer_a.clone();
        variant.dep_path = "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string();
        variant
            .dependencies
            .insert("react".to_string(), "18.0.0".to_string());
        packages.insert(
            "consumer@1.0.0(react@18.0.0)(react@18.0.0)".to_string(),
            variant,
        );
        packages.insert("react@18.0.0".to_string(), react);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0(react@18.0.0)".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        // Flag-off codepath: dedupe_peer_variants is never called,
        // so both variants survive untouched.
        let consumer_keys_off: Vec<_> = graph
            .packages
            .keys()
            .filter(|k| k.starts_with("consumer@"))
            .cloned()
            .collect();
        assert_eq!(
            consumer_keys_off.len(),
            2,
            "expected both variants to survive with dedupe_peer_dependents=false, got: {:?}",
            consumer_keys_off
        );

        // Flag-on codepath (for comparison): dedupe_peer_variants
        // collapses the two peer-equivalent variants into one.
        let merged = dedupe_peer_variants(graph);
        let consumer_keys_on: Vec<_> = merged
            .packages
            .keys()
            .filter(|k| k.starts_with("consumer@"))
            .cloned()
            .collect();
        assert_eq!(
            consumer_keys_on.len(),
            1,
            "expected single canonical variant with dedupe_peer_dependents=true, got: {:?}",
            consumer_keys_on
        );
    }

    // `dedupe-peers=true` should emit suffixes as `(version)` instead
    // of `(name@version)`. The `parse_dep_path` function in
    // aube-lockfile handles both forms (splits on the first `(`), so
    // round-tripping the key still gives back the package name and
    // canonical version.
    #[test]
    fn dedupe_peers_suffix_is_version_only() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "18.2.0")],
            &[("react", "^18")],
        );
        consumer.dep_path = "consumer@1.0.0".to_string();
        let react = mk_locked("react", "18.2.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("react@18.2.0".to_string(), react);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let options = PeerContextOptions {
            dedupe_peers: true,
            ..PeerContextOptions::default()
        };
        let out = apply_peer_contexts(graph, &options);

        // Suffix should be `(18.2.0)`, not `(react@18.2.0)`.
        assert!(
            out.packages.contains_key("consumer@1.0.0(18.2.0)"),
            "expected version-only suffix: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        assert!(
            !out.packages.contains_key("consumer@1.0.0(react@18.2.0)"),
            "name-based suffix should not appear under dedupe-peers=true"
        );
    }

    // `resolve-peers-from-workspace-root=true` should satisfy an
    // unresolved peer from the root importer's direct deps BEFORE the
    // graph-wide scan tier. Fixture: workspace importer `packages/app`
    // directly depends on `consumer`, which peers on react@>=17.
    // `packages/app` itself has no react in its deps. Root importer
    // pins `react@17.0.2`; the graph also contains `react@18.2.0`
    // reachable via some other path. Because ancestor_scope for
    // consumer is built from `packages/app`'s direct deps (NOT root's),
    // react is missing from the ancestor chain — so only the
    // root-tier and graph-scan tiers can satisfy it, and they resolve
    // to different versions. Paired on/off assertions distinguish
    // which tier ran.
    #[test]
    fn resolve_peers_from_workspace_root_prefers_root() {
        let build_graph = || {
            let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("react", ">=17")]);
            consumer.dep_path = "consumer@1.0.0".to_string();
            let react17 = mk_locked("react", "17.0.2", &[], &[]);
            let react18 = mk_locked("react", "18.2.0", &[], &[]);

            let mut packages = BTreeMap::new();
            packages.insert("consumer@1.0.0".to_string(), consumer);
            packages.insert("react@17.0.2".to_string(), react17);
            packages.insert("react@18.2.0".to_string(), react18);

            let mut importers = BTreeMap::new();
            // Root importer: pins react@17.0.2. Feeds root_scope.
            importers.insert(
                ".".to_string(),
                vec![DirectDep {
                    name: "react".to_string(),
                    dep_path: "react@17.0.2".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^17".to_string()),
                }],
            );
            // Workspace importer: depends on consumer, but does NOT
            // have react in its own direct deps. Consumer's
            // ancestor_scope therefore does not include react, forcing
            // peer resolution down to the root-or-scan tiers.
            importers.insert(
                "packages/app".to_string(),
                vec![DirectDep {
                    name: "consumer".to_string(),
                    dep_path: "consumer@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                }],
            );

            LockfileGraph {
                importers,
                packages,
                ..Default::default()
            }
        };

        let options_on = PeerContextOptions {
            resolve_from_workspace_root: true,
            ..PeerContextOptions::default()
        };
        let out_on = apply_peer_contexts(build_graph(), &options_on);
        assert!(
            out_on.packages.contains_key("consumer@1.0.0(react@17.0.2)"),
            "with flag on, consumer should resolve peer from workspace root (17.0.2): {:?}",
            out_on.packages.keys().collect::<Vec<_>>()
        );

        let options_off = PeerContextOptions {
            resolve_from_workspace_root: false,
            ..PeerContextOptions::default()
        };
        let out_off = apply_peer_contexts(build_graph(), &options_off);
        assert!(
            out_off
                .packages
                .contains_key("consumer@1.0.0(react@18.2.0)"),
            "with flag off, consumer should fall through to graph-wide scan (18.2.0): {:?}",
            out_off.packages.keys().collect::<Vec<_>>()
        );
    }

    // Mutual-peer cycle fixture with `dedupe-peers=true` should still
    // converge without hitting MAX_ITERATIONS. The cycle-break
    // handling in `contains_canonical_back_ref` uses the `name@version`
    // form of the canonical base, but when `dedupe_peers=true` the
    // suffix uses just `version` — the check still succeeds because
    // nested tails reach back to the same `canonical_base` computed
    // from the input key (which is still `name@version`).
    #[test]
    fn dedupe_peers_cycle_break_still_converges() {
        let a = mk_locked("a", "1.0.0", &[("b", "1.0.0")], &[("b", "^1")]);
        let b = mk_locked("b", "1.0.0", &[("a", "1.0.0")], &[("a", "^1")]);

        let mut packages = BTreeMap::new();
        packages.insert("a@1.0.0".to_string(), a);
        packages.insert("b@1.0.0".to_string(), b);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "a".to_string(),
                dep_path: "a@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let canonical = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let options = PeerContextOptions {
            dedupe_peers: true,
            ..PeerContextOptions::default()
        };
        let out = apply_peer_contexts(canonical, &options);

        // Under dedupe_peers=true the keys collapse to version-only
        // suffixes.
        let a_key = "a@1.0.0(1.0.0)";
        let b_key = "b@1.0.0(1.0.0)";
        assert!(
            out.packages.contains_key(a_key),
            "expected {a_key} in {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        assert!(
            out.packages.contains_key(b_key),
            "expected {b_key} in {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );

        // Every referenced dependency tail resolves to a real entry
        // — proves the cycle break didn't strand references.
        for pkg in out.packages.values() {
            for (child_name, child_tail) in &pkg.dependencies {
                let child_key = format!("{child_name}@{child_tail}");
                assert!(
                    out.packages.contains_key(&child_key),
                    "dangling dep_path {child_key} referenced from {}",
                    pkg.dep_path
                );
            }
        }
    }

    // Regression: under `dedupe-peers=true`, a package whose canonical
    // version coincidentally matches a nested peer's version in an
    // unrelated subtree must NOT collide. Cycle detection runs against
    // the full `name@version` form during the fixed-point loop, and
    // `dedupe_peer_suffixes` rewrites the suffix to version-only as a
    // purely cosmetic post-pass — so A@1.0.0's cycle check against
    // B's tail `2.0.0(c@1.0.0)` distinguishes "C at 1.0.0" from
    // "back-ref to A at 1.0.0".
    #[test]
    fn dedupe_peers_no_false_positive_on_version_collision() {
        // A@1.0.0 peers on B. B@2.0.0 peers on C. C@1.0.0 has no peers.
        // A and C share version 1.0.0 but are otherwise unrelated.
        // Under `dedupe_peers=true` B's deduped tail is `(2.0.0(1.0.0))`
        // — the inner `1.0.0` is C's peer, not a back-ref to A.
        let a = mk_locked("a", "1.0.0", &[("b", "2.0.0")], &[("b", "^2")]);
        let b = mk_locked("b", "2.0.0", &[("c", "1.0.0")], &[("c", "^1")]);
        let c = mk_locked("c", "1.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("a@1.0.0".to_string(), a);
        packages.insert("b@2.0.0".to_string(), b);
        packages.insert("c@1.0.0".to_string(), c);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "a".to_string(),
                dep_path: "a@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let options = PeerContextOptions {
            dedupe_peers: true,
            ..PeerContextOptions::default()
        };
        let out = apply_peer_contexts(graph, &options);

        // A's key must carry B's full nested tail including C's peer.
        // If cycle detection false-positived on the bare version, B's
        // tail would collapse to `(2.0.0)` (dropping `(1.0.0)`) and
        // we'd see `a@1.0.0(2.0.0)` instead.
        assert!(
            out.packages.contains_key("a@1.0.0(2.0.0(1.0.0))"),
            "expected A's key to preserve B's nested peer chain: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
        assert!(
            !out.packages.contains_key("a@1.0.0(2.0.0)"),
            "false-positive cycle break would produce the truncated form"
        );
    }

    // Unit test for the dedupe-peers post-pass: given a key with
    // `name@version` suffix segments, produce the version-only form.
    #[test]
    fn apply_dedupe_peers_to_key_strips_names_in_suffix() {
        assert_eq!(
            apply_dedupe_peers_to_key("react-dom@18.2.0(react@18.2.0)"),
            "react-dom@18.2.0(18.2.0)"
        );
        assert_eq!(
            apply_dedupe_peers_to_key("a@1.0.0(b@2.0.0(c@3.0.0))"),
            "a@1.0.0(2.0.0(3.0.0))"
        );
        // No parens = no change.
        assert_eq!(apply_dedupe_peers_to_key("react@18.2.0"), "react@18.2.0");
        // Already deduped (no `name@` inside parens) = no change.
        assert_eq!(
            apply_dedupe_peers_to_key("a@1.0.0(18.2.0)"),
            "a@1.0.0(18.2.0)"
        );
    }

    // Regression: two peer-variant keys that differ only in which peer
    // NAME they declared (but whose peer versions coincide) must not
    // silently collapse into each other when `dedupe_peers=true`.
    // `apply_dedupe_peers_to_key` strips peer names, so naive insertion
    // into a `BTreeMap` would drop one variant. `dedupe_peer_suffixes`
    // detects the collision and keeps both sides in full form.
    #[test]
    fn dedupe_peer_suffixes_preserves_full_form_on_name_collision() {
        // Construct two distinct variants that would collide after
        // naive suffix rewriting:
        //   consumer@1.0.0(foo@1.0.0)  and  consumer@1.0.0(bar@1.0.0)
        let consumer_foo = {
            let mut pkg = mk_locked("consumer", "1.0.0", &[("foo", "1.0.0")], &[("foo", "^1")]);
            pkg.dep_path = "consumer@1.0.0(foo@1.0.0)".to_string();
            pkg
        };
        let consumer_bar = {
            let mut pkg = mk_locked("consumer", "1.0.0", &[("bar", "1.0.0")], &[("bar", "^1")]);
            pkg.dep_path = "consumer@1.0.0(bar@1.0.0)".to_string();
            pkg
        };
        let foo = mk_locked("foo", "1.0.0", &[], &[]);
        let bar = mk_locked("bar", "1.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0(foo@1.0.0)".to_string(), consumer_foo);
        packages.insert("consumer@1.0.0(bar@1.0.0)".to_string(), consumer_bar);
        packages.insert("foo@1.0.0".to_string(), foo);
        packages.insert("bar@1.0.0".to_string(), bar);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "consumer".to_string(),
                    dep_path: "consumer@1.0.0(foo@1.0.0)".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                },
                DirectDep {
                    name: "consumer".to_string(),
                    dep_path: "consumer@1.0.0(bar@1.0.0)".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                },
            ],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let out = dedupe_peer_suffixes(graph);

        // Both variants must survive: colliding keys fall back to the
        // original full-form keys instead of silently overwriting each
        // other.
        let consumer_keys: BTreeSet<_> = out
            .packages
            .keys()
            .filter(|k| k.starts_with("consumer@"))
            .cloned()
            .collect();
        assert_eq!(
            consumer_keys.len(),
            2,
            "both consumer variants must survive collision: {consumer_keys:?}"
        );
        assert!(consumer_keys.contains("consumer@1.0.0(foo@1.0.0)"));
        assert!(consumer_keys.contains("consumer@1.0.0(bar@1.0.0)"));

        // Importer references to the full-form keys must stay pointing
        // at the preserved variants.
        let importer_keys: BTreeSet<_> = out
            .importers
            .get(".")
            .unwrap()
            .iter()
            .map(|d| d.dep_path.clone())
            .collect();
        assert!(importer_keys.contains("consumer@1.0.0(foo@1.0.0)"));
        assert!(importer_keys.contains("consumer@1.0.0(bar@1.0.0)"));
    }

    // Scoped packages have two `@` chars (scope prefix + version
    // separator); the version separator is the rightmost one, so the
    // suffix-stripper must use `rfind('@')`. Regression for a bug
    // where `find('@')` returned the scope's leading `@` and produced
    // malformed keys like `(types/react@18.2.0)`.
    #[test]
    fn apply_dedupe_peers_to_key_handles_scoped_packages() {
        assert_eq!(
            apply_dedupe_peers_to_key("consumer@1.0.0(@types/react@18.2.0)"),
            "consumer@1.0.0(18.2.0)"
        );
        // Scoped head and scoped peer.
        assert_eq!(
            apply_dedupe_peers_to_key("@foo/bar@1.0.0(@types/react@18.2.0)"),
            "@foo/bar@1.0.0(18.2.0)"
        );
        // Nested scoped peers.
        assert_eq!(
            apply_dedupe_peers_to_key("a@1.0.0(@types/react@18.2.0(@babel/core@7.0.0))"),
            "a@1.0.0(18.2.0(7.0.0))"
        );
    }

    // Cycle helper sanity check: a value that contains the canonical
    // back-ref should be recognized only at proper boundaries, not
    // inside longer version strings.
    #[test]
    fn contains_canonical_back_ref_respects_boundaries() {
        assert!(contains_canonical_back_ref("1.0.0(a@1.0.0)", "a@1.0.0"));
        assert!(contains_canonical_back_ref(
            "1.0.0(a@1.0.0(b@1.0.0))",
            "a@1.0.0"
        ));
        // False positive guard: "a@1.0" should NOT match inside
        // "a@1.0.5" because the following char ('.') is not a boundary.
        assert!(!contains_canonical_back_ref("1.0.0(a@1.0.5)", "a@1.0"));
        // No match when the canonical isn't inside a peer suffix at all.
        assert!(!contains_canonical_back_ref("1.0.0", "a@1.0.0"));
    }

    // A package whose only dep is another package that declares a peer
    // should hoist that peer to the importer — matching pnpm's
    // `auto-install-peers=true` default. The hoisted DirectDep carries
    // the declared peer range as its specifier.
    #[test]
    fn hoist_auto_installed_peers_hoists_unmet_peers_to_importer() {
        // consumer declares `peer react: ^17 || ^18` and already has
        // `react@18.2.0` wired via its auto-install dependencies map.
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "18.2.0")],
            &[("react", "^17 || ^18")],
        );
        consumer.dep_path = "consumer@1.0.0".to_string();

        let react = mk_locked("react", "18.2.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("react@18.2.0".to_string(), react);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let hoisted = hoist_auto_installed_peers(graph);
        let root = hoisted.importers.get(".").unwrap();

        // Sorted by name → [consumer, react].
        assert_eq!(root.len(), 2);
        assert_eq!(root[0].name, "consumer");
        assert_eq!(root[1].name, "react");
        assert_eq!(root[1].dep_path, "react@18.2.0");
        assert_eq!(root[1].dep_type, DepType::Production);
        // Specifier carries the declared peer range verbatim.
        assert_eq!(root[1].specifier.as_deref(), Some("^17 || ^18"));
    }

    // If the peer is already in the importer's direct deps, hoist is a
    // no-op — we don't duplicate or shadow the user's own specifier.
    #[test]
    fn hoist_auto_installed_peers_leaves_already_satisfied_peers_alone() {
        let consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "17.0.2")],
            &[("react", "^17 || ^18")],
        );
        let react = mk_locked("react", "17.0.2", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("react@17.0.2".to_string(), react);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![
                DirectDep {
                    name: "consumer".to_string(),
                    dep_path: "consumer@1.0.0".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("^1".to_string()),
                },
                DirectDep {
                    name: "react".to_string(),
                    dep_path: "react@17.0.2".to_string(),
                    dep_type: DepType::Production,
                    specifier: Some("17.0.2".to_string()),
                },
            ],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let hoisted = hoist_auto_installed_peers(graph);
        let root = hoisted.importers.get(".").unwrap();

        // Still just the two original entries — no extra react snuck in.
        assert_eq!(root.len(), 2);
        let react_dep = root.iter().find(|d| d.name == "react").unwrap();
        // The user's own pin (17.0.2) survives — not clobbered by the
        // peer range.
        assert_eq!(react_dep.specifier.as_deref(), Some("17.0.2"));
    }

    // `detect_unmet_peers` should flag a package whose declared peer
    // range isn't satisfied by whatever the graph ends up providing.
    // This is the core case: user pins `react@15.7.0`, a consumer
    // declares `peer react: ^18`, and we need a warning so the user
    // knows their runtime will break.
    #[test]
    fn detect_unmet_peers_flags_version_mismatch() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "15.7.0")],
            &[("react", "^18")],
        );
        consumer.dep_path = "consumer@1.0.0(react@15.7.0)".to_string();

        let mut packages = BTreeMap::new();
        packages.insert(consumer.dep_path.clone(), consumer);

        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages,
            ..Default::default()
        };

        let unmet = detect_unmet_peers(&graph);
        assert_eq!(unmet.len(), 1, "expected one unmet peer, got {unmet:?}");
        let u = &unmet[0];
        assert_eq!(u.from_name, "consumer");
        assert_eq!(u.peer_name, "react");
        assert_eq!(u.declared, "^18");
        assert_eq!(u.found.as_deref(), Some("15.7.0"));
    }

    // When the resolved version *does* satisfy the declared range, no
    // warning should fire.
    #[test]
    fn detect_unmet_peers_silent_when_satisfied() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "18.2.0")],
            &[("react", "^17 || ^18")],
        );
        consumer.dep_path = "consumer@1.0.0(react@18.2.0)".to_string();

        let mut packages = BTreeMap::new();
        packages.insert(consumer.dep_path.clone(), consumer);

        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages,
            ..Default::default()
        };
        assert!(detect_unmet_peers(&graph).is_empty());
    }

    // Peer declared but completely absent from `pkg.dependencies` —
    // exercises the `found: None` branch that drives the "missing
    // required peer" display path in `warn_unmet_peers`. Rare in
    // practice because the BFS peer walk usually drags *some* version
    // in, but possible for corner cases (registry fetch failure, etc).
    #[test]
    fn detect_unmet_peers_flags_completely_missing_peer() {
        let mut consumer = mk_locked("consumer", "1.0.0", &[], &[("react", "^18")]);
        consumer.dep_path = "consumer@1.0.0".to_string();

        let mut packages = BTreeMap::new();
        packages.insert(consumer.dep_path.clone(), consumer);

        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages,
            ..Default::default()
        };

        let unmet = detect_unmet_peers(&graph);
        assert_eq!(unmet.len(), 1);
        let u = &unmet[0];
        assert_eq!(u.from_name, "consumer");
        assert_eq!(u.peer_name, "react");
        assert_eq!(u.declared, "^18");
        assert_eq!(u.found, None);
    }

    // Optional peers are suppressed even when they would otherwise be
    // flagged — matches pnpm's `peerDependenciesMeta.optional` behavior
    // with `auto-install-peers=true`.
    #[test]
    fn detect_unmet_peers_skips_optional_peers() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("react", "15.7.0")],
            &[("react", "^18")],
        );
        consumer.dep_path = "consumer@1.0.0(react@15.7.0)".to_string();
        consumer.peer_dependencies_meta.insert(
            "react".to_string(),
            aube_lockfile::PeerDepMeta { optional: true },
        );

        let mut packages = BTreeMap::new();
        packages.insert(consumer.dep_path.clone(), consumer);

        let graph = LockfileGraph {
            importers: BTreeMap::new(),
            packages,
            ..Default::default()
        };
        assert!(detect_unmet_peers(&graph).is_empty());
    }

    // Mutual dependency cycles must not hang the BFS resolver. The
    // walker dedupes on `name@version`, so the second time the cycle
    // brings us back to a package we already resolved, we wire the
    // parent edge but skip recursing into its transitives.
    //
    //     cycle-a@1.0.0 -> cycle-b@1.0.0
    //     cycle-b@1.0.0 -> cycle-a@1.0.0
    #[tokio::test]
    async fn resolve_terminates_on_dependency_cycle() {
        let mut a = make_packument("cycle-a", &["1.0.0"], "1.0.0");
        a.versions
            .get_mut("1.0.0")
            .unwrap()
            .dependencies
            .insert("cycle-b".to_string(), "1.0.0".to_string());
        let mut b = make_packument("cycle-b", &["1.0.0"], "1.0.0");
        b.versions
            .get_mut("1.0.0")
            .unwrap()
            .dependencies
            .insert("cycle-a".to_string(), "1.0.0".to_string());

        // The RegistryClient is never hit because we pre-seed the cache.
        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver.cache.insert("cycle-a".to_string(), a);
        resolver.cache.insert("cycle-b".to_string(), b);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("cycle-a".to_string(), "1.0.0".to_string());

        let graph = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            resolver.resolve(&manifest, None),
        )
        .await
        .expect("resolver hung on dependency cycle")
        .expect("resolve failed");

        assert!(graph.packages.contains_key("cycle-a@1.0.0"));
        assert!(graph.packages.contains_key("cycle-b@1.0.0"));
        assert_eq!(
            graph.packages["cycle-a@1.0.0"].dependencies.get("cycle-b"),
            Some(&"1.0.0".to_string())
        );
        assert_eq!(
            graph.packages["cycle-b@1.0.0"].dependencies.get("cycle-a"),
            Some(&"1.0.0".to_string())
        );
    }

    #[tokio::test]
    async fn auto_install_peers_installs_missing_required_peer() {
        let mut consumer = make_packument("consumer", &["1.0.0"], "1.0.0");
        consumer
            .versions
            .get_mut("1.0.0")
            .unwrap()
            .peer_dependencies
            .insert("react".to_string(), "^18".to_string());
        let react = make_packument("react", &["18.2.0"], "18.2.0");

        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver.cache.insert("consumer".to_string(), consumer);
        resolver.cache.insert("react".to_string(), react);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("consumer".to_string(), "1.0.0".to_string());

        let graph = resolver
            .resolve(&manifest, None)
            .await
            .expect("resolve failed");

        assert!(graph_has_package(&graph, "consumer", "1.0.0"));
        assert!(
            graph_has_package(&graph, "react", "18.2.0"),
            "missing required peer should be auto-installed"
        );
    }

    #[tokio::test]
    async fn auto_install_peers_uses_importer_declared_peer_name_without_extra_version() {
        let mut plugin = make_packument("plugin", &["1.0.0"], "1.0.0");
        plugin
            .versions
            .get_mut("1.0.0")
            .unwrap()
            .peer_dependencies
            .insert("eslint".to_string(), "^8.56.0".to_string());
        let eslint = make_packument("eslint", &["8.57.1", "9.0.0"], "9.0.0");

        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver.cache.insert("plugin".to_string(), plugin);
        resolver.cache.insert("eslint".to_string(), eslint);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("eslint".to_string(), "^9".to_string());
        manifest
            .dependencies
            .insert("plugin".to_string(), "1.0.0".to_string());

        let graph = resolver
            .resolve(&manifest, None)
            .await
            .expect("resolve failed");

        assert!(graph_has_package(&graph, "eslint", "9.0.0"));
        assert!(graph_has_package(&graph, "plugin", "1.0.0"));
        assert!(
            !graph_has_package(&graph, "eslint", "8.57.1"),
            "importer-declared peer name should not pull a second compatible peer tree"
        );
        let unmet = detect_unmet_peers(&graph);
        assert!(
            unmet.iter().any(|unmet| unmet.from_name == "plugin"
                && unmet.peer_name == "eslint"
                && unmet.declared == "^8.56.0"
                && unmet.found.as_deref() == Some("9.0.0")),
            "incompatible importer peer should surface as a version-mismatch warning"
        );
    }

    #[tokio::test]
    async fn auto_install_peers_skips_unrequested_optional_peer_alternatives() {
        let mut loader = make_packument("loader", &["1.0.0"], "1.0.0");
        let loader_meta = loader.versions.get_mut("1.0.0").unwrap();
        loader_meta
            .peer_dependencies
            .insert("sass".to_string(), "^1".to_string());
        loader_meta
            .peer_dependencies
            .insert("webpack".to_string(), "^5".to_string());
        loader_meta
            .peer_dependencies
            .insert("@rspack/core".to_string(), "^1".to_string());
        loader_meta
            .peer_dependencies
            .insert("node-sass".to_string(), "^9".to_string());
        loader_meta.peer_dependencies_meta.insert(
            "@rspack/core".to_string(),
            aube_registry::PeerDepMeta { optional: true },
        );
        loader_meta.peer_dependencies_meta.insert(
            "node-sass".to_string(),
            aube_registry::PeerDepMeta { optional: true },
        );

        let sass = make_packument("sass", &["1.69.0"], "1.69.0");
        let webpack = make_packument("webpack", &["5.0.0"], "5.0.0");
        let rspack = make_packument("@rspack/core", &["1.0.0"], "1.0.0");
        let node_sass = make_packument("node-sass", &["9.0.0"], "9.0.0");

        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver.cache.insert("loader".to_string(), loader);
        resolver.cache.insert("sass".to_string(), sass);
        resolver.cache.insert("webpack".to_string(), webpack);
        resolver.cache.insert("@rspack/core".to_string(), rspack);
        resolver.cache.insert("node-sass".to_string(), node_sass);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("loader".to_string(), "1.0.0".to_string());
        manifest
            .dependencies
            .insert("sass".to_string(), "^1".to_string());
        manifest
            .dependencies
            .insert("webpack".to_string(), "^5".to_string());

        let graph = resolver
            .resolve(&manifest, None)
            .await
            .expect("resolve failed");

        assert!(graph_has_package(&graph, "loader", "1.0.0"));
        assert!(graph_has_package(&graph, "sass", "1.69.0"));
        assert!(graph_has_package(&graph, "webpack", "5.0.0"));
        assert!(
            !graph_has_package(&graph, "@rspack/core", "1.0.0"),
            "optional peer alternative should not be auto-installed"
        );
        assert!(
            !graph_has_package(&graph, "node-sass", "9.0.0"),
            "optional peer alternative should not be auto-installed"
        );
    }

    // Scenario test for the bug Cursor Bugbot flagged on #142:
    // lockfile has `dep-a@1.0.0`; manifest wants both `dep-a@^1`
    // (matches lockfile) AND `other-a@^2` (fresh); `other-a@2.0.0`
    // declares a transitive `dep-a@^2` that no lockfile entry
    // satisfies.
    //
    // Correct behavior: resolver picks dep-a@1.0.0 for the direct
    // dep (via lockfile reuse) and dep-a@2.0.0 for the transitive
    // (via the fetch path).
    //
    // The original bug: `ensure_fetch!` wrongly skipped the spawn
    // when `resolved_versions[dep-a]` was non-empty, regardless of
    // whether the packument was actually in `self.cache`. The
    // lockfile-reuse path populates `resolved_versions` without
    // ever caching the packument, so the transitive dep-a@^2 task
    // fell through to the fetch-wait loop, called `ensure_fetch!`,
    // got skipped, and panicked with "packument fetch disappeared
    // before completing". The fix removes the `resolved_versions`
    // guard from `ensure_fetch!` — the macro now checks only
    // in-flight + cache, and prefetch gating on lockfile-covered
    // names is done by callers via an explicit `existing_names`
    // check.
    //
    // Note: this test pre-seeds the resolver cache with both
    // packuments, so the wait-for-fetch loop exits immediately
    // without actually calling `ensure_fetch!` — which means the
    // test passes with or without the fix. It's kept as an
    // end-to-end scenario assertion (resolver produces the
    // expected two-version graph) rather than a direct regression
    // test for the `ensure_fetch!` bug itself. Triggering the
    // actual bug requires a real registry mock that returns the
    // packument during the wait loop, which the unit-test harness
    // doesn't have; the BATS suite covers the end-to-end path
    // through a local Verdaccio registry.
    #[tokio::test]
    async fn resolve_handles_lockfile_reused_name_with_incompatible_transitive_range() {
        // Packument for `dep-a` has both a 1.x and a 2.x line; only
        // 1.0.0 is in the (fake) lockfile, so the fetch path has to
        // cover the 2.x case.
        let dep_a = make_packument("dep-a", &["1.0.0", "2.0.0"], "2.0.0");
        // `other-a@2.0.0` is the package that triggers the
        // transitive `dep-a@^2` task.
        let mut other_a = make_packument("other-a", &["2.0.0"], "2.0.0");
        other_a
            .versions
            .get_mut("2.0.0")
            .unwrap()
            .dependencies
            .insert("dep-a".to_string(), "^2".to_string());

        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        // Pre-seed the in-memory packument cache so the resolver
        // never needs to touch the fake registry URL.
        resolver.cache.insert("dep-a".to_string(), dep_a);
        resolver.cache.insert("other-a".to_string(), other_a);

        // Existing lockfile: has `dep-a@1.0.0` (the lockfile-reuse
        // hit) but nothing else. `other-a@^2` is a fresh dep that
        // won't lockfile-reuse.
        let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
        existing_pkgs.insert(
            "dep-a@1.0.0".to_string(),
            LockedPackage {
                name: "dep-a".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "dep-a@1.0.0".to_string(),
                ..Default::default()
            },
        );
        let existing = LockfileGraph {
            packages: existing_pkgs,
            importers: BTreeMap::new(),
            settings: Default::default(),
            overrides: BTreeMap::new(),
            ignored_optional_dependencies: BTreeSet::new(),
            times: BTreeMap::new(),
            skipped_optional_dependencies: BTreeMap::new(),
            catalogs: BTreeMap::new(),
        };

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("dep-a".to_string(), "^1".to_string());
        manifest
            .dependencies
            .insert("other-a".to_string(), "^2".to_string());

        let graph = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            resolver.resolve(&manifest, Some(&existing)),
        )
        .await
        .expect("resolver hung")
        .expect("resolve failed");

        // Both versions of dep-a should be in the resolved graph:
        // 1.0.0 from lockfile-reuse, 2.0.0 from the fetch path.
        assert!(
            graph.packages.contains_key("dep-a@1.0.0"),
            "dep-a@1.0.0 missing (lockfile reuse)"
        );
        assert!(
            graph.packages.contains_key("dep-a@2.0.0"),
            "dep-a@2.0.0 missing (transitive fetch fell through the ensure_fetch guard)"
        );
        assert!(graph.packages.contains_key("other-a@2.0.0"));
    }

    #[tokio::test]
    async fn lockfile_reuse_preserves_transitive_optional_edges() {
        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);

        let mut existing_pkgs: BTreeMap<String, LockedPackage> = BTreeMap::new();
        existing_pkgs.insert(
            "host@1.0.0".to_string(),
            LockedPackage {
                name: "host".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dependencies: [("native".to_string(), "1.0.0".to_string())].into(),
                optional_dependencies: [("native".to_string(), "1.0.0".to_string())].into(),
                ..Default::default()
            },
        );
        existing_pkgs.insert(
            "native@1.0.0".to_string(),
            LockedPackage {
                name: "native".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native@1.0.0".to_string(),
                ..Default::default()
            },
        );
        let existing = LockfileGraph {
            packages: existing_pkgs,
            importers: BTreeMap::new(),
            settings: Default::default(),
            overrides: BTreeMap::new(),
            ignored_optional_dependencies: BTreeSet::new(),
            times: BTreeMap::new(),
            skipped_optional_dependencies: BTreeMap::new(),
            catalogs: BTreeMap::new(),
        };

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("host".to_string(), "1.0.0".to_string());

        let graph = resolver
            .resolve(&manifest, Some(&existing))
            .await
            .expect("resolve failed");

        let host = graph.packages.get("host@1.0.0").unwrap();
        assert_eq!(host.dependencies.get("native").unwrap(), "1.0.0");
        assert_eq!(
            host.optional_dependencies.get("native").unwrap(),
            "1.0.0",
            "lockfile reuse must keep the optional edge metadata for write()"
        );
    }

    // ===== peersSuffixMaxLength =====
    //
    // Helpers exercised directly: `hash_peer_suffix` for the format
    // invariant; `apply_peer_contexts` for the integration path that
    // reads the cap and decides whether to swap the suffix.

    #[test]
    fn hash_peer_suffix_matches_expected_format() {
        let out = hash_peer_suffix("(react@18.2.0)");
        // `_` prefix, 10 hex chars, nothing else.
        assert!(out.starts_with('_'), "expected `_` prefix: {out:?}");
        assert_eq!(out.len(), 11, "expected `_` + 10 hex chars: {out:?}");
        assert!(
            out[1..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "expected lowercase hex after `_`: {out:?}"
        );
        // Stable output — regression guard against accidental format changes.
        assert_eq!(hash_peer_suffix("(react@18.2.0)"), out);
    }

    // Small cap forces the suffix to collapse to `_<hex>`. Uses the
    // nested-peer fixture that already proves correct behavior at the
    // default cap — same fixture, different cap, different output.
    #[test]
    fn peer_suffix_is_hashed_when_exceeding_cap() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("adapter", "1.0.0"), ("core", "1.0.0")],
            &[("adapter", "^1"), ("core", "^1")],
        );
        consumer.dep_path = "consumer@1.0.0".to_string();
        let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
        adapter.dep_path = "adapter@1.0.0".to_string();
        let core = mk_locked("core", "1.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("adapter@1.0.0".to_string(), adapter);
        packages.insert("core@1.0.0".to_string(), core);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        // Cap of 10 bytes is smaller than any realistic suffix.
        let options = PeerContextOptions {
            peers_suffix_max_length: 10,
            ..PeerContextOptions::default()
        };
        let out = apply_peer_contexts(graph, &options);

        // At least one package should have a hashed suffix. The outer
        // `consumer` package is the one most likely to overflow (nested
        // suffix `(adapter@1.0.0(core@1.0.0))(core@1.0.0)` = 42 bytes).
        let consumer_key = out
            .packages
            .keys()
            .find(|k| k.starts_with("consumer@1.0.0"))
            .cloned()
            .expect("consumer@1.0.0 variant missing");
        let suffix = consumer_key.strip_prefix("consumer@1.0.0").unwrap();
        assert!(
            suffix.starts_with('_') && suffix.len() == 11,
            "expected hashed suffix _<10-hex>, got {suffix:?} from {consumer_key:?}"
        );
    }

    // Default cap leaves the nested form byte-identical to pre-cap output.
    // Regression guard: the wiring must not change behavior when the cap
    // isn't hit — which is the overwhelmingly common case.
    #[test]
    fn peer_suffix_unchanged_when_within_cap() {
        let mut consumer = mk_locked(
            "consumer",
            "1.0.0",
            &[("adapter", "1.0.0"), ("core", "1.0.0")],
            &[("adapter", "^1"), ("core", "^1")],
        );
        consumer.dep_path = "consumer@1.0.0".to_string();
        let mut adapter = mk_locked("adapter", "1.0.0", &[("core", "1.0.0")], &[("core", "^1")]);
        adapter.dep_path = "adapter@1.0.0".to_string();
        let core = mk_locked("core", "1.0.0", &[], &[]);

        let mut packages = BTreeMap::new();
        packages.insert("consumer@1.0.0".to_string(), consumer);
        packages.insert("adapter@1.0.0".to_string(), adapter);
        packages.insert("core@1.0.0".to_string(), core);

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "consumer".to_string(),
                dep_path: "consumer@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: Some("^1".to_string()),
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };
        let out = apply_peer_contexts(graph, &PeerContextOptions::default());

        // The nested-peer test's expected key must still be produced.
        assert!(
            out.packages
                .contains_key("consumer@1.0.0(adapter@1.0.0(core@1.0.0))(core@1.0.0)"),
            "default cap corrupted output: {:?}",
            out.packages.keys().collect::<Vec<_>>()
        );
    }

    // Fresh resolve: when the root manifest carries
    // `"odd-alias": "npm:is-odd@3.0.1"`, the resolver must emit the
    // graph keyed by the *alias* and stash the real registry name in
    // `alias_of`. Before this fix, `task.name` was clobbered to
    // `is-odd` at the `npm:` rewrite site, which collapsed
    // `node_modules/odd-alias/` to `node_modules/is-odd/` and broke
    // `require("odd-alias")` at runtime.
    #[tokio::test]
    async fn fresh_resolve_preserves_npm_alias_as_folder_name() {
        let is_odd = make_packument("is-odd", &["3.0.1"], "3.0.1");

        // Pre-seed the cache under the *real* package name — the
        // whole point of the fix is that the registry fetch keys by
        // the real name (`is-odd`), not the alias-qualified
        // `odd-alias` that would 404 the registry.
        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver.cache.insert("is-odd".to_string(), is_odd);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("odd-alias".to_string(), "npm:is-odd@3.0.1".to_string());

        let graph = resolver
            .resolve(&manifest, None)
            .await
            .expect("alias resolve failed");

        // Graph key and `LockedPackage.name` both carry the alias —
        // that's what the linker drops into `node_modules/` and what
        // any `require("odd-alias")` walks to.
        let pkg = graph
            .packages
            .get("odd-alias@3.0.1")
            .expect("aliased package must be keyed by the alias dep_path");
        assert_eq!(pkg.name, "odd-alias");
        assert_eq!(pkg.version, "3.0.1");
        assert_eq!(pkg.alias_of.as_deref(), Some("is-odd"));
        assert_eq!(pkg.registry_name(), "is-odd");

        // No stray `is-odd@3.0.1` entry from the rewrite leaking the
        // real name past the alias boundary.
        assert!(!graph.packages.contains_key("is-odd@3.0.1"));

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "odd-alias");
        assert_eq!(root[0].dep_path, "odd-alias@3.0.1");
    }

    #[tokio::test]
    async fn fresh_resolve_preserves_jsr_name_as_folder_name() {
        let jsr_collections = make_packument("@jsr/std__collections", &["1.1.6"], "1.1.6");

        let client = Arc::new(aube_registry::client::RegistryClient::new(
            "http://127.0.0.1:0",
        ));
        let mut resolver = Resolver::new(client);
        resolver
            .cache
            .insert("@jsr/std__collections".to_string(), jsr_collections);

        let mut manifest = PackageJson::default();
        manifest
            .dependencies
            .insert("@std/collections".to_string(), "jsr:^1.1.6".to_string());

        let graph = resolver
            .resolve(&manifest, None)
            .await
            .expect("jsr resolve failed");

        let pkg = graph
            .packages
            .get("@std/collections@1.1.6")
            .expect("jsr package must be keyed by the user-facing dep_path");
        assert_eq!(pkg.name, "@std/collections");
        assert_eq!(pkg.version, "1.1.6");
        assert_eq!(pkg.alias_of.as_deref(), Some("@jsr/std__collections"));
        assert_eq!(pkg.registry_name(), "@jsr/std__collections");
        assert!(
            pkg.tarball_url
                .as_deref()
                .is_some_and(|url| url.contains("@jsr/std__collections")),
            "JSR resolver output must preserve dist.tarball"
        );
        assert!(!graph.packages.contains_key("@jsr/std__collections@1.1.6"));

        let root = graph.importers.get(".").unwrap();
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].name, "@std/collections");
        assert_eq!(root[0].dep_path, "@std/collections@1.1.6");
    }
}
