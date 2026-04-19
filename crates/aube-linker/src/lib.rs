#[macro_use]
extern crate log;

use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use aube_lockfile::graph_hash::GraphHashes;
use aube_lockfile::{LocalSource, LockedPackage, LockfileGraph};
use aube_store::{PackageIndex, Store};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod hoisted;
pub mod sys;
pub use hoisted::HoistedPlacements;
pub use sys::{
    BinShimOptions, create_bin_shim, create_dir_link, normalize_path, parse_posix_shim_target,
    remove_bin_shim,
};

/// Strategy for arranging packages under `node_modules/`.
///
/// `Isolated` is pnpm's default layout — every package lives under
/// `.aube/<dep_path>/node_modules/<name>` and the top-level
/// `node_modules/<name>` entry is a symlink into that virtual store.
/// `Hoisted` flattens the tree npm-style: packages are materialized
/// directly into `node_modules/<name>/` with conflicting versions
/// nested under the requiring parent. `Hoisted` is slower to
/// materialize and uses more disk, but matches the layout a handful
/// of legacy toolchains still expect.
/// `FromStr` is case-insensitive so settings-file and CLI inputs like
/// `Isolated` or `HOISTED` parse the same as the canonical lowercase
/// spellings. Callers that accept user input should still `trim()`
/// before parsing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, strum::EnumString)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum NodeLinker {
    #[default]
    Isolated,
    Hoisted,
}

/// Links packages from the global store into a project's node_modules/.
///
/// Uses pnpm-compatible symlink layout backed by a global virtual store:
/// - Packages are materialized once in `~/.cache/aube/virtual-store/`
///   (or `$XDG_CACHE_HOME/aube/virtual-store/`)
/// - Per-project `.aube/` entries are symlinks into the global virtual store
/// - Top-level `node_modules/<name>` entries are symlinks to
///   `.aube/<dep_path>/node_modules/<name>` (matching pnpm)
/// - Transitive deps live as sibling symlinks inside `.aube/<dep_path>/node_modules/`
///   so Node's directory walk finds them when resolving from inside the package
pub struct Linker {
    virtual_store: PathBuf,
    /// Keep a handle to the global CAS so the linker can lazy-load a
    /// `PackageIndex` on demand when the install driver skipped
    /// `load_index` on the fast path but a stale symlink or missing
    /// virtual-store entry forces a (re)materialization. Without this,
    /// the optimistic no-op short-circuit in `install.rs` wouldn't be
    /// safe against graph-hash changes (e.g. patches added,
    /// `allowBuilds` entries flipped, engine version bumped).
    pub(crate) store: Store,
    use_global_virtual_store: bool,
    strategy: LinkStrategy,
    /// Per-`name@version` patch contents applied at materialize
    /// time. Empty when the project has no `pnpm.patchedDependencies`.
    pub(crate) patches: Patches,
    /// Optional content-addressed hashes for global-store subdir
    /// naming. When set, every path inside `self.virtual_store` uses
    /// `hashes.hashed_dep_path(dep_path)` as the dep's leaf name,
    /// which folds the recursive dep-graph hash (and the engine
    /// string, for packages that transitively require building) into
    /// the filesystem path. Packages with different builds can't
    /// collide in the shared store because they end up at different
    /// paths. When `None`, the linker falls back to the raw dep_path
    /// (backwards-compatible with pre-hash callers and with the
    /// per-project `.aube/` layout, which always uses dep_path).
    hashes: Option<GraphHashes>,
    /// Cap on the length of a single virtual-store directory name.
    /// Matches pnpm's `virtual-store-dir-max-length` config (default
    /// 120). Every dep_path the linker writes to disk gets routed
    /// through `dep_path_to_filename(_, this)`, which truncates and
    /// hashes names longer than this cap so peer-heavy graphs (e.g.
    /// anything pulling in the ESLint + TypeScript matrix) don't
    /// overflow Linux's 255-byte `NAME_MAX`.
    virtual_store_dir_max_length: usize,
    /// pnpm's `shamefully-hoist`: after creating the usual top-level
    /// symlinks for direct deps, walk every package in the graph and
    /// create a `node_modules/<name>` symlink for any name that
    /// isn't already claimed. Mirrors pnpm's "flat node_modules"
    /// compatibility escape hatch. First-write-wins on name clashes.
    shamefully_hoist: bool,
    /// pnpm's `public-hoist-pattern`: glob list matched against
    /// package names. Any non-local package in the graph whose name
    /// matches at least one positive pattern (and no `!`-prefixed
    /// negation) gets a top-level `node_modules/<name>` symlink in
    /// addition to the direct-dep entries. First-write-wins, so
    /// direct deps and earlier hoist passes keep priority. Empty list
    /// disables the feature entirely. Frameworks like Next.js,
    /// Storybook, and Jest rely on this to resolve transitive deps
    /// from the project root.
    public_hoist_patterns: Vec<glob::Pattern>,
    public_hoist_negations: Vec<glob::Pattern>,
    /// pnpm's `hoist`: master switch for the hidden modules directory
    /// at `node_modules/.aube/node_modules/`. When true (the default),
    /// every non-local package whose name matches `hoist_patterns`
    /// (and no `hoist_negations`) gets a symlink into that hidden
    /// directory so Node's parent-directory walk can satisfy
    /// undeclared deps in third-party packages. When false, the
    /// hidden tree is skipped entirely and any existing
    /// `.aube/node_modules/` is wiped so stale entries don't linger.
    hoist: bool,
    /// pnpm's `hoist-pattern`: glob list matched against package names
    /// for hidden-hoist promotion. Populated with `*` in `new()` so a
    /// default-constructed linker matches everything (pnpm parity).
    /// `with_hoist_pattern` replaces both positive and negative
    /// patterns in full, so passing `[]` or only-negation means
    /// "hoist nothing". Only consulted when `hoist == true`.
    hoist_patterns: Vec<glob::Pattern>,
    hoist_negations: Vec<glob::Pattern>,
    /// pnpm's `hoist-workspace-packages`: when false, workspace
    /// packages are not symlinked into the root `node_modules/`.
    /// Other workspace packages can still resolve them through the
    /// lockfile's workspace protocol, but plain `require('<ws-pkg>')`
    /// from the root stops working. Default true.
    hoist_workspace_packages: bool,
    /// pnpm's `dedupe-direct-deps`: when true, the linker skips
    /// creating a per-importer `node_modules/<name>` symlink for a
    /// direct dep whose root importer already declares the same
    /// package at the same resolved version. The root-level symlink
    /// still exists, so Node's parent-directory walk from inside the
    /// workspace package resolves the same copy — callers just avoid
    /// the duplicate per-importer link. Default false (pnpm parity).
    dedupe_direct_deps: bool,
    /// Active layout mode. `NodeLinker::Isolated` (default) routes
    /// through the existing `.aube/` virtual-store paths;
    /// `NodeLinker::Hoisted` dispatches to `hoisted::link_hoisted_importer`
    /// which writes real package directories flat into `node_modules/`.
    /// Mode is per-install, not per-package — switching between
    /// modes leaves the opposite layout on disk so subsequent
    /// installs in the other mode reuse what's already there (and
    /// pay the materialization cost once).
    pub(crate) node_linker: NodeLinker,
    /// pnpm's `modules-dir`: the *project-level* directory that holds
    /// the top-level `<name>` entries the user sees under the project
    /// root. Defaults to `"node_modules"`, which is also what Node.js
    /// itself expects for the walk from `<project>/src/file.js` up to
    /// the project root. The virtual-store tree under
    /// `<modules_dir>/.aube/<dep_path>/node_modules/<name>` keeps its
    /// inner `node_modules/` name literal — Node requires the exact
    /// string `node_modules` when resolving sibling deps from inside a
    /// package — so this setting only affects the *outer* directory
    /// name, matching pnpm's behavior. Users who change it are
    /// responsible for setting `NODE_PATH` (or using a custom
    /// resolver) so Node can still find their deps.
    pub(crate) modules_dir_name: String,
    /// pnpm's `virtual-store-dir`: absolute path of the per-project
    /// virtual store (what pnpm calls `node_modules/.pnpm`). `None`
    /// means "derive from `modules_dir_name` at link time":
    /// `<project_dir>/<modules_dir_name>/.aube`, matching the default
    /// behavior every caller expected before this knob existed. When
    /// set by the install driver via `with_aube_dir_override`, it
    /// overrides that derivation — the linker writes its
    /// `.aube/<dep_path>/` tree into the supplied path instead. The
    /// path is *absolute*; relative overrides from `.npmrc` /
    /// `pnpm-workspace.yaml` get resolved against the project dir by
    /// the caller (see
    /// `aube_cli::commands::resolve_virtual_store_dir`).
    pub(crate) aube_dir_override: Option<std::path::PathBuf>,
    /// Cap for package-level filesystem materialization/linking work.
    /// This is deliberately separate from Rayon's global thread-count
    /// environment: aube is tuning metadata/syscall pressure, not CPU
    /// parallelism. Defaults are platform-aware and can be overridden by
    /// the install driver via the `linkConcurrency` setting.
    link_concurrency: Option<usize>,
    /// pnpm's `virtual-store-only`: when true, the linker still
    /// populates `.aube/<dep_path>/node_modules/<name>` (and, in
    /// global-store mode, the shared virtual store under
    /// `~/.cache/aube/virtual-store/`), but skips the final pass that
    /// creates the top-level `node_modules/<name>` symlinks. The
    /// `shamefullyHoist` and `publicHoistPattern` hoist passes are
    /// also skipped because both target the same top-level directory.
    /// Useful for CI jobs that pre-populate a shared store without
    /// exposing the graph to Node's resolver. No-op under
    /// `NodeLinker::Hoisted` — that layout *is* a flat top-level
    /// materialization, so "only the virtual store" doesn't apply.
    virtual_store_only: bool,
}

/// Patches to apply at materialize time, keyed by `name@version`. Each
/// value is the raw multi-file unified diff text written by `aube
/// patch-commit` (or any compatible tool).
pub type Patches = std::collections::BTreeMap<String, String>;

fn default_linker_parallelism() -> usize {
    let default_limit = if cfg!(target_os = "macos") { 4 } else { 16 };

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(default_limit)
}

fn with_link_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("aube-linker-{i}"))
        .build()
    {
        Ok(pool) => pool.install(f),
        Err(err) => {
            warn!("failed to build aube linker thread pool: {err}; falling back to caller thread");
            f()
        }
    }
}

/// Strategy for linking files from the store to node_modules.
#[derive(Debug, Clone, Copy)]
pub enum LinkStrategy {
    /// Copy-on-write (APFS clonefile, btrfs reflink)
    Reflink,
    /// Hard link (ext4, NTFS)
    Hardlink,
    /// Full copy (fallback)
    Copy,
}

impl Linker {
    pub fn new(store: &Store, strategy: LinkStrategy) -> Self {
        // Disable global virtual store in CI (cold cache makes it slower)
        let use_global_virtual_store = std::env::var("CI").is_err();
        Self {
            virtual_store: store.virtual_store_dir(),
            store: store.clone(),
            use_global_virtual_store,
            strategy,
            patches: Patches::new(),
            hashes: None,
            virtual_store_dir_max_length: DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            shamefully_hoist: false,
            public_hoist_patterns: Vec::new(),
            public_hoist_negations: Vec::new(),
            hoist: true,
            hoist_patterns: vec![glob::Pattern::new("*").expect("'*' is a valid glob pattern")],
            hoist_negations: Vec::new(),
            hoist_workspace_packages: true,
            dedupe_direct_deps: false,
            node_linker: NodeLinker::Isolated,
            link_concurrency: None,
            virtual_store_only: false,
            modules_dir_name: "node_modules".to_string(),
            aube_dir_override: None,
        }
    }

    /// Select the layout mode. Defaults to `NodeLinker::Isolated`
    /// (pnpm's `.aube/`-backed virtual-store layout); `Hoisted`
    /// dispatches `link_all` / `link_workspace` to the flat
    /// node_modules materializer in `crate::hoisted`.
    pub fn with_node_linker(mut self, node_linker: NodeLinker) -> Self {
        self.node_linker = node_linker;
        self
    }

    /// Current layout mode. The install driver reads this after
    /// linking to decide how to resolve per-package directories for
    /// bin linking and lifecycle scripts — isolated uses the
    /// `.aube/<dep_path>` convention, hoisted consults the
    /// `HoistedPlacements` returned on `LinkStats`.
    pub fn node_linker(&self) -> NodeLinker {
        self.node_linker
    }

    /// Override the name of the project-level `node_modules` directory
    /// (pnpm's `modules-dir` setting). Empty strings are coerced back
    /// to the default so a `.npmrc` typo can't make the linker write
    /// into the project root itself. The setting only affects the
    /// outer directory name — the inner virtual-store layout still
    /// uses the literal `node_modules` that Node's resolver expects
    /// when walking up from inside a package.
    pub fn with_modules_dir_name(mut self, name: impl Into<String>) -> Self {
        let s = name.into();
        self.modules_dir_name = if s.trim().is_empty() {
            "node_modules".to_string()
        } else {
            s
        };
        self
    }

    /// Project-level modules directory name. `aube` reads this
    /// when it needs the same path the linker writes into — keeping
    /// the computation DRY with whatever the linker was built with.
    pub fn modules_dir_name(&self) -> &str {
        &self.modules_dir_name
    }

    /// Override the per-project virtual-store path (pnpm's
    /// `virtualStoreDir`). The supplied path should be *absolute* —
    /// `aube` resolves relative `.npmrc` / `pnpm-workspace.yaml`
    /// values against the project dir before handing them here.
    /// When not set, the linker derives the virtual store path as
    /// `<project_dir>/<modules_dir_name>/.aube` at link time, which
    /// matches the historical behavior.
    pub fn with_aube_dir_override(mut self, path: std::path::PathBuf) -> Self {
        self.aube_dir_override = Some(path);
        self
    }

    /// Compute the effective `.aube/` path for `project_dir`.
    /// Consults the override installed by `with_aube_dir_override` if
    /// any; otherwise falls back to `<project_dir>/<modules_dir>/.aube`.
    /// Used internally by `link_all`; also called by the install
    /// driver's "already linked" fast path so both sites land on the
    /// same directory when the user has overridden `virtualStoreDir`.
    pub fn aube_dir_for(&self, project_dir: &Path) -> std::path::PathBuf {
        self.aube_dir_override
            .clone()
            .unwrap_or_else(|| project_dir.join(&self.modules_dir_name).join(".aube"))
    }

    /// Override the package-level linker worker count. Values below 1
    /// are ignored by the install driver before they reach this point.
    pub fn with_link_concurrency(mut self, concurrency: Option<usize>) -> Self {
        self.link_concurrency = concurrency;
        self
    }

    /// Override the global-virtual-store toggle set by `Linker::new`
    /// (which looks at `CI`). Callers use this to force per-project
    /// materialization when they've detected a consumer that breaks on
    /// directory symlinks escaping the project root — e.g. Next.js /
    /// Turbopack, which canonicalizes `node_modules/<pkg>` and rejects
    /// anything that lands outside its declared filesystem root.
    pub fn with_use_global_virtual_store(mut self, enabled: bool) -> Self {
        self.use_global_virtual_store = enabled;
        self
    }

    fn link_parallelism(&self) -> usize {
        self.link_concurrency
            .unwrap_or_else(default_linker_parallelism)
            .max(1)
    }

    /// Enable pnpm's `shamefully-hoist` mode. When true, every package
    /// in the graph gets a top-level `node_modules/<name>` symlink in
    /// addition to the direct-dep entries, producing npm's flat
    /// layout at the cost of phantom-dep correctness. First-write-wins
    /// on duplicate names, so root deps always take precedence.
    pub fn with_shamefully_hoist(mut self, shamefully_hoist: bool) -> Self {
        self.shamefully_hoist = shamefully_hoist;
        self
    }

    /// Configure pnpm's `public-hoist-pattern`. Each input is a glob
    /// matched against package names; a leading `!` flips it into a
    /// negation. After the usual direct-dep symlinks, every non-local
    /// package whose name matches at least one positive pattern and
    /// no negation gets a top-level `node_modules/<name>` symlink.
    /// Invalid patterns are silently dropped (same tolerance as
    /// pnpm), so a typo in `.npmrc` degrades to "not hoisted" instead
    /// of failing the install.
    pub fn with_public_hoist_pattern(mut self, patterns: &[String]) -> Self {
        for raw in patterns {
            let (neg, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw.as_str()),
            };
            let Ok(pat) = glob::Pattern::new(body) else {
                continue;
            };
            if neg {
                self.public_hoist_negations.push(pat);
            } else {
                self.public_hoist_patterns.push(pat);
            }
        }
        self
    }

    /// Toggle pnpm's `hoist` setting. When true (the default), the
    /// hidden modules tree at `node_modules/.aube/node_modules/` is
    /// populated via `with_hoist_pattern`. When false, that tree is
    /// skipped and any existing directory is swept so stale symlinks
    /// from a previous `hoist=true` run don't keep resolving.
    pub fn with_hoist(mut self, hoist: bool) -> Self {
        self.hoist = hoist;
        self
    }

    /// Configure pnpm's `hoist-pattern`. Each input is a glob matched
    /// against package names; a leading `!` flips it into a negation.
    /// Every non-local package in the graph whose name matches at
    /// least one positive pattern (and no negation) gets a
    /// `node_modules/.aube/node_modules/<name>` symlink — the hidden
    /// fallback dir for Node's parent-directory walk. Invalid
    /// patterns are silently dropped (pnpm parity). Supplying an
    /// empty list or only-negation list means "hoist nothing";
    /// leaving this unconfigured keeps the default `*` match.
    pub fn with_hoist_pattern(mut self, patterns: &[String]) -> Self {
        self.hoist_patterns.clear();
        self.hoist_negations.clear();
        for raw in patterns {
            let (neg, body) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw.as_str()),
            };
            let Ok(pat) = glob::Pattern::new(body) else {
                continue;
            };
            if neg {
                self.hoist_negations.push(pat);
            } else {
                self.hoist_patterns.push(pat);
            }
        }
        self
    }

    /// Toggle pnpm's `hoist-workspace-packages`. When false, the
    /// linker skips creating `node_modules/<ws-pkg>` symlinks for
    /// workspace packages in every importer, including the root.
    /// Cross-importer `workspace:` deps already resolve through the
    /// lockfile, so only direct `require('<ws-pkg>')` from a package
    /// that doesn't declare it stops working. Default true (pnpm
    /// parity).
    pub fn with_hoist_workspace_packages(mut self, on: bool) -> Self {
        self.hoist_workspace_packages = on;
        self
    }

    /// Toggle pnpm's `dedupe-direct-deps`. When true, the linker
    /// skips creating a per-importer `node_modules/<name>` symlink for
    /// any direct dep whose root importer already declares the same
    /// package at the same resolved version — Node's parent-directory
    /// walk from inside the workspace package still resolves the same
    /// copy via the root-level symlink, so consumer code is
    /// unaffected. Default false (pnpm parity). No-op under
    /// `virtualStoreOnly=true` (no per-importer symlink pass runs)
    /// and under `NodeLinker::Hoisted` (each importer gets an
    /// independent flat tree — no shared root to dedupe against).
    pub fn with_dedupe_direct_deps(mut self, on: bool) -> Self {
        self.dedupe_direct_deps = on;
        self
    }

    /// Whether `pkg_name` should be symlinked into the hidden hoist
    /// tree. Returns false when `hoist == false` regardless of
    /// patterns, or when no positive pattern matches. Matching is
    /// case-insensitive, matching pnpm.
    fn hoist_matches(&self, pkg_name: &str) -> bool {
        if !self.hoist {
            return false;
        }
        let opts = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        if !self
            .hoist_patterns
            .iter()
            .any(|p| p.matches_with(pkg_name, opts))
        {
            return false;
        }
        !self
            .hoist_negations
            .iter()
            .any(|p| p.matches_with(pkg_name, opts))
    }

    /// Whether `pkg_name` should be promoted to the root
    /// `node_modules` under the configured `public-hoist-pattern`.
    /// Names with no positive match are rejected; a name that
    /// matches a positive pattern is still rejected if any negation
    /// also matches. Matching is case-insensitive.
    fn public_hoist_matches(&self, pkg_name: &str) -> bool {
        if self.public_hoist_patterns.is_empty() {
            return false;
        }
        let opts = glob::MatchOptions {
            case_sensitive: false,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        if !self
            .public_hoist_patterns
            .iter()
            .any(|p| p.matches_with(pkg_name, opts))
        {
            return false;
        }
        !self
            .public_hoist_negations
            .iter()
            .any(|p| p.matches_with(pkg_name, opts))
    }

    /// Override the virtual-store directory name length cap. Primarily
    /// a hook for tests and for parity with pnpm's
    /// `virtual-store-dir-max-length` config; most callers should
    /// leave it at the default.
    pub fn with_virtual_store_dir_max_length(mut self, max_length: usize) -> Self {
        self.virtual_store_dir_max_length = max_length;
        self
    }

    /// Toggle pnpm's `virtual-store-only`. When enabled, `link_all` /
    /// `link_workspace` still populate `.aube/<dep_path>/node_modules`
    /// (and the shared global virtual store under
    /// `~/.cache/aube/virtual-store/`) but skip the pass that writes
    /// top-level `node_modules/<name>` symlinks and the hoisting
    /// passes that target the same directory. No-op under
    /// `NodeLinker::Hoisted` — that layout is inherently a flat
    /// top-level materialization.
    pub fn with_virtual_store_only(mut self, only: bool) -> Self {
        self.virtual_store_only = only;
        self
    }

    /// Whether this linker will skip the top-level `node_modules/<name>`
    /// symlink pass. Exposed so the install driver can omit root-level
    /// bin linking and lifecycle-script invocations when the user has
    /// asked for a virtual-store-only install — both operate on the
    /// top-level tree that won't exist.
    pub fn virtual_store_only(&self) -> bool {
        self.virtual_store_only
    }

    /// Install a set of pre-computed graph hashes. Every virtual-store
    /// path the linker constructs after this point will use the
    /// hashed subdir name for the matching `dep_path`. Callers
    /// normally derive the hashes once per install via
    /// `aube_lockfile::graph_hash::compute_graph_hashes` and pass the
    /// result in here.
    pub fn with_graph_hashes(mut self, hashes: GraphHashes) -> Self {
        self.hashes = Some(hashes);
        self
    }

    /// Directory name for `dep_path` inside the global virtual store.
    /// Applies the graph hash (if any) to fold in build state, then
    /// runs the result through `dep_path_to_filename` so the final
    /// name is both filesystem-safe and bounded.
    fn virtual_store_subdir(&self, dep_path: &str) -> String {
        let hashed = match &self.hashes {
            Some(h) => h.hashed_dep_path(dep_path),
            None => dep_path.to_string(),
        };
        dep_path_to_filename(&hashed, self.virtual_store_dir_max_length)
    }

    /// Directory name for `dep_path` inside a project's local
    /// `node_modules/.aube/`. Same filename-bounding as the global
    /// store, but without the graph-hash fold — local `.aube/` is
    /// keyed by dep_path alone because node's resolver walks by
    /// dep_path and never inspects the shared-store identity.
    fn aube_dir_entry_name(&self, dep_path: &str) -> String {
        dep_path_to_filename(dep_path, self.virtual_store_dir_max_length)
    }

    /// Whether this linker populates the project's `.aube/` entries as
    /// symlinks into the shared virtual store (true) or materializes a
    /// per-project copy (false). Callers that want to mutate package
    /// directories after linking — e.g. running allowBuilds lifecycle
    /// scripts — need to know because shared-store writes leak across
    /// projects.
    pub fn uses_global_virtual_store(&self) -> bool {
        self.use_global_virtual_store
    }

    #[cfg(test)]
    fn new_with_gvs(store: &Store, strategy: LinkStrategy, use_global_virtual_store: bool) -> Self {
        Self {
            virtual_store: store.virtual_store_dir(),
            store: store.clone(),
            use_global_virtual_store,
            strategy,
            patches: Patches::new(),
            hashes: None,
            virtual_store_dir_max_length: DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            shamefully_hoist: false,
            public_hoist_patterns: Vec::new(),
            public_hoist_negations: Vec::new(),
            hoist: true,
            hoist_patterns: vec![glob::Pattern::new("*").expect("'*' is a valid glob pattern")],
            hoist_negations: Vec::new(),
            hoist_workspace_packages: true,
            dedupe_direct_deps: false,
            node_linker: NodeLinker::Isolated,
            link_concurrency: None,
            virtual_store_only: false,
            modules_dir_name: "node_modules".to_string(),
            aube_dir_override: None,
        }
    }

    /// Install a set of patch contents to apply at materialize time.
    /// Replaces any previously installed patches. Pair with
    /// `with_graph_hashes` whose `patch_hash` callback returns the same
    /// per-`(name, version)` digest, so the patched bytes land at a
    /// distinct virtual-store path from the unpatched ones.
    pub fn with_patches(mut self, patches: Patches) -> Self {
        self.patches = patches;
        self
    }

    /// Detect the best linking strategy for the filesystem at the given path.
    pub fn detect_strategy(path: &Path) -> LinkStrategy {
        let test_src = path.join(".aube-link-test-src");
        let test_dst = path.join(".aube-link-test-dst");

        if std::fs::write(&test_src, b"test").is_ok() {
            if reflink_copy::reflink(&test_src, &test_dst).is_ok() {
                let _ = std::fs::remove_file(&test_src);
                let _ = std::fs::remove_file(&test_dst);
                return LinkStrategy::Reflink;
            }

            let _ = std::fs::remove_file(&test_dst);
            if std::fs::hard_link(&test_src, &test_dst).is_ok() {
                let _ = std::fs::remove_file(&test_src);
                let _ = std::fs::remove_file(&test_dst);
                return LinkStrategy::Hardlink;
            }

            let _ = std::fs::remove_file(&test_src);
            let _ = std::fs::remove_file(&test_dst);
        }

        LinkStrategy::Copy
    }

    /// Link all packages into node_modules for the given project.
    pub fn link_all(
        &self,
        project_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
    ) -> Result<LinkStats, Error> {
        if matches!(self.node_linker, NodeLinker::Hoisted) {
            let mut stats = LinkStats::default();
            let mut placements = HoistedPlacements::default();
            hoisted::link_hoisted_importer(
                self,
                project_dir,
                graph.root_deps(),
                graph,
                package_indices,
                &mut stats,
                &mut placements,
            )?;
            // Hoisted mode doesn't use the isolated `.aube/` virtual
            // store, so a hidden hoist tree under `.aube/node_modules/`
            // has no consumer. If a previous isolated install left one
            // behind, sweep it — hoisted's top-level cleanup preserves
            // dotfiles, so it wouldn't be removed otherwise, and a
            // stale tree would keep satisfying phantom deps for any
            // leftover `.aube/<dep_path>/` directories until their
            // eventual cleanup. Honors `virtualStoreDir`.
            let _ = std::fs::remove_dir_all(self.aube_dir_for(project_dir).join("node_modules"));
            stats.hoisted_placements = Some(placements);
            return Ok(stats);
        }

        let nm = project_dir.join(&self.modules_dir_name);
        let aube_dir = self.aube_dir_for(project_dir);

        xx::file::mkdirp(&aube_dir).map_err(|e| Error::Xx(e.to_string()))?;

        // Clean up stale top-level entries not in the current graph.
        // With shamefully_hoist, every package name in the graph is
        // also a legitimate top-level entry, so fold those into the
        // preserve set before sweeping. Scoped packages live under
        // `node_modules/@scope/<pkg>`, but `read_dir` on `node_modules`
        // yields the bare `@scope` directory — so we build a second
        // set of scope prefixes and preserve any entry that matches.
        let mut root_dep_names: std::collections::HashSet<&str> =
            graph.root_deps().iter().map(|d| d.name.as_str()).collect();
        if self.shamefully_hoist {
            for pkg in graph.packages.values() {
                root_dep_names.insert(pkg.name.as_str());
            }
        } else if !self.public_hoist_patterns.is_empty() {
            for pkg in graph.packages.values() {
                if pkg.local_source.is_none() && self.public_hoist_matches(&pkg.name) {
                    root_dep_names.insert(pkg.name.as_str());
                }
            }
        }
        let scope_prefixes: std::collections::HashSet<&str> = root_dep_names
            .iter()
            .filter_map(|n| n.split_once('/').map(|(scope, _)| scope))
            .collect();
        // Preserve the virtual-store leaf name when `aube_dir` sits
        // directly under `nm`. With the default `.aube` the dotfile
        // check below covers it, but a user who sets
        // `virtualStoreDir=node_modules/vstore` would otherwise see
        // the sweep delete the freshly-`mkdirp`d virtual store on
        // every install because `vstore` isn't a dotfile and isn't
        // in `root_dep_names`.
        let aube_dir_leaf: Option<std::ffi::OsString> = if aube_dir.parent() == Some(nm.as_path()) {
            aube_dir.file_name().map(|s| s.to_owned())
        } else {
            None
        };
        if let Ok(entries) = std::fs::read_dir(&nm) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                // Skip .aube, .bin, and other dotfiles
                if name_str.starts_with('.') {
                    continue;
                }
                if aube_dir_leaf.as_deref() == Some(name.as_os_str()) {
                    continue;
                }
                if root_dep_names.contains(name_str.as_ref()) {
                    continue;
                }
                // Preserved `@scope` directory: keep the directory
                // itself, but recurse and drop any `@scope/<pkg>`
                // whose full name is no longer in the graph. Without
                // this, a scoped dep removed from package.json would
                // linger as a phantom symlink (its `.aube/` entry
                // also persists) and still resolve successfully.
                if scope_prefixes.contains(name_str.as_ref()) {
                    let scope_dir = entry.path();
                    if let Ok(inner) = std::fs::read_dir(&scope_dir) {
                        for inner_entry in inner.flatten() {
                            let inner_name = inner_entry.file_name();
                            let full = format!("{}/{}", name_str, inner_name.to_string_lossy());
                            if !root_dep_names.contains(full.as_str()) {
                                let _ = std::fs::remove_dir_all(inner_entry.path());
                                let _ = std::fs::remove_file(inner_entry.path());
                            }
                        }
                    }
                    continue;
                }
                let _ = std::fs::remove_dir_all(entry.path());
                let _ = std::fs::remove_file(entry.path());
            }
        }

        let mut stats = LinkStats::default();

        // Reconcile previously-applied patches against the current
        // `self.patches` set. Without graph hashes (CI / no-global-store
        // mode) the `.aube/<dep_path>` directory name doesn't change
        // when a patch is added or removed, so the simple "exists?
        // skip!" check would otherwise leave stale patched bytes in
        // place after `aube patch-remove` or fail to apply a brand new
        // patch after `aube patch-commit`. We track the per-`(name,
        // version)` patch fingerprint in a sidecar file under
        // `node_modules/` and wipe the matching `.aube/<dep_path>`
        // entries whenever the fingerprint changes.
        let prev_applied = read_applied_patches(&nm);
        let curr_applied = current_patch_hashes(&self.patches);
        if !self.use_global_virtual_store {
            wipe_changed_patched_entries(
                &aube_dir,
                graph,
                &prev_applied,
                &curr_applied,
                self.virtual_store_dir_max_length,
            );
        }

        // Step 1: Populate .aube virtual store
        //
        // Local packages (file:/link:) never go into the shared global
        // virtual store — their source is project-specific, so we
        // materialize them straight into per-project `.aube/` below.
        // `link:` entries don't need any `.aube/` entry at all; their
        // top-level symlink points directly at the target.
        for (dep_path, pkg) in &graph.packages {
            let Some(ref local) = pkg.local_source else {
                continue;
            };
            if matches!(local, LocalSource::Link(_)) {
                continue;
            }
            let Some(index) = package_indices.get(dep_path) else {
                continue;
            };
            let aube_entry = aube_dir.join(dep_path);
            if !aube_entry.exists() {
                self.materialize_into(&aube_dir, dep_path, pkg, index, &mut stats, false)?;
            } else {
                stats.packages_cached += 1;
            }
        }

        if self.use_global_virtual_store {
            use rayon::prelude::*;

            let link_parallelism = self.link_parallelism();
            let step1_timer = std::time::Instant::now();
            let step1_results: Vec<Result<LinkStats, Error>> =
                with_link_pool(link_parallelism, || {
                    graph
                        .packages
                        .par_iter()
                        .filter_map(|(dep_path, pkg)| {
                            if pkg.local_source.is_some() {
                                return None;
                            }
                            Some((dep_path, pkg))
                        })
                        .map(|(dep_path, pkg)| {
                            let mut local_stats = LinkStats::default();
                            let local_aube_entry =
                                aube_dir.join(self.aube_dir_entry_name(dep_path));
                            let global_entry =
                                self.virtual_store.join(self.virtual_store_subdir(dep_path));

                            // Single readlink classifies the entry into one of
                            // three states and drives the whole per-package
                            // decision tree below. Avoids the double-check
                            // (`read_link` then `exists`) the previous version
                            // did and eliminates the unconditional
                            // `remove_dir`/`remove_file` pair on cold installs,
                            // which strace showed as ~1.4k ENOENT syscalls per
                            // install on the medium fixture.
                            enum EntryState {
                                /// `.aube/<dep_path>` already points at the
                                /// current hashed global entry — nothing to do.
                                Fresh,
                                /// `.aube/<dep_path>` doesn't exist yet. We need
                                /// to materialize + create the symlink, but
                                /// there's nothing to remove first.
                                Missing,
                                /// `.aube/<dep_path>` exists and points somewhere
                                /// else (stale hash, patch change, etc.). Must
                                /// unlink before resymlinking.
                                Stale,
                            }
                            let state = match std::fs::read_link(&local_aube_entry) {
                                Ok(existing) if existing == global_entry => {
                                    // Verify the target actually exists — a
                                    // dangling link needs to fall through to
                                    // the materialize path.
                                    if local_aube_entry.exists() {
                                        EntryState::Fresh
                                    } else {
                                        EntryState::Stale
                                    }
                                }
                                Ok(_) => EntryState::Stale,
                                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                    EntryState::Missing
                                }
                                // Some other error (permission, etc.): treat as
                                // Stale and let the removal/recreate path try
                                // its best-effort cleanup + surface the real
                                // error on symlink creation if unlucky.
                                Err(_) => EntryState::Stale,
                            };

                            if matches!(state, EntryState::Fresh) {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }

                            // Symlink is stale or missing — need the package
                            // index to (re)materialize. The install driver
                            // omits `package_indices` entries for packages on
                            // the fast path; load from the store on demand if
                            // this one slipped through. This keeps the
                            // fast-path safe against graph-hash changes that
                            // invalidate the symlink target (patches, engine
                            // bumps, `allowBuilds` flips).
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(pkg.registry_name(), &pkg.version)
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.ensure_in_virtual_store(dep_path, pkg, index, &mut local_stats)?;

                            // Only pay the `remove_dir`/`remove_file` syscalls
                            // when we actually have something to remove.
                            // On Windows, `.aube/<dep_path>` is an NTFS
                            // junction (created via `sys::create_dir_link`);
                            // `remove_file` can't unlink those, so try
                            // `remove_dir` first and fall back to
                            // `remove_file` for the unix case (where
                            // `symlink` produces a file-style link).
                            if matches!(state, EntryState::Stale) {
                                let _ = std::fs::remove_dir(&local_aube_entry)
                                    .or_else(|_| std::fs::remove_file(&local_aube_entry));
                            }
                            if let Some(parent) = local_aube_entry.parent() {
                                xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                            }
                            sys::create_dir_link(&global_entry, &local_aube_entry)
                                .map_err(|e| Error::Io(local_aube_entry.clone(), e))?;
                            Ok(local_stats)
                        })
                        .collect()
                });

            for result in step1_results {
                let local_stats = result?;
                stats.packages_linked += local_stats.packages_linked;
                stats.packages_cached += local_stats.packages_cached;
                stats.files_linked += local_stats.files_linked;
            }
            log::debug!("link:step1 (gvs populate) {:.1?}", step1_timer.elapsed());
        } else {
            // `wipe_changed_patched_entries` above already removed any
            // `.aube/<dep_path>` whose patch fingerprint changed since
            // the last install, so the existence check below will fall
            // through to `materialize_into` for those packages and
            // pick up the current patch state.
            for (dep_path, pkg) in &graph.packages {
                if pkg.local_source.is_some() {
                    continue;
                }
                let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                if aube_entry.exists() {
                    // Already in place from a previous run — count as
                    // cached. `install.rs` deliberately omits this
                    // dep_path from `package_indices` on the fast
                    // path, so do the existence check first.
                    stats.packages_cached += 1;
                    continue;
                }
                // Entry missing — load the index. Fast path in
                // `install.rs` skips `load_index` when `aube_entry`
                // already exists; lazy-load here for the case where a
                // patch / allowBuilds change invalidated the entry
                // since.
                let owned_index;
                let index = match package_indices.get(dep_path) {
                    Some(idx) => idx,
                    None => {
                        owned_index = self
                            .store
                            .load_index(pkg.registry_name(), &pkg.version)
                            .ok_or_else(|| Error::MissingPackageIndex(dep_path.to_string()))?;
                        &owned_index
                    }
                };
                self.materialize_into(&aube_dir, dep_path, pkg, index, &mut stats, false)?;
            }
        }

        // `virtualStoreOnly=true` skips Steps 2 + 3 — the
        // user-visible top-level `node_modules/<name>` symlinks and
        // the hoisting passes that target the same directory — but
        // Step 4 (the hidden `.aube/node_modules/` hoist) still runs
        // because that tree lives *inside* the virtual store and
        // packages walking up for undeclared deps need it. Anything
        // that walks the user-visible root tree (bin linking,
        // lifecycle scripts, the state sidecar) is the install
        // driver's responsibility to skip in this mode.
        if self.virtual_store_only {
            self.link_hidden_hoist(&aube_dir, graph)?;
            write_applied_patches(&nm, &curr_applied);
            return Ok(stats);
        }

        // Step 2: Create top-level entries as symlinks into .aube.
        // The .aube/<dep_path>/node_modules/ directory already contains the
        // package and sibling symlinks to its direct deps (set up by
        // materialize_into / ensure_in_virtual_store), so a single symlink at
        // node_modules/<name> gives Node everything it needs to resolve
        // transitive deps via its normal directory walk.
        use rayon::prelude::*;

        let root_deps: Vec<_> = graph.root_deps().to_vec();
        let link_parallelism = self.link_parallelism();
        let step2_timer = std::time::Instant::now();
        let results: Vec<Result<bool, Error>> = with_link_pool(link_parallelism, || {
            root_deps
                .par_iter()
                .map(|dep| {
                    let target_dir = nm.join(&dep.name);

                    // Already in place from a previous run — don't recount.
                    // A *broken* symlink (target no longer exists) is left
                    // over from a stale install and must be reclaimed so we
                    // fall through and recreate it below.
                    if keep_or_reclaim_broken_symlink(&target_dir)? {
                        return Ok(false);
                    }

                    // `link:` direct deps point at the on-disk target with
                    // a plain symlink, bypassing `.aube/` entirely.
                    if let Some(pkg) = graph.packages.get(&dep.dep_path)
                        && let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref()
                    {
                        let abs_target = project_dir.join(rel);
                        if let Some(parent) = target_dir.parent() {
                            xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                        }
                        let link_parent = target_dir.parent().unwrap_or(&nm);
                        let rel_target =
                            pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                        sys::create_dir_link(&rel_target, &target_dir)
                            .map_err(|e| Error::Io(target_dir.clone(), e))?;
                        return Ok(true);
                    }

                    // Verify the source actually exists in .aube before symlinking
                    let source_dir = aube_dir
                        .join(self.aube_dir_entry_name(&dep.dep_path))
                        .join("node_modules")
                        .join(&dep.name);
                    if !source_dir.exists() {
                        return Ok(false);
                    }

                    // Symlink target is relative to node_modules/<name>'s parent.
                    // For non-scoped packages the parent is node_modules/, but for
                    // scoped packages (e.g. @scope/name) it is node_modules/@scope/,
                    // so we must compute the relative path dynamically.
                    if let Some(parent) = target_dir.parent() {
                        xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                    }
                    let link_parent = target_dir.parent().unwrap_or(&nm);
                    let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                        .unwrap_or_else(|| source_dir.clone());

                    sys::create_dir_link(&rel_target, &target_dir)
                        .map_err(|e| Error::Io(target_dir.clone(), e))?;

                    trace!("top-level: {}", dep.name);
                    Ok(true)
                })
                .collect()
        });

        for result in results {
            if result? {
                stats.top_level_linked += 1;
            }
        }
        log::debug!(
            "link:step2 (top-level symlinks) {:.1?}",
            step2_timer.elapsed()
        );

        // Step 3: public-hoist-pattern matches get surfaced to the
        // root first, then shamefully_hoist (if enabled) sweeps up
        // everything else. Both use first-write-wins so direct deps
        // keep their symlinks and the pattern-matched names take
        // precedence over the bulk hoist.
        if !self.public_hoist_patterns.is_empty() {
            self.hoist_remaining_into(
                &nm,
                &aube_dir,
                graph,
                &mut stats,
                "public-hoist",
                &|name| self.public_hoist_matches(name),
            )?;
        }
        if self.shamefully_hoist {
            self.hoist_remaining_into(&nm, &aube_dir, graph, &mut stats, "hoist", &|_| true)?;
        }

        // Step 4: populate (or sweep) the hidden modules tree under
        // `.aube/node_modules/`. This runs regardless of the root
        // hoist passes above — it targets a different consumer
        // (packages inside the virtual store walking up for
        // undeclared deps) and wouldn't interact with the
        // root-level symlinks even on name clashes.
        self.link_hidden_hoist(&aube_dir, graph)?;

        write_applied_patches(&nm, &curr_applied);
        Ok(stats)
    }

    /// Hoisted-mode workspace linker. Runs the per-importer
    /// hoisted planner once per importer in the graph, accumulating
    /// stats + placements into a single `LinkStats`. Each importer
    /// gets its own independent flat tree (no shared root
    /// virtual-store like the isolated layout), matching npm
    /// workspaces and what hoisted-mode toolchains expect: a
    /// self-contained `node_modules/` under every importer.
    fn link_workspace_hoisted(
        &self,
        root_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
        workspace_dirs: &BTreeMap<String, PathBuf>,
    ) -> Result<LinkStats, Error> {
        let mut stats = LinkStats::default();
        let mut placements = HoistedPlacements::default();
        for (importer_path, deps) in &graph.importers {
            let importer_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                root_dir.join(importer_path)
            };
            // Workspace deps resolve through `workspace_dirs` rather
            // than going through the placement tree, so the hoisted
            // planner shouldn't try to copy their contents. Filter
            // them out of the seed set — we'll symlink them in a
            // post-pass below.
            let planner_deps: Vec<aube_lockfile::DirectDep> = deps
                .iter()
                .filter(|d| !workspace_dirs.contains_key(&d.name))
                .cloned()
                .collect();
            hoisted::link_hoisted_importer(
                self,
                &importer_dir,
                &planner_deps,
                graph,
                package_indices,
                &mut stats,
                &mut placements,
            )?;

            // Drop workspace deps in as symlinks, same as isolated mode.
            let nm = importer_dir.join(&self.modules_dir_name);
            if !self.hoist_workspace_packages {
                continue;
            }
            for dep in deps {
                let Some(ws_dir) = workspace_dirs.get(&dep.name) else {
                    continue;
                };
                let link_path = nm.join(&dep.name);
                if let Some(parent) = link_path.parent() {
                    xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                }
                let _ = std::fs::remove_dir_all(&link_path);
                let _ = std::fs::remove_file(&link_path);
                let link_parent = link_path.parent().unwrap_or(&nm);
                let target = pathdiff::diff_paths(ws_dir, link_parent).unwrap_or(ws_dir.clone());
                sys::create_dir_link(&target, &link_path)
                    .map_err(|e| Error::Io(link_path.clone(), e))?;
                stats.top_level_linked += 1;
            }
        }
        // Same rationale as the non-workspace hoisted path: sweep any
        // `.aube/node_modules/` left behind by a prior isolated
        // install so hoisted's dotfile-preserving cleanup doesn't
        // leak a stale hidden tree. Honors `virtualStoreDir`.
        let _ = std::fs::remove_dir_all(self.aube_dir_for(root_dir).join("node_modules"));
        stats.hoisted_placements = Some(placements);
        Ok(stats)
    }

    /// Link all packages for a workspace (multiple importers).
    ///
    /// Creates the shared `.aube/` virtual store at root, then for each workspace
    /// package creates `node_modules/` with its direct deps linked from the root `.aube/`.
    /// Workspace packages that depend on each other get symlinks to the package directory.
    pub fn link_workspace(
        &self,
        root_dir: &Path,
        graph: &LockfileGraph,
        package_indices: &BTreeMap<String, PackageIndex>,
        workspace_dirs: &BTreeMap<String, PathBuf>,
    ) -> Result<LinkStats, Error> {
        if matches!(self.node_linker, NodeLinker::Hoisted) {
            return self.link_workspace_hoisted(root_dir, graph, package_indices, workspace_dirs);
        }

        let root_nm = root_dir.join(&self.modules_dir_name);
        let aube_dir = self.aube_dir_for(root_dir);

        if root_nm.exists() {
            debug!("removing existing root node_modules");
            xx::file::remove_dir_all(&root_nm).map_err(|e| Error::Xx(e.to_string()))?;
        }
        // When `virtualStoreDir` points outside `root_nm` (custom
        // override), wiping `root_nm` won't have cleared it, so
        // workspace installs would otherwise carry stale entries
        // across re-links. Removing before `mkdirp` gives the same
        // clean-slate guarantee the default-path wipe provides.
        if aube_dir.exists() && !aube_dir.starts_with(&root_nm) {
            xx::file::remove_dir_all(&aube_dir).map_err(|e| Error::Xx(e.to_string()))?;
        }
        xx::file::mkdirp(&aube_dir).map_err(|e| Error::Xx(e.to_string()))?;
        // Recreate `root_nm` explicitly. With the default layout this is
        // a no-op (`mkdirp(aube_dir)` created it as an ancestor); with a
        // custom `virtualStoreDir` outside `node_modules/`, the later
        // `create_dir_link` calls below need `root_nm` to exist up front
        // — there's no other site that mkdirps it.
        xx::file::mkdirp(&root_nm).map_err(|e| Error::Xx(e.to_string()))?;

        let mut stats = LinkStats::default();

        // Step 1a: Materialize local (`file:` dir/tarball) packages
        // straight into the shared per-project `.aube/`. They never
        // participate in the global virtual store since their source
        // is project-specific. `link:` deps get no `.aube/` entry at
        // all — step 2 symlinks directly to the target.
        for (dep_path, pkg) in &graph.packages {
            let Some(ref local) = pkg.local_source else {
                continue;
            };
            if matches!(local, LocalSource::Link(_)) {
                continue;
            }
            let Some(index) = package_indices.get(dep_path) else {
                continue;
            };
            self.materialize_into(&aube_dir, dep_path, pkg, index, &mut stats, false)?;
        }

        // Step 1b: Populate shared .aube virtual store at root for
        // registry packages.
        for (dep_path, pkg) in &graph.packages {
            if pkg.local_source.is_some() {
                continue;
            }
            // Workspace installs always wipe `root_nm` above, so the
            // fetch phase's "already linked" fast path is invalid
            // here — `.aube/<dep_path>` was just deleted. Lazy-load
            // the index from the store on demand when the sparse
            // `package_indices` map doesn't cover this dep_path.
            let owned_index;
            let index = match package_indices.get(dep_path) {
                Some(idx) => idx,
                None => {
                    let Some(idx) = self.store.load_index(pkg.registry_name(), &pkg.version) else {
                        return Err(Error::MissingPackageIndex(dep_path.to_string()));
                    };
                    owned_index = idx;
                    &owned_index
                }
            };

            if self.use_global_virtual_store {
                self.ensure_in_virtual_store(dep_path, pkg, index, &mut stats)?;

                let local_aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                let global_entry = self.virtual_store.join(self.virtual_store_subdir(dep_path));

                if let Some(parent) = local_aube_entry.parent() {
                    xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                }
                sys::create_dir_link(&global_entry, &local_aube_entry)
                    .map_err(|e| Error::Io(local_aube_entry.clone(), e))?;
            } else {
                self.materialize_into(&aube_dir, dep_path, pkg, index, &mut stats, false)?;
            }
        }

        // `virtualStoreOnly=true` skips per-importer node_modules
        // population and the root-level hoisting passes, but the
        // hidden `.aube/node_modules/` hoist (Step 4 below) still
        // runs because it lives *inside* the virtual store. Bin
        // linking and lifecycle scripts for the top-level importers
        // are the install driver's responsibility to skip in this
        // mode.
        if self.virtual_store_only {
            self.link_hidden_hoist(&aube_dir, graph)?;
            return Ok(stats);
        }

        // Precompute root importer's direct deps keyed by name so the
        // per-importer loop below can short-circuit on `dedupeDirectDeps`
        // without walking the root's dep list for every child entry.
        // Empty when the root has no direct deps (lockfile-only workspaces)
        // or when `dedupeDirectDeps=false` — skipping the build on the
        // common path avoids an allocation the per-dep check would
        // never consult.
        let root_deps_by_name: std::collections::HashMap<&str, &aube_lockfile::DirectDep> =
            if self.dedupe_direct_deps {
                graph
                    .importers
                    .get(".")
                    .map(|deps| deps.iter().map(|d| (d.name.as_str(), d)).collect())
                    .unwrap_or_default()
            } else {
                std::collections::HashMap::new()
            };

        // Step 2: For each importer, create node_modules with its direct deps
        for (importer_path, deps) in &graph.importers {
            let pkg_dir = if importer_path == "." {
                root_dir.to_path_buf()
            } else {
                root_dir.join(importer_path)
            };

            let nm = pkg_dir.join(&self.modules_dir_name);
            if importer_path != "." && nm.exists() {
                xx::file::remove_dir_all(&nm).map_err(|e| Error::Xx(e.to_string()))?;
            }
            if importer_path != "." {
                xx::file::mkdirp(&nm).map_err(|e| Error::Xx(e.to_string()))?;
            }

            for dep in deps {
                // `dedupeDirectDeps`: when a non-root importer declares
                // the same package the workspace root does and they
                // resolve to the same `dep_path` (which captures both
                // version and peer context), skip the per-importer
                // symlink. Node's parent-directory walk from inside
                // the workspace package will reach the root-level
                // symlink and resolve the same copy. Only applies to
                // non-root importers — the root is the source of truth.
                if self.dedupe_direct_deps
                    && importer_path != "."
                    && let Some(root_dep) = root_deps_by_name.get(dep.name.as_str())
                    && root_dep.dep_path == dep.dep_path
                {
                    continue;
                }

                // Check if this dep is a workspace package
                if let Some(ws_dir) = workspace_dirs.get(&dep.name) {
                    // `hoist-workspace-packages=false` suppresses
                    // the `node_modules/<ws-pkg>` symlink. The
                    // workspace graph still records the dep, so a
                    // cross-importer `workspace:` resolution keeps
                    // working — only a plain top-level import of
                    // the workspace package stops resolving.
                    if !self.hoist_workspace_packages {
                        continue;
                    }
                    // Symlink to the workspace package directory
                    let link_path = nm.join(&dep.name);
                    // Compute relative path from the symlink's parent (handles scoped packages)
                    let link_parent = link_path.parent().unwrap_or(&nm);
                    let target =
                        pathdiff::diff_paths(ws_dir, link_parent).unwrap_or(ws_dir.clone());

                    if let Some(parent) = link_path.parent() {
                        xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                    }

                    sys::create_dir_link(&target, &link_path)
                        .map_err(|e| Error::Io(link_path.clone(), e))?;

                    stats.top_level_linked += 1;
                    continue;
                }

                // `link:` direct deps point straight at the on-disk
                // target. The resolver's `rebase_local` normalizes
                // the path to be relative to the workspace root
                // (`root_dir`), not the importer's directory, so the
                // absolute target is always `root_dir.join(rel)`
                // regardless of which importer declared the dep.
                if let Some(locked) = graph.packages.get(&dep.dep_path)
                    && let Some(LocalSource::Link(rel)) = locked.local_source.as_ref()
                {
                    let abs_target = root_dir.join(rel);
                    let link_path = nm.join(&dep.name);
                    if let Some(parent) = link_path.parent() {
                        xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                    }
                    let link_parent = link_path.parent().unwrap_or(&nm);
                    let rel_target =
                        pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                    sys::create_dir_link(&rel_target, &link_path)
                        .map_err(|e| Error::Io(link_path.clone(), e))?;
                    stats.top_level_linked += 1;
                    continue;
                }

                // Regular dep — symlink from importer's node_modules to root .aube.
                // The .aube/<dep_path>/node_modules/ already contains sibling
                // symlinks to transitive deps (set up by materialize_into).
                let source_dir = aube_dir
                    .join(self.aube_dir_entry_name(&dep.dep_path))
                    .join("node_modules")
                    .join(&dep.name);
                if !source_dir.exists() {
                    continue;
                }

                let target_dir = nm.join(&dep.name);
                let link_parent = target_dir.parent().unwrap_or(&nm);
                let rel_target =
                    pathdiff::diff_paths(&source_dir, link_parent).unwrap_or(source_dir.clone());

                if let Some(parent) = target_dir.parent() {
                    xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
                }

                sys::create_dir_link(&rel_target, &target_dir)
                    .map_err(|e| Error::Io(target_dir.clone(), e))?;

                trace!("workspace top-level: {} -> {}", dep.name, importer_path);
                stats.top_level_linked += 1;
            }
        }

        // Hoisting passes run against the *root* importer only —
        // pnpm never hoists into nested workspace packages. Run the
        // selective public-hoist-pattern first so matched names take
        // precedence, then `shamefully_hoist` sweeps up everything
        // else.
        if !self.public_hoist_patterns.is_empty() {
            self.hoist_remaining_into(
                &root_nm,
                &aube_dir,
                graph,
                &mut stats,
                "workspace public-hoist",
                &|name| self.public_hoist_matches(name),
            )?;
        }
        if self.shamefully_hoist {
            self.hoist_remaining_into(
                &root_nm,
                &aube_dir,
                graph,
                &mut stats,
                "workspace hoist",
                &|_| true,
            )?;
        }

        // Hidden hoist targets the shared `.aube/node_modules/`
        // regardless of importer, so a single sweep here is
        // sufficient for the whole workspace.
        self.link_hidden_hoist(&aube_dir, graph)?;

        Ok(stats)
    }

    /// Populate (or sweep) the hidden modules directory at
    /// `aube_dir/node_modules/<name>`. When `self.hoist` is enabled,
    /// walks every non-local package in the graph and creates a
    /// symlink for names that match `hoist_patterns` into the
    /// corresponding `.aube/<dep_path>/node_modules/<name>` entry.
    /// When disabled, wipes the directory so previously-hoisted
    /// symlinks don't keep resolving through Node's parent walk.
    ///
    /// Unlike `hoist_remaining_into`, this writes into a private
    /// sibling of `.aube/<dep_path>/` rather than the visible root
    /// `node_modules/`. Packages inside the virtual store (e.g.
    /// `.aube/react@18/node_modules/react/`) walk up through
    /// `.aube/node_modules/` during require resolution, which is the
    /// only consumer of these links — nothing inside the user's own
    /// `node_modules/<name>` view is affected.
    fn link_hidden_hoist(&self, aube_dir: &Path, graph: &LockfileGraph) -> Result<(), Error> {
        let hidden = aube_dir.join("node_modules");
        if !self.hoist {
            // Previous install may have populated this tree with
            // hoist=true. Nuke it so Node doesn't keep resolving
            // phantom deps through the stale symlinks.
            let _ = std::fs::remove_dir_all(&hidden);
            return Ok(());
        }
        // Wipe before repopulating so a dependency removed from the
        // graph (or a pattern that no longer matches) doesn't linger.
        let _ = std::fs::remove_dir_all(&hidden);
        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (dep_path, pkg) in &graph.packages {
            if pkg.local_source.is_some() {
                continue;
            }
            if !self.hoist_matches(&pkg.name) {
                continue;
            }
            // First-writer-wins on name clashes across versions. BTree
            // iteration over `graph.packages` gives a deterministic
            // tiebreaker across runs (earliest `name@version` key).
            if !claimed.insert(pkg.name.clone()) {
                continue;
            }
            let source_dir = aube_dir
                .join(self.aube_dir_entry_name(dep_path))
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                continue;
            }
            let target_dir = hidden.join(&pkg.name);
            if let Some(parent) = target_dir.parent() {
                xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
            }
            let link_parent = target_dir.parent().unwrap_or(&hidden);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            sys::create_dir_link(&rel_target, &target_dir)
                .map_err(|e| Error::Io(target_dir.clone(), e))?;
            trace!("hidden-hoist: {}", pkg.name);
            // Intentionally not counted in `stats.top_level_linked`.
            // That counter reflects the user-visible root
            // `node_modules/<name>` entries; hidden-hoist symlinks
            // live under `.aube/node_modules/` and are only reached
            // via Node's parent-directory walk from inside the
            // virtual store, not from the user's own code.
        }
        Ok(())
    }

    /// Shared `shamefully_hoist` implementation. For every non-local
    /// package in the graph, create a symlink at `nm/<pkg.name>`
    /// pointing at the matching `.aube/<dep_path>/node_modules/<pkg.name>`
    /// entry. Names already claimed by a prior pass (direct deps,
    /// workspace packages, link: deps) are preserved — first-write-wins.
    /// Iteration order is BTreeMap-stable so the tiebreaker is
    /// deterministic across runs. `trace_label` distinguishes the
    /// `link_all` vs `link_workspace` callers in `-v` output.
    fn hoist_remaining_into(
        &self,
        nm: &Path,
        aube_dir: &Path,
        graph: &LockfileGraph,
        stats: &mut LinkStats,
        trace_label: &str,
        select: &dyn Fn(&str) -> bool,
    ) -> Result<(), Error> {
        for (dep_path, pkg) in &graph.packages {
            if pkg.local_source.is_some() {
                continue;
            }
            if !select(&pkg.name) {
                continue;
            }
            let target_dir = nm.join(&pkg.name);
            if keep_or_reclaim_broken_symlink(&target_dir)? {
                continue;
            }
            let source_dir = aube_dir
                .join(self.aube_dir_entry_name(dep_path))
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                continue;
            }
            if let Some(parent) = target_dir.parent() {
                xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
            }
            let link_parent = target_dir.parent().unwrap_or(nm);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            sys::create_dir_link(&rel_target, &target_dir)
                .map_err(|e| Error::Io(target_dir.clone(), e))?;
            trace!("{trace_label}: {}", pkg.name);
            stats.top_level_linked += 1;
        }
        Ok(())
    }

    /// Materialize a package in the global virtual store if not already present.
    ///
    /// Materialize `dep_path` into the shared global virtual store.
    ///
    /// Uses atomic rename to avoid TOCTOU races: materializes into a
    /// PID-stamped temp directory, then renames into place. If another
    /// process wins the race, its result is kept and the temp dir is
    /// cleaned up.
    ///
    /// Exposed so the install driver can pipeline GVS population into
    /// the fetch phase: as each tarball finishes importing into the
    /// CAS, the driver calls this to reflink the package into its
    /// `~/.cache/aube/virtual-store/<subdir>` entry. Link step 1 then
    /// hits the `pkg_nm_dir.exists()` fast path and only creates the
    /// per-project `.aube/<dep_path>` symlink.
    pub fn ensure_in_virtual_store(
        &self,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
    ) -> Result<(), Error> {
        // Global-store paths always run through the vstore_key map —
        // when hashes are installed this folds dep-graph + engine
        // state into the leaf name, so concurrent builds of the same
        // package against different toolchains don't collide.
        let subdir = self.virtual_store_subdir(dep_path);
        let pkg_nm_dir = self
            .virtual_store
            .join(&subdir)
            .join("node_modules")
            .join(&pkg.name);

        if pkg_nm_dir.exists() {
            trace!("virtual store hit: {dep_path}");
            stats.packages_cached += 1;
            return Ok(());
        }

        // Materialize into a temp directory, then atomically rename into place
        // to avoid TOCTOU races between concurrent `aube install` processes.
        // `subdir` already comes from `dep_path_to_filename`, which
        // flattens `/` to `+` as part of its escape pass, so it's
        // already safe to splice into a single path component.
        let tmp_name = format!(".tmp-{}-{subdir}", std::process::id());
        let tmp_base = self.virtual_store.join(&tmp_name);

        let result = self.materialize_into(&tmp_base, dep_path, pkg, index, stats, true);

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return result;
        }

        // Atomically move the dep_path entry from the temp dir to the final location.
        let tmp_entry = tmp_base.join(&subdir);
        let final_entry = self.virtual_store.join(&subdir);

        // Ensure the parent of the final entry exists (e.g. for scoped packages).
        if let Some(parent) = final_entry.parent() {
            xx::file::mkdirp(parent).map_err(|e| Error::Xx(e.to_string()))?;
        }

        match std::fs::rename(&tmp_entry, &final_entry) {
            Ok(()) => {
                trace!("atomically placed {subdir} in virtual store");
            }
            Err(e) if final_entry.exists() => {
                // Another process won the race — that's fine, use theirs.
                trace!("lost rename race for {dep_path}, using existing: {e}");
                // Undo the stats from our materialization since we're discarding it
                stats.packages_linked = stats.packages_linked.saturating_sub(1);
                stats.files_linked = stats.files_linked.saturating_sub(index.len());
                stats.packages_cached += 1;
                // Lost-race path: our `subdir` is still inside
                // `tmp_base`, so a full recursive delete is needed.
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Ok(());
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_base);
                return Err(Error::Io(final_entry, e));
            }
        }

        // Successful rename: `tmp_base` is now an empty wrapper directory
        // (its single child was the subdir we just renamed out). Use
        // `remove_dir` instead of `remove_dir_all` — the latter still
        // does the full `opendir`/`fdopendir`(fcntl)/`readdir`/`close`
        // walk even on an empty dir, which dtrace shows as ~6 extra
        // syscalls per package. At 227 packages that's ~1.4k wasted
        // syscalls on every cold install.
        //
        // `remove_dir` fails with `ENOTEMPTY` if a future change to
        // `materialize_into` starts dropping extra files into
        // `tmp_base`. Log at debug so the leak is observable without
        // being fatal; the worst-case outcome is a stray tmp dir, and
        // concurrent-writer races already use the full
        // `remove_dir_all` branch above.
        if let Err(e) = std::fs::remove_dir(&tmp_base) {
            debug!(
                "remove_dir({}) failed, leaving tmp in place: {e}",
                tmp_base.display()
            );
        }

        Ok(())
    }

    /// Materialize a package's files and transitive dep symlinks into a base directory.
    ///
    /// `apply_hashes` controls whether per-dep subdir names are run
    /// through `vstore_key` (the content-addressed name) or used as
    /// raw `dep_path` strings. Global-store callers pass `true` so
    /// the shared `~/.cache/aube/virtual-store/` can hold isolated
    /// copies for each `(deps_hash, engine)` combination;
    /// per-project `.aube/` callers pass `false` because node's
    /// runtime module walk resolves by dep_path only.
    fn materialize_into(
        &self,
        base_dir: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        apply_hashes: bool,
    ) -> Result<(), Error> {
        let subdir = if apply_hashes {
            self.virtual_store_subdir(dep_path)
        } else {
            self.aube_dir_entry_name(dep_path)
        };
        let pkg_nm_dir = base_dir.join(&subdir).join("node_modules").join(&pkg.name);

        // Pre-compute the set of unique parent directories across
        // every file in the index AND every scoped transitive-dep
        // symlink we're about to create, then mkdir them in a single
        // pass. Previously each file looped through `mkdirp(parent)`
        // which always did an `exists()` check (= statx syscall) even
        // though the same parents were shared by dozens of siblings —
        // `materialize_into` for a typical 32-file npm package
        // resulted in ~25 redundant statx calls. Collecting the unique
        // parents first, sorting by length (so ancestors precede
        // descendants), and calling `create_dir_all` once each cuts
        // out the redundant stats entirely. `BTreeSet` sorts
        // lexicographically, which is good enough because every
        // ancestor of a directory is a prefix of it.
        let pkg_nm_parent = base_dir.join(&subdir).join("node_modules");
        let mut parents: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        parents.insert(pkg_nm_dir.clone());
        for rel_path in index.keys() {
            let target = pkg_nm_dir.join(rel_path);
            if let Some(parent) = target.parent() {
                parents.insert(parent.to_path_buf());
            }
        }
        // Scoped transitive deps need `pkg_nm_parent/@scope/` to exist
        // before the symlink call; include those parents in the batch.
        for dep_name in pkg.dependencies.keys() {
            if let Some(slash) = dep_name.find('/')
                && dep_name.starts_with('@')
            {
                parents.insert(pkg_nm_parent.join(&dep_name[..slash]));
            }
        }
        for parent in &parents {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.clone(), e))?;
        }

        // `materialize_into` always writes into a fresh location
        // (either a `.tmp-<pid>-...` staging dir for the global virtual
        // store or a per-project `.aube/<dep_path>` just created by
        // the caller), so we can skip the `remove_file(dst)` that
        // `link_file` does defensively. Pass `fresh = true` to suppress
        // the unlink syscall on every file. For a 1.4k-package install
        // that's ~45k wasted `unlink` calls on the hot path.
        for (rel_path, stored) in index {
            let target = pkg_nm_dir.join(rel_path);

            self.link_file_fresh(&stored.store_path, &target)?;
            stats.files_linked += 1;

            if stored.executable {
                #[cfg(unix)]
                xx::file::make_executable(&target).map_err(|e| Error::Xx(e.to_string()))?;
            }
        }

        // Apply any user-supplied patch for this `(name, version)`.
        // Patches are applied *after* the files have been linked into
        // the virtual store but *before* transitive symlinks, so the
        // patched bytes live alongside the unpatched ones at a
        // distinct subdir (the graph hash callback is responsible for
        // making sure that's true).
        let patch_key = format!("{}@{}", pkg.name, pkg.version);
        if let Some(patch_text) = self.patches.get(&patch_key) {
            apply_multi_file_patch(&pkg_nm_dir, patch_text)
                .map_err(|msg| Error::Patch(patch_key.clone(), msg))?;
        }

        // Create symlinks for transitive dependencies. Parents for
        // scoped packages were added to the `parents` batch above, so
        // we no longer need a per-symlink mkdirp. We also skip the
        // `symlink_metadata().is_ok()` existence check: callers
        // guarantee the target directory is freshly created (either a
        // `.tmp-<pid>-...` staging dir for the global virtual store or
        // a per-project `.aube/<dep_path>` that the caller just
        // ensured is empty), so nothing can be in the way.
        for (dep_name, dep_version) in &pkg.dependencies {
            let dep_dep_path = format!("{dep_name}@{dep_version}");
            if dep_dep_path == *dep_path && dep_name == &pkg.name {
                continue;
            }
            // Match the parent's convention: global-store materialization
            // walks sibling subdirs under their hashed names, while the
            // per-project `.aube/` layout uses raw dep_paths.
            let sibling_subdir = if apply_hashes {
                self.virtual_store_subdir(&dep_dep_path)
            } else {
                self.aube_dir_entry_name(&dep_dep_path)
            };
            let symlink_path = pkg_nm_parent.join(dep_name);
            // Compute the relative path from the symlink's parent to
            // the sibling dep directory. The symlink's parent is
            // `pkg_nm_parent/` for a bare name but
            // `pkg_nm_parent/@scope/` for a scoped one, so we can't
            // hard-code `../..` — doing so would undercount by one
            // level for every scoped transitive dep and produce a
            // dangling link. `pathdiff::diff_paths` walks the
            // difference for us, yielding `../..` for `foo` and
            // `../../..` for `@vue/shared`, both relative to whatever
            // parent `symlink_path` ends up with.
            // `pkg_nm_parent` is `<base_dir>/<subdir>/node_modules/`, so
            // two parents deep brings us to `<base_dir>/` where all
            // sibling subdirs live side-by-side.
            let virtual_root = pkg_nm_parent
                .parent()
                .and_then(Path::parent)
                .unwrap_or(&pkg_nm_parent);
            let sibling_abs = virtual_root
                .join(&sibling_subdir)
                .join("node_modules")
                .join(dep_name);
            let link_parent = symlink_path.parent().unwrap_or(&pkg_nm_parent);
            let target = pathdiff::diff_paths(&sibling_abs, link_parent)
                .unwrap_or_else(|| sibling_abs.clone());

            sys::create_dir_link(&target, &symlink_path)
                .map_err(|e| Error::Io(symlink_path.clone(), e))?;
        }

        stats.packages_linked += 1;
        trace!("materialized {dep_path} ({} files)", index.len());
        Ok(())
    }

    /// Hardlink/reflink/copy a file into a freshly-created destination.
    /// Assumes `dst` does not exist — callers (`materialize_into`)
    /// always write into a `.tmp-<pid>-...` staging dir or a
    /// just-wiped per-project `.aube/<dep_path>`, so the defensive
    /// `remove_file(dst)` an idempotent variant would need is skipped.
    /// Eliminates one syscall per linked file (~45k on the medium
    /// benchmark fixture).
    pub(crate) fn link_file_fresh(&self, src: &Path, dst: &Path) -> Result<(), Error> {
        match self.strategy {
            LinkStrategy::Reflink => {
                if let Err(e) = reflink_copy::reflink(src, dst) {
                    // Fall back to copy on cross-filesystem errors
                    trace!("reflink failed, falling back to copy: {e}");
                    std::fs::copy(src, dst).map_err(|e| Error::Io(dst.to_path_buf(), e))?;
                }
            }
            LinkStrategy::Hardlink => {
                if let Err(e) = std::fs::hard_link(src, dst) {
                    // Fall back to copy on cross-filesystem errors (EXDEV)
                    trace!("hardlink failed, falling back to copy: {e}");
                    std::fs::copy(src, dst).map_err(|e| Error::Io(dst.to_path_buf(), e))?;
                }
            }
            LinkStrategy::Copy => {
                std::fs::copy(src, dst).map_err(|e| Error::Io(dst.to_path_buf(), e))?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct LinkStats {
    pub packages_linked: usize,
    pub packages_cached: usize,
    pub files_linked: usize,
    pub top_level_linked: usize,
    /// Populated only when the linker ran in `NodeLinker::Hoisted`
    /// mode. Maps lockfile `dep_path` → list of on-disk directories
    /// where that package was materialized (most entries have one
    /// path; name conflicts produce multiple nested copies). The
    /// install driver uses this to locate packages for bin linking
    /// and lifecycle scripts without recomputing the placement tree.
    /// `None` means "isolated layout — use the `.aube/<dep_path>`
    /// convention".
    pub hoisted_placements: Option<HoistedPlacements>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("file error: {0}")]
    Xx(String),
    #[error("failed to link {0} -> {1}: {2}")]
    Link(PathBuf, PathBuf, String),
    #[error("failed to apply patch for {0}: {1}")]
    Patch(String, String),
    #[error(
        "internal: missing package index for {0} — caller skipped `load_index` but the package wasn't already materialized"
    )]
    MissingPackageIndex(String),
}

/// Decide whether an existing `node_modules/<name>` entry can be left
/// alone, or must be removed so the caller can recreate it.
///
/// Returns `Ok(true)` when a live entry is present and should be
/// preserved. Returns `Ok(false)` when nothing is there (or a broken
/// link was reclaimed) and the caller should proceed to create the
/// entry. `symlink_metadata().is_ok()` on its own treats a dangling
/// symlink — whose `.aube/<dep_path>/...` target has been deleted — as
/// "already in place", which silently leaves the project unresolvable.
///
/// `sys::create_dir_link` produces a Unix symlink on Unix and an NTFS
/// junction on Windows. A junction's `file_type().is_symlink()` is
/// `false`, so we trust the `symlink_metadata().is_ok() && !exists()`
/// pair to identify "something is at `path` but its target is gone",
/// and use the same `remove_dir().or_else(remove_file())` fallback
/// used elsewhere in this file to unlink both shapes.
fn keep_or_reclaim_broken_symlink(path: &Path) -> Result<bool, Error> {
    if path.symlink_metadata().is_err() {
        return Ok(false);
    }
    // `Path::exists` follows symlinks/junctions, so a dangling link
    // returns false here even though `symlink_metadata` succeeded.
    if path.exists() {
        return Ok(true);
    }
    match std::fs::remove_dir(path).or_else(|_| std::fs::remove_file(path)) {
        Ok(()) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(Error::Io(path.to_path_buf(), e)),
    }
}

/// Compute per-`(name@version)` content hashes for the currently
/// configured patch set. Returns a stable map so the caller can
/// compare it against a sidecar from a previous install.
fn current_patch_hashes(patches: &Patches) -> std::collections::BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    patches
        .iter()
        .map(|(k, v)| {
            let mut h = Sha256::new();
            h.update(v.as_bytes());
            (k.clone(), hex::encode(h.finalize()))
        })
        .collect()
}

/// Read the previously-applied patch sidecar at
/// `node_modules/.aube-applied-patches.json`. Missing or malformed
/// files return an empty map — the caller treats them as "no patches
/// were ever applied here," which conservatively triggers a re-link
/// on the first run after the linker started writing the sidecar.
fn read_applied_patches(nm_dir: &Path) -> std::collections::BTreeMap<String, String> {
    let path = nm_dir.join(".aube-applied-patches.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json_parse_map(&raw).unwrap_or_default()
}

/// Tiny hand-rolled JSON object parser specialized for the sidecar:
/// `{"name@ver": "hex", ...}`. Avoids dragging serde_json into
/// `aube-linker` for one file. Returns `None` on any malformed input
/// so the caller falls back to "no previous state."
fn serde_json_parse_map(s: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let s = s.trim();
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    let mut out = std::collections::BTreeMap::new();
    if s.trim().is_empty() {
        return Some(out);
    }
    for entry in s.split(',') {
        let (k, v) = entry.split_once(':')?;
        let k = k.trim().trim_matches('"');
        let v = v.trim().trim_matches('"');
        out.insert(k.to_string(), v.to_string());
    }
    Some(out)
}

/// Write the applied-patch sidecar so the next install can detect
/// added/removed/changed patches and re-materialize the affected
/// `.aube/<dep_path>` entries. Best-effort: a write error here just
/// means the next run will conservatively wipe more entries than
/// strictly necessary.
fn write_applied_patches(nm_dir: &Path, map: &std::collections::BTreeMap<String, String>) {
    let path = nm_dir.join(".aube-applied-patches.json");
    let mut out = String::from("{");
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\"{k}\":\"{v}\""));
    }
    out.push('}');
    let _ = std::fs::write(path, out);
}

/// Wipe `.aube/<dep_path>` for any package whose patch fingerprint
/// changed between the previous and current install. Used by the
/// per-project (no-global-store) link path, where the directory name
/// doesn't otherwise change when a patch is added or removed.
fn wipe_changed_patched_entries(
    aube_dir: &Path,
    graph: &LockfileGraph,
    prev: &std::collections::BTreeMap<String, String>,
    curr: &std::collections::BTreeMap<String, String>,
    max_length: usize,
) {
    let mut affected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for k in prev.keys().chain(curr.keys()) {
        if prev.get(k) != curr.get(k) {
            affected.insert(k.clone());
        }
    }
    if affected.is_empty() {
        return;
    }
    for (dep_path, pkg) in &graph.packages {
        let key = format!("{}@{}", pkg.name, pkg.version);
        if affected.contains(&key) {
            let entry = aube_dir.join(dep_path_to_filename(dep_path, max_length));
            let _ = std::fs::remove_dir_all(entry);
        }
    }
}

/// Apply a git-style multi-file unified diff to a package directory.
///
/// The patch text is split on `diff --git ` boundaries; each section
/// is parsed as a single-file unified diff and applied to the matching
/// file under `pkg_dir`. We deliberately unlink the destination
/// before writing, because the linker materializes files via reflink
/// or hardlink — modifying the file in place would corrupt the global
/// content-addressed store the linked file points to.
fn apply_multi_file_patch(pkg_dir: &Path, patch_text: &str) -> Result<(), String> {
    let sections = split_patch_sections(patch_text);
    if sections.is_empty() {
        return Err("patch contained no `diff --git` sections".to_string());
    }
    for section in sections {
        let rel = section
            .rel_path
            .as_ref()
            .ok_or_else(|| "patch section missing file path".to_string())?;
        let target = pkg_dir.join(rel);
        let original = if target.exists() {
            std::fs::read_to_string(&target)
                .map_err(|e| format!("failed to read {}: {e}", target.display()))?
        } else {
            String::new()
        };
        // `+++ /dev/null` means the patch deletes the file. Skip diffy
        // entirely — `diffy::apply` would otherwise produce an empty
        // string and we'd write a zero-byte file in place of the
        // original, leaving `require('./removed')` resolving to an
        // empty module instead of the expected `MODULE_NOT_FOUND`.
        if section.is_deletion {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            }
            continue;
        }
        let parsed = diffy::Patch::from_str(&section.body)
            .map_err(|e| format!("failed to parse patch for {rel}: {e}"))?;
        let patched = diffy::apply(&original, &parsed)
            .map_err(|e| format!("failed to apply patch for {rel}: {e}"))?;
        // Break any reflink/hardlink to the global store before
        // writing the patched bytes — otherwise we'd silently mutate
        // every other project sharing this CAS file.
        if target.exists() {
            std::fs::remove_file(&target)
                .map_err(|e| format!("failed to unlink {}: {e}", target.display()))?;
        } else if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, patched.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", target.display()))?;
    }
    Ok(())
}

struct PatchSection {
    rel_path: Option<String>,
    /// Single-file unified diff body — `diffy::Patch::from_str` parses
    /// this directly. Always begins with `--- ` so the diffy parser
    /// finds its anchor.
    body: String,
    /// `+++ /dev/null` was seen in the header — the patch deletes this
    /// file, so the linker should `remove_file` instead of writing
    /// patched bytes (which `diffy::apply` would emit as an empty
    /// string).
    is_deletion: bool,
}

/// Split a git-style multi-file patch into one section per file.
/// We look for `diff --git a/<path> b/<path>` markers, pull the path
/// out of the `b/...` half (post-edit name), and capture everything
/// from the next `--- ` line until the following `diff --git ` (or
/// EOF) as the diffy-compatible body.
fn split_patch_sections(text: &str) -> Vec<PatchSection> {
    let mut out: Vec<PatchSection> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut body = String::new();
    let mut in_body = false;
    let mut is_deletion = false;

    let flush = |out: &mut Vec<PatchSection>,
                 path: &mut Option<String>,
                 body: &mut String,
                 is_deletion: &mut bool| {
        if !body.is_empty() || *is_deletion {
            out.push(PatchSection {
                rel_path: path.take(),
                body: std::mem::take(body),
                is_deletion: std::mem::replace(is_deletion, false),
            });
        } else {
            *path = None;
        }
    };

    for line in text.split_inclusive('\n') {
        let stripped = line.trim_end_matches('\n');
        if let Some(rest) = stripped.strip_prefix("diff --git ") {
            // New file boundary — flush whatever we were collecting.
            flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
            in_body = false;
            // Parse `a/<path> b/<path>` and prefer the post-edit
            // (`b/`) path so renames land on the new name.
            if let Some(b_idx) = rest.find(" b/") {
                let after_b = &rest[b_idx + 3..];
                current_path = Some(after_b.to_string());
            }
            continue;
        }
        if !in_body {
            if stripped.starts_with("--- ") {
                in_body = true;
                // Rewrite `--- /dev/null` (file addition) to `--- a/<path>`
                // so diffy's parser still gets a valid header. The
                // original file content we feed `diffy::apply` is empty
                // for additions, which is what diffy expects.
                if stripped == "--- /dev/null"
                    && let Some(rel) = current_path.as_deref()
                {
                    body.push_str(&format!("--- a/{rel}\n"));
                } else {
                    body.push_str(line);
                }
            }
            // Skip git's `index ...` / `new file mode ...` /
            // `similarity index ...` decorations — diffy doesn't
            // understand them and they aren't needed once we know
            // the target path.
            continue;
        }
        if stripped == "+++ /dev/null" {
            // File deletion — note it and drop this header line. The
            // linker will `remove_file` and skip the diffy apply path
            // entirely, so the rest of the body (the hunk that empties
            // the file) is intentionally discarded.
            is_deletion = true;
            continue;
        }
        body.push_str(line);
    }
    flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
    out
}

#[cfg(test)]
mod public_hoist_tests {
    use super::*;

    fn linker_with(patterns: &[&str]) -> Linker {
        // Construct a Linker without touching disk: we only call
        // `public_hoist_matches`, which never looks at `store` or
        // `virtual_store`. A dummy store is acceptable because
        // Store::clone is cheap and this test never invokes a method
        // that would actually touch the CAS.
        let store = Store::at(std::env::temp_dir().join("aube-public-hoist-test"));
        let strs: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        Linker::new(&store, LinkStrategy::Copy).with_public_hoist_pattern(&strs)
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        let l = linker_with(&[]);
        assert!(!l.public_hoist_matches("react"));
        assert!(!l.public_hoist_matches("eslint"));
    }

    #[test]
    fn wildcard_matches_substring() {
        let l = linker_with(&["*eslint*", "*prettier*"]);
        assert!(l.public_hoist_matches("eslint"));
        assert!(l.public_hoist_matches("eslint-plugin-react"));
        assert!(l.public_hoist_matches("@typescript-eslint/parser"));
        assert!(l.public_hoist_matches("prettier"));
        assert!(!l.public_hoist_matches("react"));
    }

    #[test]
    fn exact_name_match() {
        let l = linker_with(&["react"]);
        assert!(l.public_hoist_matches("react"));
        assert!(!l.public_hoist_matches("react-dom"));
    }

    #[test]
    fn negation_excludes_positive_match() {
        let l = linker_with(&["*eslint*", "!eslint-config-*"]);
        assert!(l.public_hoist_matches("eslint"));
        assert!(l.public_hoist_matches("eslint-plugin-react"));
        assert!(!l.public_hoist_matches("eslint-config-next"));
    }

    #[test]
    fn case_insensitive() {
        let l = linker_with(&["*ESLINT*"]);
        assert!(l.public_hoist_matches("eslint"));
        assert!(l.public_hoist_matches("ESLint"));
    }

    #[test]
    fn invalid_patterns_are_silently_dropped() {
        // `[` opens an unclosed character class — glob::Pattern::new
        // rejects it; the builder skips the pattern instead of
        // failing install. The accompanying valid pattern still
        // matches.
        let l = linker_with(&["[unterminated", "react"]);
        assert!(l.public_hoist_matches("react"));
        assert!(!l.public_hoist_matches("eslint"));
    }
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn round_trips_simple_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = 'old';\n").unwrap();

        let patch = "diff --git a/index.js b/index.js\n\
                     --- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -module.exports = 'old';\n\
                     +module.exports = 'new';\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "module.exports = 'new';\n"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aube_lockfile::{DepType, DirectDep, LockedPackage, LockfileGraph};
    use aube_store::Store;

    fn setup_store_with_files(dir: &Path) -> (Store, BTreeMap<String, aube_store::PackageIndex>) {
        let store = Store::at(dir.join("store/files"));

        let mut indices = BTreeMap::new();

        // foo@1.0.0 with index.js
        let foo_stored = store
            .import_bytes(b"module.exports = 'foo';", false)
            .unwrap();
        let mut foo_index = BTreeMap::new();
        foo_index.insert("index.js".to_string(), foo_stored);

        // foo also has package.json
        let foo_pkg = store
            .import_bytes(b"{\"name\":\"foo\",\"version\":\"1.0.0\"}", false)
            .unwrap();
        foo_index.insert("package.json".to_string(), foo_pkg);
        indices.insert("foo@1.0.0".to_string(), foo_index);

        // bar@2.0.0 with index.js
        let bar_stored = store
            .import_bytes(b"module.exports = 'bar';", false)
            .unwrap();
        let mut bar_index = BTreeMap::new();
        bar_index.insert("index.js".to_string(), bar_stored);
        indices.insert("bar@2.0.0".to_string(), bar_index);

        (store, indices)
    }

    fn make_graph() -> LockfileGraph {
        let mut packages = BTreeMap::new();

        let mut foo_deps = BTreeMap::new();
        foo_deps.insert("bar".to_string(), "2.0.0".to_string());

        packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                integrity: None,
                dependencies: foo_deps,
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        packages.insert(
            "bar@2.0.0".to_string(),
            LockedPackage {
                name: "bar".to_string(),
                version: "2.0.0".to_string(),
                integrity: None,
                dependencies: BTreeMap::new(),
                dep_path: "bar@2.0.0".to_string(),
                ..Default::default()
            },
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        LockfileGraph {
            importers,
            packages,
            ..Default::default()
        }
    }

    #[test]
    fn test_detect_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let strategy = Linker::detect_strategy(dir.path());
        // Should detect reflink or hardlink on most systems, never panic
        match strategy {
            LinkStrategy::Reflink | LinkStrategy::Hardlink | LinkStrategy::Copy => {}
        }
    }

    #[test]
    fn test_link_all_creates_pnpm_virtual_store() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let graph = make_graph();

        let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

        // .aube virtual store should exist
        assert!(project_dir.join("node_modules/.aube").exists());

        // .aube/foo@1.0.0 should be a symlink to the global virtual store
        let aube_foo = project_dir.join("node_modules/.aube/foo@1.0.0");
        assert!(aube_foo.symlink_metadata().unwrap().is_symlink());

        // foo@1.0.0 content should be accessible through the symlink
        let foo_in_pnpm =
            project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/foo/index.js");
        assert!(foo_in_pnpm.exists());
        assert_eq!(
            std::fs::read_to_string(&foo_in_pnpm).unwrap(),
            "module.exports = 'foo';"
        );

        // bar@2.0.0 should also be accessible
        let bar_in_pnpm =
            project_dir.join("node_modules/.aube/bar@2.0.0/node_modules/bar/index.js");
        assert!(bar_in_pnpm.exists());

        assert_eq!(stats.packages_linked, 2);
        assert!(stats.files_linked >= 3); // foo has 2 files, bar has 1
    }

    #[test]
    fn test_link_all_creates_top_level_entries() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new(&store, LinkStrategy::Copy);
        let graph = make_graph();

        let stats = linker.link_all(&project_dir, &graph, &indices).unwrap();

        // Top-level foo/ should exist (it's a direct dep)
        let foo_top = project_dir.join("node_modules/foo/index.js");
        assert!(foo_top.exists());
        assert_eq!(
            std::fs::read_to_string(&foo_top).unwrap(),
            "module.exports = 'foo';"
        );

        // bar should NOT be top-level (it's only a transitive dep)
        let bar_top = project_dir.join("node_modules/bar/index.js");
        assert!(!bar_top.exists());

        assert_eq!(stats.top_level_linked, 1);
    }

    #[test]
    fn test_link_all_transitive_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new(&store, LinkStrategy::Copy);
        let graph = make_graph();

        linker.link_all(&project_dir, &graph, &indices).unwrap();

        // foo's node_modules/bar should be a symlink (inside the global virtual store)
        // The path resolves through the .aube symlink into the global store
        let bar_symlink = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
        assert!(bar_symlink.symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn test_link_all_cleans_existing_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        let nm = project_dir.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("stale-file.txt"), "old").unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new(&store, LinkStrategy::Copy);
        let graph = make_graph();

        linker.link_all(&project_dir, &graph, &indices).unwrap();

        // Old file should be gone
        assert!(!nm.join("stale-file.txt").exists());
        // New structure should exist
        assert!(nm.join(".aube").exists());
    }

    #[test]
    fn test_link_all_nested_node_modules_for_direct_deps() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new(&store, LinkStrategy::Copy);
        let graph = make_graph();

        linker.link_all(&project_dir, &graph, &indices).unwrap();

        // foo is a direct dep with bar as a transitive dep.
        // The top-level node_modules/foo is a symlink to .aube/foo@1.0.0/node_modules/foo,
        // and bar lives as a sibling at .aube/foo@1.0.0/node_modules/bar (also a symlink
        // pointing to .aube/bar@2.0.0/node_modules/bar). Node's directory walk from inside
        // foo finds bar this way without aube creating any nested node_modules.
        let foo_link = project_dir.join("node_modules/foo");
        assert!(foo_link.symlink_metadata().unwrap().is_symlink());
        let bar_sibling = project_dir.join("node_modules/.aube/foo@1.0.0/node_modules/bar");
        assert!(bar_sibling.symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn test_global_virtual_store_is_populated() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let virtual_store = store.virtual_store_dir();
        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let graph = make_graph();

        linker.link_all(&project_dir, &graph, &indices).unwrap();

        // Global virtual store should contain materialized packages
        let foo_global = virtual_store.join("foo@1.0.0/node_modules/foo/index.js");
        assert!(foo_global.exists());
        assert_eq!(
            std::fs::read_to_string(&foo_global).unwrap(),
            "module.exports = 'foo';"
        );

        let bar_global = virtual_store.join("bar@2.0.0/node_modules/bar/index.js");
        assert!(bar_global.exists());
    }

    #[test]
    fn test_second_install_reuses_global_store() {
        let dir = tempfile::tempdir().unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let graph = make_graph();

        // First install
        let project1 = dir.path().join("project1");
        std::fs::create_dir_all(&project1).unwrap();
        let stats1 = linker.link_all(&project1, &graph, &indices).unwrap();
        assert_eq!(stats1.packages_linked, 2);
        assert_eq!(stats1.packages_cached, 0);

        // Second install with same deps — should reuse global virtual store
        let project2 = dir.path().join("project2");
        std::fs::create_dir_all(&project2).unwrap();
        let stats2 = linker.link_all(&project2, &graph, &indices).unwrap();
        assert_eq!(stats2.packages_linked, 0);
        assert_eq!(stats2.packages_cached, 2);
        assert_eq!(stats2.files_linked, 0); // no CAS linking needed

        // Both projects should work
        let foo1 = project1.join("node_modules/foo/index.js");
        let foo2 = project2.join("node_modules/foo/index.js");
        assert!(foo1.exists());
        assert!(foo2.exists());
        assert_eq!(
            std::fs::read_to_string(&foo1).unwrap(),
            std::fs::read_to_string(&foo2).unwrap()
        );
    }
}
