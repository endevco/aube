use tracing::{debug, trace, warn};

use aube_lockfile::dep_path_filename::{
    DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH, dep_path_to_filename,
};
use aube_lockfile::graph_hash::GraphHashes;
use aube_lockfile::{LocalSource, LockedPackage, LockfileGraph};
use aube_store::{PackageIndex, Store, StoredFile};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod hoisted;
pub mod sys;
pub use hoisted::HoistedPlacements;

/// Sweep orphan `.tmp-<pid>-*` directories in the virtual store.
///
/// Linker materializes each package into `.tmp-<pid>-<subdir>/`
/// then atomic-renames into `.aube/<subdir>/`. Crash or Ctrl-C
/// between materialize and rename leaves the tmp dir behind.
/// Nothing else cleans these up so they accumulate on every aborted
/// install. Small footprint per entry but a few hundred aborted
/// CI runs pile up gigabytes.
///
/// Called early in link_all so each fresh install reclaims space
/// from prior crashes. Only matches the exact prefix we produce so
/// user files named `.tmp-*` in the virtual store are safe.
pub fn sweep_stale_tmp_dirs(virtual_store: &Path) {
    let Ok(entries) = std::fs::read_dir(virtual_store) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match our exact prefix. Format: `.tmp-<pid>-<subdir>`
        // where pid is numeric.
        if !name.starts_with(".tmp-") {
            continue;
        }
        let rest = &name[".tmp-".len()..];
        let Some((pid_str, _rest)) = rest.split_once('-') else {
            continue;
        };
        if pid_str.chars().any(|c| !c.is_ascii_digit()) {
            continue;
        }
        // Do not touch the dir of our own still-running process.
        // Materialize path creates and removes its tmp dir in the
        // same call and crashes mid-way are the target here, the
        // active pid will not leave ones around that matter.
        if pid_str == std::process::id().to_string() {
            continue;
        }
        let _ = remove_dir_all_with_retry(&entry.path());
    }
}

/// Remove a directory with retry on Windows sharing violations.
///
/// Windows does not let you delete a file while another process holds
/// a handle open. Dev server, vitest watcher, tsc --watch all hold
/// .js / .node files inside node_modules. aube reinstall hits ERROR
/// 32 (SHARING_VIOLATION) or ERROR 5 (ACCESS_DENIED, AV scanner
/// mid-scan) and leaves a half-deleted virtual store. pnpm, npm,
/// rimraf all retry with backoff. Do the same. Unix passthrough.
///
/// Retries 10 times with exponential backoff starting at 50ms. Total
/// worst case around 10 seconds which is tolerable for an install
/// already paying for filesystem work.
pub fn remove_dir_all_with_retry(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        std::fs::remove_dir_all(path)
    }
    #[cfg(windows)]
    {
        use std::io::ErrorKind;
        let mut delay_ms = 50u64;
        for attempt in 0..10 {
            match std::fs::remove_dir_all(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    // Sharing violation and PermissionDenied both
                    // map to retriable Windows errors. Bail on
                    // attempt 10.
                    let retriable =
                        matches!(e.kind(), ErrorKind::PermissionDenied | ErrorKind::Other)
                            || e.raw_os_error() == Some(32);
                    if !retriable || attempt == 9 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    delay_ms = (delay_ms * 2).min(2000);
                }
            }
        }
        // Unreachable, loop always returns by attempt 10.
        Ok(())
    }
}

/// Real workspace importer, not a peer-context bookkeeping entry.
///
/// pnpm v9 lockfiles record the peer-resolution view of each
/// workspace package reached through every nested `node_modules/`
/// traversal. Those virtual importer paths (e.g.
/// `packages/a/node_modules/@scope/b/node_modules/@scope/c`) describe
/// *how* a package looks from a particular context — they are reached
/// via the workspace-to-workspace symlink chain and have no
/// independent `node_modules/` to populate. When the link pipeline
/// treats them as physical importers it queues parallel symlink tasks
/// whose `link_path`s canonicalize to the same inode as a physical
/// importer's task, producing EEXIST races on large monorepos.
pub fn is_physical_importer(importer_path: &str) -> bool {
    importer_path == "." || !importer_path.contains("/node_modules/")
}

/// Wipe `path` when it looks like a linker-managed `.aube/node_modules`
/// tree. If a previously-tampered install (or attacker) replaced the
/// tree with a symlink / junction pointing elsewhere on disk, refuse
/// to recurse into it — modern Rust `remove_dir_all` already declines
/// to follow symlinks, mirroring the invariant at the call site keeps
/// the intent explicit and catches any future regression in the
/// callee.
fn remove_hidden_hoist_tree(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => {
            let _ = std::fs::remove_file(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_dir_all(path);
        }
        Err(_) => {}
    }
}

/// Best-effort unlink of `path` regardless of whether it's a file,
/// symlink, junction, or directory. Errors are intentionally ignored
/// because this is a "clear the slot" operation — the caller is about
/// to place something else here and any residual entry that survives
/// will surface as a downstream error.
pub(crate) fn try_remove_entry(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
    let _ = std::fs::remove_file(path);
}

/// `xx::file::mkdirp` wrapped with the linker's `Error::Xx` conversion.
/// Every materialize pass calls this before creating a symlink /
/// junction, so the lossy `.to_string()` wrap lives in exactly one
/// place.
pub(crate) fn mkdirp(dir: &Path) -> Result<(), Error> {
    xx::file::mkdirp(dir).map_err(|e| Error::Xx(e.to_string()))
}

/// Classification of a `.aube/<dep_path>` symlink relative to the
/// current hashed global entry the linker wants to point at.
#[derive(Copy, Clone)]
pub(crate) enum EntryState {
    /// The symlink already points at `expected` and the target exists —
    /// nothing to do. Caller can bump a `packages_cached` counter and
    /// move on.
    Fresh,
    /// No entry at `link_path` yet. Caller needs to materialize and
    /// create the symlink, but there's nothing to unlink first.
    Missing,
    /// An entry exists but is stale (different target, dangling link,
    /// or an `Err` read that isn't NotFound). Caller must unlink
    /// before resymlinking.
    Stale,
}

/// Sweep stale entries out of a `node_modules/` directory while
/// preserving everything in `preserve` (bare names like `lodash` and
/// scope prefixes like `@babel`), dotfiles, and — if set — the
/// virtual-store leaf (`aube_dir_leaf`) sitting right under `nm`
/// with a non-dotfile name (the `virtualStoreDir=node_modules/vstore`
/// case). For `@scope` entries we recurse one level and drop any
/// `@scope/<pkg>` whose full `@scope/pkg` name is not in `preserve`;
/// an empty scope directory left behind by the sweep is removed so
/// the next install doesn't trip over a phantom scope tombstone.
pub(crate) fn sweep_stale_top_level_entries(
    nm: &Path,
    preserve: &std::collections::HashSet<&str>,
    aube_dir_leaf: Option<&std::ffi::OsStr>,
) {
    let scope_prefixes: std::collections::HashSet<&str> = preserve
        .iter()
        .filter_map(|n| n.split_once('/').map(|(scope, _)| scope))
        .collect();
    let Ok(entries) = std::fs::read_dir(nm) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        if aube_dir_leaf == Some(name.as_os_str()) {
            continue;
        }
        if preserve.contains(name_str.as_ref()) {
            continue;
        }
        if scope_prefixes.contains(name_str.as_ref()) {
            let scope_dir = entry.path();
            if let Ok(inner) = std::fs::read_dir(&scope_dir) {
                for inner_entry in inner.flatten() {
                    let inner_name = inner_entry.file_name();
                    let full = format!("{}/{}", name_str, inner_name.to_string_lossy());
                    if !preserve.contains(full.as_str()) {
                        try_remove_entry(&inner_entry.path());
                    }
                }
            }
            // If the scope dir is now empty (every member was stale),
            // drop the tombstone directory too.
            if std::fs::read_dir(&scope_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(&scope_dir);
            }
            continue;
        }
        try_remove_entry(&entry.path());
    }
}

/// Sweep broken entries from a shared hidden-hoist directory without
/// deleting live links owned by other projects. The GVS hidden hoist is
/// global, so "not in this project's graph" is not stale enough: another
/// project may still need that link. Only entries whose target no longer
/// exists (or non-link junk) are reclaimed here; current-project names are
/// still target-reconciled by `reconcile_top_level_link` below.
pub(crate) fn sweep_dead_hidden_hoist_entries(hidden: &Path) {
    let Ok(entries) = std::fs::read_dir(hidden) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if name_str.starts_with('@') {
            match std::fs::symlink_metadata(&path) {
                Ok(md) if md.is_dir() && !md.file_type().is_symlink() => {
                    sweep_dead_hidden_hoist_scope(&path);
                    if std::fs::read_dir(&path)
                        .map(|mut d| d.next().is_none())
                        .unwrap_or(false)
                    {
                        let _ = std::fs::remove_dir(&path);
                    }
                }
                Ok(_) => sweep_dead_hidden_hoist_entry(&path),
                Err(_) => {}
            }
            continue;
        }
        sweep_dead_hidden_hoist_entry(&path);
    }
}

fn sweep_dead_hidden_hoist_scope(scope_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(scope_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        sweep_dead_hidden_hoist_entry(&entry.path());
    }
}

fn sweep_dead_hidden_hoist_entry(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() && path.exists() => {}
        Ok(md) if md.file_type().is_symlink() => {
            try_remove_entry(path);
        }
        Ok(md) if md.is_dir() => {
            try_remove_entry(path);
        }
        Ok(_) => {
            try_remove_entry(path);
        }
        Err(_) => {}
    }
}

/// Classify `link_path` against `expected` without the double-check
/// (`read_link` then `exists`) that ate ~1.4k ENOENT syscalls per
/// install on the medium fixture. Fresh means "points at expected
/// AND the target still exists"; everything else is Missing or
/// Stale. The fast path returns without touching disk a second time.
#[inline]
pub(crate) fn classify_entry_state(link_path: &Path, expected: &Path) -> EntryState {
    match std::fs::read_link(link_path) {
        Ok(existing) if existing == expected => {
            if link_path.exists() {
                EntryState::Fresh
            } else {
                EntryState::Stale
            }
        }
        Ok(_) => EntryState::Stale,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EntryState::Missing,
        // Some other error (permission, etc.): treat as Stale and
        // let the removal/recreate path try its best-effort cleanup
        // + surface the real error on symlink creation if unlucky.
        Err(_) => EntryState::Stale,
    }
}

pub use sys::{
    BinShimOptions, create_bin_shim, create_dir_link, normalize_path, parse_posix_shim_target,
    remove_bin_shim, validate_bin_name, validate_bin_target,
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

pub fn default_linker_parallelism() -> usize {
    let default_limit = if cfg!(target_os = "macos") { 4 } else { 16 };

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(default_limit)
}

type LinkPoolCache = std::sync::Mutex<Vec<(usize, std::sync::Arc<rayon::ThreadPool>)>>;
static LINK_POOL_CACHE: std::sync::OnceLock<LinkPoolCache> = std::sync::OnceLock::new();

fn link_pool(threads: usize) -> Option<std::sync::Arc<rayon::ThreadPool>> {
    let cache = LINK_POOL_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().ok()?;
    if let Some((_, pool)) = guard.iter().find(|(t, _)| *t == threads) {
        return Some(pool.clone());
    }
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("aube-linker-{i}"))
        .build()
    {
        Ok(pool) => {
            let pool = std::sync::Arc::new(pool);
            guard.push((threads, pool.clone()));
            Some(pool)
        }
        Err(err) => {
            warn!("failed to build aube linker thread pool: {err}; falling back to caller thread");
            None
        }
    }
}

fn with_link_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    match link_pool(threads) {
        Some(pool) => pool.install(f),
        None => f(),
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
        let use_global_virtual_store = !aube_util::env::is_ci();
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
    ///
    /// One-arg form. Probes within one dir. Fine when store and
    /// project node_modules share the same mount. Use the two-arg
    /// form for installs where the store lives on a different
    /// filesystem than the project (USB drives, bind mounts, Docker
    /// volumes, cross-drive Windows installs). Otherwise the probe
    /// reports reflink or hardlink based on project-FS self-test,
    /// then every real link call crosses an FS boundary and hits
    /// EXDEV. Runtime falls back to `fs::copy` per file silently,
    /// thousands of wasted syscalls, user thinks they got hardlinks.
    pub fn detect_strategy(path: &Path) -> LinkStrategy {
        Self::detect_strategy_cross(path, path)
    }

    /// Two-arg probe. src is the store shard (or any dir on the
    /// store FS), dst is the project modules dir (or any dir on the
    /// destination FS). Probe creates a real cross-mount src file
    /// and tries to reflink/hardlink into dst, which catches EXDEV
    /// up front.
    pub fn detect_strategy_cross(src_dir: &Path, dst_dir: &Path) -> LinkStrategy {
        // Memoize per (src_dir, dst_dir) for the process lifetime.
        // The probe writes a real test file and tries reflink +
        // hardlink, ~3 syscalls + 2 unlinks. Multiple Linker
        // instances within one install (prewarm + final + per-
        // workspace) all repeat the probe; cache the answer.
        type ProbeKey = (std::path::PathBuf, std::path::PathBuf);
        static CACHE: std::sync::OnceLock<
            std::sync::RwLock<std::collections::HashMap<ProbeKey, LinkStrategy>>,
        > = std::sync::OnceLock::new();
        let key = (src_dir.to_path_buf(), dst_dir.to_path_buf());
        let cache = CACHE.get_or_init(Default::default);
        if let Some(hit) = cache.read().expect("probe cache poisoned").get(&key) {
            return *hit;
        }

        let test_src = src_dir.join(".aube-link-test-src");
        let test_dst = dst_dir.join(".aube-link-test-dst");

        let strategy = if std::fs::write(&test_src, b"test").is_ok() {
            if reflink_copy::reflink(&test_src, &test_dst).is_ok() {
                let _ = std::fs::remove_file(&test_src);
                let _ = std::fs::remove_file(&test_dst);
                LinkStrategy::Reflink
            } else {
                let _ = std::fs::remove_file(&test_dst);
                let result = if std::fs::hard_link(&test_src, &test_dst).is_ok() {
                    LinkStrategy::Hardlink
                } else {
                    LinkStrategy::Copy
                };
                let _ = std::fs::remove_file(&test_src);
                let _ = std::fs::remove_file(&test_dst);
                result
            }
        } else {
            LinkStrategy::Copy
        };

        cache
            .write()
            .expect("probe cache poisoned")
            .insert(key, strategy);
        strategy
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
            let _ = crate::remove_dir_all_with_retry(
                &self.aube_dir_for(project_dir).join("node_modules"),
            );
            stats.hoisted_placements = Some(placements);
            return Ok(stats);
        }

        let nm = project_dir.join(&self.modules_dir_name);
        let aube_dir = self.aube_dir_for(project_dir);

        mkdirp(&aube_dir)?;

        // Reclaim space from prior aborted installs. A crash or
        // Ctrl+C between materialize_into and the atomic rename
        // leaves `.tmp-<pid>-*` dirs in the virtual store. Sweep
        // them now so the current install starts clean.
        sweep_stale_tmp_dirs(&aube_dir);

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
        // Preserve the virtual-store leaf name when `aube_dir` sits
        // directly under `nm`. With the default `.aube` the dotfile
        // check inside the sweep covers it, but a user who sets
        // `virtualStoreDir=node_modules/vstore` would otherwise see
        // the sweep delete the freshly-`mkdirp`d virtual store on
        // every install because `vstore` isn't a dotfile and isn't
        // in `root_dep_names`.
        let aube_dir_leaf: Option<std::ffi::OsString> = if aube_dir.parent() == Some(nm.as_path()) {
            aube_dir.file_name().map(|s| s.to_owned())
        } else {
            None
        };
        sweep_stale_top_level_entries(&nm, &root_dep_names, aube_dir_leaf.as_deref());

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

        let nested_link_targets = build_nested_link_targets(project_dir, graph);

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
                self.materialize_into(
                    &aube_dir,
                    dep_path,
                    pkg,
                    index,
                    &mut stats,
                    false,
                    nested_link_targets.as_ref(),
                )?;
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
                            let state = classify_entry_state(&local_aube_entry, &global_entry);

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
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.ensure_in_virtual_store(
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                nested_link_targets.as_ref(),
                            )?;

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
                                mkdirp(parent)?;
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
            tracing::debug!("link:step1 (gvs populate) {:.1?}", step1_timer.elapsed());
        } else {
            use rayon::prelude::*;

            // `wipe_changed_patched_entries` above already removed any
            // `.aube/<dep_path>` whose patch fingerprint changed since
            // the last install, so the existence check below will fall
            // through to `materialize_into` for those packages and
            // pick up the current patch state. In per-project mode the
            // dep paths are already isolated, so we can materialize
            // them independently on the same rayon pool the gvs path
            // uses instead of rebuilding the whole tree serially.
            let link_parallelism = self.link_parallelism();
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
                            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                            if aube_entry.exists() {
                                // Already in place from a previous run —
                                // count as cached. `install.rs`
                                // deliberately omits this dep_path from
                                // `package_indices` on the fast path, so
                                // do the existence check first.
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }
                            // Entry missing — load the index. Fast path in
                            // `install.rs` skips `load_index` when
                            // `aube_entry` already exists; lazy-load here
                            // for the case where a patch / allowBuilds
                            // change invalidated the entry since.
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.materialize_into(
                                &aube_dir,
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                false,
                                nested_link_targets.as_ref(),
                            )?;
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
            if let Err(e) = write_applied_patches(&nm, &curr_applied) {
                tracing::error!(
                    "failed to write .aube-applied-patches.json: {e}. next install may miss stale patched entries"
                );
            }
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

                    // `link:` direct deps point at the on-disk target with
                    // a plain symlink, bypassing `.aube/` entirely.
                    if let Some(pkg) = graph.packages.get(&dep.dep_path)
                        && let Some(LocalSource::Link(rel)) = pkg.local_source.as_ref()
                    {
                        let abs_target = project_dir.join(rel);
                        let link_parent = target_dir.parent().unwrap_or(&nm);
                        let rel_target =
                            pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                        if reconcile_top_level_link(&target_dir, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = target_dir.parent() {
                            mkdirp(parent)?;
                        }
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
                    let link_parent = target_dir.parent().unwrap_or(&nm);
                    let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                        .unwrap_or_else(|| source_dir.clone());
                    // Target-aware reconcile: a version upgrade keeps the
                    // old `node_modules/<name>` symlink but it now points
                    // at a stale `.aube/<old-dep-path>`; we need to
                    // rewrite it to the new `.aube/<new-dep-path>`.
                    if reconcile_top_level_link(&target_dir, &rel_target)? {
                        return Ok(false);
                    }
                    if let Some(parent) = target_dir.parent() {
                        mkdirp(parent)?;
                    }

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
        tracing::debug!(
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

        if let Err(e) = write_applied_patches(&nm, &curr_applied) {
            tracing::error!(
                "failed to write .aube-applied-patches.json: {e}. next install may miss stale patched entries"
            );
        }
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
            if !is_physical_importer(importer_path) {
                continue;
            }
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
                    mkdirp(parent)?;
                }
                try_remove_entry(&link_path);
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
        let _ = crate::remove_dir_all_with_retry(&self.aube_dir_for(root_dir).join("node_modules"));
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

        mkdirp(&aube_dir)?;
        mkdirp(&root_nm)?;

        let mut stats = LinkStats::default();

        // Patch reconciliation. Mirrors `link_all`'s logic: wipe
        // `.aube/<dep_path>` for any package whose patch fingerprint
        // changed between the previous and current install. Only
        // applies to per-project (non-gvs) mode because the gvs path
        // already folds patches into the hashed `.aube/<dep_path>`
        // name via `with_graph_hashes`.
        let prev_applied = read_applied_patches(&root_nm);
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

        let nested_link_targets = build_nested_link_targets(root_dir, graph);

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
            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
            if aube_entry.exists() {
                stats.packages_cached += 1;
                continue;
            }
            self.materialize_into(
                &aube_dir,
                dep_path,
                pkg,
                index,
                &mut stats,
                false,
                nested_link_targets.as_ref(),
            )?;
        }

        // Step 1b: Populate shared .aube virtual store at root for
        // registry packages. Mirrors `link_all`'s parallel +
        // Fresh/Missing/Stale state machine so warm re-runs are a
        // `readlink` per package instead of a recreate per package.
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

                            let state = classify_entry_state(&local_aube_entry, &global_entry);

                            if matches!(state, EntryState::Fresh) {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }

                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.ensure_in_virtual_store(
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                nested_link_targets.as_ref(),
                            )?;

                            if matches!(state, EntryState::Stale) {
                                let _ = std::fs::remove_dir(&local_aube_entry)
                                    .or_else(|_| std::fs::remove_file(&local_aube_entry));
                            }
                            if let Some(parent) = local_aube_entry.parent() {
                                mkdirp(parent)?;
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
            tracing::debug!(
                "link_workspace:step1 (gvs populate) {:.1?}",
                step1_timer.elapsed()
            );
        } else {
            use rayon::prelude::*;

            let link_parallelism = self.link_parallelism();
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
                            let aube_entry = aube_dir.join(self.aube_dir_entry_name(dep_path));
                            if aube_entry.exists() {
                                local_stats.packages_cached += 1;
                                return Ok(local_stats);
                            }
                            let owned_index;
                            let index = match package_indices.get(dep_path) {
                                Some(idx) => idx,
                                None => {
                                    owned_index = self
                                        .store
                                        .load_index(
                                            pkg.registry_name(),
                                            &pkg.version,
                                            pkg.integrity.as_deref(),
                                        )
                                        .ok_or_else(|| {
                                            Error::MissingPackageIndex(dep_path.to_string())
                                        })?;
                                    &owned_index
                                }
                            };
                            self.materialize_into(
                                &aube_dir,
                                dep_path,
                                pkg,
                                index,
                                &mut local_stats,
                                false,
                                nested_link_targets.as_ref(),
                            )?;
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
        }

        // `virtualStoreOnly=true` skips per-importer node_modules
        // population and the root-level hoisting passes, but the
        // hidden `.aube/node_modules/` hoist (Step 4 below) still
        // runs because it lives *inside* the virtual store. Bin
        // linking and lifecycle scripts for the top-level importers
        // are the install driver's responsibility to skip in this
        // mode.
        if self.virtual_store_only {
            // Sweep root_nm of any user-visible entries a prior
            // (non-virtualStoreOnly) install left behind. With the
            // default `virtualStoreDir`, `.aube/` lives directly
            // under `root_nm` and must be preserved. Custom
            // `virtualStoreDir` overrides put `.aube/` outside the
            // sweep zone already.
            let aube_dir_leaf: Option<std::ffi::OsString> =
                if aube_dir.parent() == Some(root_nm.as_path()) {
                    aube_dir.file_name().map(|s| s.to_owned())
                } else {
                    None
                };
            if let Ok(entries) = std::fs::read_dir(&root_nm) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with('.') {
                        continue;
                    }
                    if aube_dir_leaf.as_deref() == Some(name.as_os_str()) {
                        continue;
                    }
                    try_remove_entry(&entry.path());
                }
            }
            self.link_hidden_hoist(&aube_dir, graph)?;
            if let Err(e) = write_applied_patches(&root_nm, &curr_applied) {
                tracing::error!(
                    "failed to write .aube-applied-patches.json: {e}. next install may miss stale patched entries"
                );
            }
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

        // Step 2a: Per-importer setup — ensure each importer's
        // `node_modules/` exists and sweep entries no longer in that
        // importer's direct deps. Cheap serial work (workspace
        // importers count is small; the expensive symlink syscalls
        // run in parallel below). For the root importer we also
        // expand the preserve set with `shamefullyHoist` /
        // `publicHoistPattern` matches so the hoist passes that run
        // after Step 2 don't redo work they'd have preserved.
        let aube_dir_leaf_root: Option<std::ffi::OsString> =
            if aube_dir.parent() == Some(root_nm.as_path()) {
                aube_dir.file_name().map(|s| s.to_owned())
            } else {
                None
            };

        for (importer_path, deps) in &graph.importers {
            if !is_physical_importer(importer_path) {
                continue;
            }
            let nm = if importer_path == "." {
                root_nm.clone()
            } else {
                root_dir.join(importer_path).join(&self.modules_dir_name)
            };
            if importer_path != "." {
                mkdirp(&nm)?;
            }

            let mut preserve: std::collections::HashSet<&str> =
                deps.iter().map(|d| d.name.as_str()).collect();
            if importer_path == "." {
                if self.shamefully_hoist {
                    for pkg in graph.packages.values() {
                        preserve.insert(pkg.name.as_str());
                    }
                } else if !self.public_hoist_patterns.is_empty() {
                    for pkg in graph.packages.values() {
                        if pkg.local_source.is_none() && self.public_hoist_matches(&pkg.name) {
                            preserve.insert(pkg.name.as_str());
                        }
                    }
                }
            }
            let aube_leaf_here = if importer_path == "." {
                aube_dir_leaf_root.as_deref()
            } else {
                None
            };
            sweep_stale_top_level_entries(&nm, &preserve, aube_leaf_here);
        }

        // Step 2b: Create top-level symlinks in parallel.
        // Flatten (importer, dep) pairs so every symlink syscall
        // runs through the rayon pool — 3k+ serial
        // `create_dir_link` calls was the second-biggest slice of
        // the workspace install phase before this change.
        use rayon::prelude::*;

        #[derive(Clone)]
        struct Step2Task<'a> {
            importer_path: &'a str,
            nm: PathBuf,
            dep: &'a aube_lockfile::DirectDep,
        }
        let tasks: Vec<Step2Task<'_>> = graph
            .importers
            .iter()
            .filter(|(importer_path, _)| is_physical_importer(importer_path))
            .flat_map(|(importer_path, deps)| {
                let nm = if importer_path == "." {
                    root_nm.clone()
                } else {
                    root_dir.join(importer_path).join(&self.modules_dir_name)
                };
                deps.iter().map(move |dep| Step2Task {
                    importer_path: importer_path.as_str(),
                    nm: nm.clone(),
                    dep,
                })
            })
            .collect();

        let link_parallelism = self.link_parallelism();
        let step2_timer = std::time::Instant::now();
        let step2_results: Vec<Result<bool, Error>> = with_link_pool(link_parallelism, || {
            tasks
                .par_iter()
                .map(|task| {
                    let Step2Task {
                        importer_path,
                        nm,
                        dep,
                    } = task;

                    // `dedupeDirectDeps`: non-root importer dep
                    // already covered by the root symlink +
                    // parent-directory walk.
                    if self.dedupe_direct_deps
                        && *importer_path != "."
                        && let Some(root_dep) = root_deps_by_name.get(dep.name.as_str())
                        && root_dep.dep_path == dep.dep_path
                    {
                        return Ok(false);
                    }

                    let link_path = nm.join(&dep.name);

                    // Workspace dep (`workspace:` protocol): link
                    // straight into the sibling package dir.
                    if let Some(ws_dir) = workspace_dirs.get(&dep.name) {
                        if !self.hoist_workspace_packages {
                            return Ok(false);
                        }
                        let link_parent = link_path.parent().unwrap_or(nm);
                        let rel_target =
                            pathdiff::diff_paths(ws_dir, link_parent).unwrap_or(ws_dir.clone());
                        if reconcile_top_level_link(&link_path, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = link_path.parent() {
                            mkdirp(parent)?;
                        }
                        sys::create_dir_link(&rel_target, &link_path)
                            .map_err(|e| Error::Io(link_path.clone(), e))?;
                        return Ok(true);
                    }

                    // `link:` dep — absolute path relative to `root_dir`.
                    if let Some(locked) = graph.packages.get(&dep.dep_path)
                        && let Some(LocalSource::Link(rel)) = locked.local_source.as_ref()
                    {
                        let abs_target = root_dir.join(rel);
                        let link_parent = link_path.parent().unwrap_or(nm);
                        let rel_target =
                            pathdiff::diff_paths(&abs_target, link_parent).unwrap_or(abs_target);
                        if reconcile_top_level_link(&link_path, &rel_target)? {
                            return Ok(false);
                        }
                        if let Some(parent) = link_path.parent() {
                            mkdirp(parent)?;
                        }
                        sys::create_dir_link(&rel_target, &link_path)
                            .map_err(|e| Error::Io(link_path.clone(), e))?;
                        return Ok(true);
                    }

                    // Regular registry dep — symlink to the root
                    // `.aube/<dep_path>/node_modules/<name>`.
                    let source_dir = aube_dir
                        .join(self.aube_dir_entry_name(&dep.dep_path))
                        .join("node_modules")
                        .join(&dep.name);
                    if !source_dir.exists() {
                        return Ok(false);
                    }
                    let link_parent = link_path.parent().unwrap_or(nm);
                    let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                        .unwrap_or_else(|| source_dir.clone());
                    if reconcile_top_level_link(&link_path, &rel_target)? {
                        return Ok(false);
                    }
                    if let Some(parent) = link_path.parent() {
                        mkdirp(parent)?;
                    }
                    sys::create_dir_link(&rel_target, &link_path)
                        .map_err(|e| Error::Io(link_path.clone(), e))?;
                    trace!("workspace top-level: {} -> {}", dep.name, importer_path);
                    Ok(true)
                })
                .collect()
        });
        for result in step2_results {
            if result? {
                stats.top_level_linked += 1;
            }
        }
        tracing::debug!(
            "link_workspace:step2 (top-level symlinks) {:.1?}",
            step2_timer.elapsed()
        );

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

        // Hidden hoist is shared across importers, so a single sweep
        // here is sufficient for the whole workspace.
        self.link_hidden_hoist(&aube_dir, graph)?;

        if let Err(e) = write_applied_patches(&root_nm, &curr_applied) {
            tracing::error!(
                "failed to write .aube-applied-patches.json: {e}. next install may miss stale patched entries"
            );
        }
        Ok(stats)
    }

    /// Populate (or sweep) the hidden modules directories at
    /// `aube_dir/node_modules/<name>` and, in global-virtual-store mode,
    /// `virtual_store/node_modules/<name>`. When `self.hoist` is
    /// enabled, walks every non-local package in the graph and creates
    /// a symlink for names that match `hoist_patterns` into each
    /// corresponding virtual-store package entry.
    /// When disabled, wipes the directory so previously-hoisted
    /// symlinks don't keep resolving through Node's parent walk.
    ///
    /// Unlike `hoist_remaining_into`, this writes into a private
    /// sibling of `.aube/<dep_path>/` rather than the visible root
    /// `node_modules/`. Packages inside the virtual store (e.g.
    /// `.aube/react@18/node_modules/react/`) walk up through
    /// `.aube/node_modules/` during require resolution, which is the
    /// only consumer of these links — nothing inside the user's own
    /// `node_modules/<name>` view is affected. In GVS mode, many
    /// toolchains canonicalize the package path into
    /// `~/.cache/aube/virtual-store/<hash>/node_modules/<name>`, so we
    /// mirror the hidden hoist under the shared virtual-store root too.
    fn link_hidden_hoist(&self, aube_dir: &Path, graph: &LockfileGraph) -> Result<(), Error> {
        self.link_hidden_hoist_at(aube_dir, aube_dir, graph, false, true)?;
        if self.use_global_virtual_store {
            self.link_hidden_hoist_at(
                &self.virtual_store,
                &self.virtual_store,
                graph,
                true,
                false,
            )?;
        }
        Ok(())
    }

    fn link_hidden_hoist_at(
        &self,
        hidden_root: &Path,
        source_root: &Path,
        graph: &LockfileGraph,
        use_hashed_subdirs: bool,
        sweep_stale_entries: bool,
    ) -> Result<(), Error> {
        let hidden = hidden_root.join("node_modules");
        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let packages: Vec<_> = if self.hoist {
            graph
                .packages
                .iter()
                .filter_map(|(dep_path, pkg)| {
                    if pkg.local_source.is_some() || !self.hoist_matches(&pkg.name) {
                        return None;
                    }
                    // First-writer-wins on name clashes across versions.
                    // BTree iteration over `graph.packages` gives a
                    // deterministic tiebreaker across runs.
                    claimed.insert(pkg.name.clone()).then_some((dep_path, pkg))
                })
                .collect()
        } else {
            Vec::new()
        };

        if !self.hoist {
            // Previous install may have populated this tree with
            // hoist=true. Drop entries so Node doesn't keep resolving
            // phantom deps through the stale symlinks. Project-local
            // hidden hoist owns the whole tree and can remove it in
            // one shot; the shared GVS mirror only reclaims broken
            // entries because live links may belong to another project.
            if sweep_stale_entries {
                remove_hidden_hoist_tree(&hidden);
            } else {
                sweep_dead_hidden_hoist_entries(&hidden);
            }
            return Ok(());
        }
        // Wipe before repopulating so a dependency removed from the
        // graph (or a pattern that no longer matches) doesn't linger.
        // The shared GVS hidden hoist only prunes broken entries:
        // removing live cross-project links would make the directory
        // last-writer-wins for sequential installs.
        if sweep_stale_entries {
            remove_hidden_hoist_tree(&hidden);
        } else {
            sweep_dead_hidden_hoist_entries(&hidden);
        }
        for (dep_path, pkg) in packages {
            let source_subdir = if use_hashed_subdirs {
                self.virtual_store_subdir(dep_path)
            } else {
                self.aube_dir_entry_name(dep_path)
            };
            let source_dir = source_root
                .join(source_subdir)
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                continue;
            }
            let target_dir = hidden.join(&pkg.name);
            if let Some(parent) = target_dir.parent() {
                mkdirp(parent)?;
            }
            let link_parent = target_dir.parent().unwrap_or(&hidden);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            if reconcile_top_level_link(&target_dir, &rel_target)? {
                continue;
            }
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
    /// entry.
    ///
    /// Two separate "first-write-wins" protections apply:
    ///
    /// - **Direct deps always win over hoisted transitives.** Names
    ///   that appear in `graph.root_deps()` were placed (or
    ///   deliberately skipped) by Step 2 and must never be overwritten
    ///   by a hoist pass — that would silently swap `node_modules/foo`
    ///   from the version the user pinned to whatever transitive
    ///   happened to sort first.
    /// - **Within the hoist pass, BTree iteration order is the
    ///   tiebreaker across versions.** The `claimed` set records
    ///   names we already hoisted this call so a later iteration with
    ///   the same name (different `dep_path`) doesn't clobber the
    ///   first winner.
    ///
    /// For everything else the caller gets a *target-aware* reconcile:
    /// an existing symlink at `nm/<name>` that points at the version
    /// this iteration wants is kept; one pointing at a stale
    /// `.aube/<old-dep-path>/` (leftover from a prior install whose
    /// hoisted version has since changed) is replaced. The old
    /// plain-`exists?` check here kept stale entries because the
    /// surrounding linker used to wipe `nm` unconditionally — now that
    /// we sweep surgically, hoist has to cope with partial priors.
    ///
    /// `trace_label` distinguishes the `link_all` vs `link_workspace`
    /// callers in `-v` output.
    fn hoist_remaining_into(
        &self,
        nm: &Path,
        aube_dir: &Path,
        graph: &LockfileGraph,
        stats: &mut LinkStats,
        trace_label: &str,
        select: &dyn Fn(&str) -> bool,
    ) -> Result<(), Error> {
        // Root direct-dep names. Populated from the importer map
        // rather than an opaque "touched by Step 2" signal so a direct
        // dep that *failed* to place (missing `source_dir.exists()`,
        // workspace toggle, etc.) still reserves its slot — pnpm
        // doesn't hoist over a direct dep even when the direct dep
        // couldn't be installed.
        let direct_dep_names: std::collections::HashSet<&str> =
            graph.root_deps().iter().map(|d| d.name.as_str()).collect();

        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (dep_path, pkg) in &graph.packages {
            if pkg.local_source.is_some() {
                continue;
            }
            if !select(&pkg.name) {
                continue;
            }
            // Direct deps always win over hoisting.
            if direct_dep_names.contains(pkg.name.as_str()) {
                continue;
            }
            // First-writer-wins within the hoist pass: if an earlier
            // iteration already hoisted this name, later iterations
            // with the same name don't overwrite it.
            if !claimed.insert(pkg.name.clone()) {
                continue;
            }
            let source_dir = aube_dir
                .join(self.aube_dir_entry_name(dep_path))
                .join("node_modules")
                .join(&pkg.name);
            if !source_dir.exists() {
                // Don't remove `name` from `claimed` — another
                // iteration for the same name would also find its
                // `source_dir` missing (the `.aube` populate phase
                // runs before hoist for every package), and leaving
                // the name claimed preserves the existing symlink
                // (whatever it points at) instead of repeatedly
                // probing for a materialization that isn't coming.
                continue;
            }
            let target_dir = nm.join(&pkg.name);
            let link_parent = target_dir.parent().unwrap_or(nm);
            let rel_target = pathdiff::diff_paths(&source_dir, link_parent)
                .unwrap_or_else(|| source_dir.clone());
            if reconcile_top_level_link(&target_dir, &rel_target)? {
                continue;
            }
            if let Some(parent) = target_dir.parent() {
                mkdirp(parent)?;
            }
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
        // `link:` transitives the resolver pinned (e.g. via root
        // `pnpm.overrides`) need their on-disk target so the parent's
        // sibling symlink doesn't dangle into a non-existent
        // `.aube/<name>@link+...`. `None` means "no nested links in
        // this graph" and the materialize hot path stays unchanged.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
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

        let result = self.materialize_into(
            &tmp_base,
            dep_path,
            pkg,
            index,
            stats,
            true,
            nested_link_targets,
        );

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&tmp_base);
            return result;
        }

        // Atomically move the dep_path entry from the temp dir to the final location.
        let tmp_entry = tmp_base.join(&subdir);
        let final_entry = self.virtual_store.join(&subdir);

        // Ensure the parent of the final entry exists (e.g. for scoped packages).
        if let Some(parent) = final_entry.parent() {
            mkdirp(parent)?;
        }

        match aube_util::fs_atomic::rename_with_retry(&tmp_entry, &final_entry) {
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
    #[allow(clippy::too_many_arguments)]
    fn materialize_into(
        &self,
        base_dir: &Path,
        dep_path: &str,
        pkg: &LockedPackage,
        index: &PackageIndex,
        stats: &mut LinkStats,
        apply_hashes: bool,
        // dep_path → absolute on-disk target for any `link:` packages
        // referenced as transitive deps. When the parent itself is a
        // `file:` Directory or `link:` Link (workspace-style locals),
        // its `package.json` may declare `link:./libs/foo` deps that
        // point inside the parent's source tree. We sidestep the
        // virtual store for those — there is no `.aube/<dep>@link+...`
        // entry — and symlink straight to the on-disk path the
        // resolver pinned. `None` means "no nested link transitives in
        // this graph", which is the common case.
        nested_link_targets: Option<&BTreeMap<String, PathBuf>>,
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
        // Validate every key once here. The file-linking loop below
        // walks the same immutable index, so skipping the check
        // there is safe.
        for rel_path in index.keys() {
            validate_index_key(rel_path)?;
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
            // Key already validated in the parent-collection loop
            // above. The index is immutable between the two loops.
            let target = pkg_nm_dir.join(rel_path);

            if let Err(e) = self.link_file_fresh(stored, rel_path, &target) {
                if let Error::MissingStoreFile { .. } = &e {
                    invalidate_stale_index_for_package(&self.store, pkg);
                }
                return Err(e);
            }
            stats.files_linked += 1;

            if stored.executable {
                // `create_cas_file` writes every CAS entry as 0o644
                // unconditionally; the only place a CAS entry's
                // shared inode gets the +x bit is the very first
                // `make_executable` call against a hardlinked or
                // reflinked target — that `chmod` upgrades the
                // shared inode for every later linker that points
                // at it. Skipping the call (an earlier optimization)
                // produced 0o644 binaries on cold installs and
                // broke every CLI shipped via npm.
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
        let patch_key = pkg.spec_key();
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
            // Skip any dep whose name matches the package being
            // materialized, regardless of version. The symlink would
            // land at `pkg_nm_parent.join(dep_name)` which is exactly
            // `pkg_nm_dir` — the directory we just populated with the
            // package's own files — and `create_dir_link` would fail
            // EEXIST. The skip used to require version-equality too,
            // but published packages occasionally declare a *different*
            // version of themselves as a dep (e.g. `react_ujs@3.3.0`
            // pins `react_ujs@^2.7.1`, an artifact of how its build
            // script generates its package.json). Treat that as a
            // self-reference: `require('<self>')` from inside the
            // package resolves to its own files, matching what npm /
            // pnpm / yarn end up with after their hoisting passes.
            if dep_name == &pkg.name {
                continue;
            }
            let symlink_path = pkg_nm_parent.join(dep_name);
            // `link:` transitive: the resolver pinned an absolute
            // on-disk target. Skip the virtual-store sibling lookup
            // (there is no `.aube/<dep>@link+...` entry for these) and
            // symlink straight at the source directory.
            if let Some(map) = nested_link_targets
                && let Some(abs_target) = map.get(&dep_dep_path)
            {
                let link_parent = symlink_path.parent().unwrap_or(&pkg_nm_parent);
                let target = pathdiff::diff_paths(abs_target, link_parent)
                    .unwrap_or_else(|| abs_target.clone());
                sys::create_dir_link(&target, &symlink_path)
                    .map_err(|e| Error::Io(symlink_path.clone(), e))?;
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

            // GVS materialize writes into `.tmp-<pid>-<subdir>/`, then
            // atomic-renames into `self.virtual_store/<subdir>/`. POSIX
            // symlinks store the relative offset verbatim. Offset stays
            // invariant under the wrapper rename, so the link resolves
            // correctly after the move. Windows junctions resolve the
            // target against `link.parent()` at create time and persist
            // an absolute path, which binds the junction to the tmp
            // wrapper. After rename every sibling link dangles into a
            // gone `.tmp-<pid>-...` path. Fix: on Windows GVS path
            // (`apply_hashes = true`) rewrite the target to point at
            // the final virtual store root so the stored absolute path
            // survives the rename.
            #[cfg(windows)]
            let target = if apply_hashes {
                self.virtual_store
                    .join(&sibling_subdir)
                    .join("node_modules")
                    .join(dep_name)
            } else {
                target
            };

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
    pub(crate) fn link_file_fresh(
        &self,
        stored: &StoredFile,
        rel_path: &str,
        dst: &Path,
    ) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        const SMALL_FILE_COPY_MAX: u64 = 16 * 1024;
        let map_io = |e: std::io::Error| classify_link_error(stored, rel_path, dst, e);
        let missing_source = || Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
        match self.strategy {
            LinkStrategy::Reflink => {
                #[cfg(target_os = "macos")]
                if matches!(stored.size, Some(size) if size <= SMALL_FILE_COPY_MAX) {
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                    return Ok(());
                }
                if let Err(e) = reflink_copy::reflink(&stored.store_path, dst) {
                    // Source-missing short-circuit avoids the misleading
                    // "fell back to copy" trace and the redundant copy
                    // attempt that would just ENOENT for the same reason.
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // Fall back to copy on cross-filesystem errors
                    trace!("reflink failed, falling back to copy: {e}");
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                }
            }
            LinkStrategy::Hardlink => {
                if let Err(e) = std::fs::hard_link(&stored.store_path, dst) {
                    if !stored.store_path.exists() {
                        return Err(missing_source());
                    }
                    // Fall back to copy on cross-filesystem errors (EXDEV)
                    trace!("hardlink failed, falling back to copy: {e}");
                    std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
                }
            }
            LinkStrategy::Copy => {
                std::fs::copy(&stored.store_path, dst).map_err(map_io)?;
            }
        }

        Ok(())
    }
}

/// Translate a copy failure into the most informative linker error.
/// ENOENT can mean either side of the operation is missing — stat the
/// source CAS shard to attribute it. A missing shard means the cached
/// package index is out of sync with the on-disk store, which the
/// caller can recover from by invalidating the cached index and
/// re-importing the tarball.
fn classify_link_error(
    stored: &StoredFile,
    rel_path: &str,
    dst: &Path,
    err: std::io::Error,
) -> Error {
    if err.kind() == std::io::ErrorKind::NotFound && !stored.store_path.exists() {
        return Error::MissingStoreFile {
            store_path: stored.store_path.clone(),
            rel_path: rel_path.to_string(),
        };
    }
    Error::Io(dst.to_path_buf(), err)
}

/// Best-effort drop the cached package index when materialize discovers
/// its referenced CAS shard is gone. Callers always surface the original
/// `MissingStoreFile` error first; this side effect just makes sure the
/// next install miss `load_index` instead of looping on the same dead
/// reference. If the cache write fails (e.g. permission error), warn
/// loudly so the user knows the auto-recovery didn't take and they need
/// to wipe `~/.cache/aube/index/` by hand.
pub(crate) fn invalidate_stale_index_for_package(store: &aube_store::Store, pkg: &LockedPackage) {
    match store.invalidate_cached_index(pkg.registry_name(), &pkg.version, pkg.integrity.as_deref())
    {
        Ok(true) => debug!("invalidated stale index for {}", pkg.spec_key()),
        Ok(false) => {}
        Err(e) => warn!(
            "failed to invalidate stale index for {}: {e}; manual recovery: rm -rf ~/.cache/aube/index",
            pkg.spec_key()
        ),
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
    #[error("refusing to materialize unsafe index key: {0:?}")]
    UnsafeIndexKey(String),
    #[error(
        "cached package index references a missing CAS shard at {store_path} (file: {rel_path:?}). The store and its index cache are out of sync — rerun the install to re-fetch the tarball."
    )]
    MissingStoreFile {
        store_path: PathBuf,
        rel_path: String,
    },
}

/// Defence in depth for the tarball path-traversal class. The
/// primary guard lives in `aube_store::import_tarball`, which
/// refuses malformed entries before they enter the `PackageIndex`.
/// This helper is the last check before `base.join(key)` is
/// written through the linker, so an index loaded from a cache
/// file that predates the store-side validation (or a bug that
/// lets a traversing key slip past it) still cannot produce a
/// file outside the package root.
fn validate_index_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || key.starts_with('/')
        || key.starts_with('\\')
        || key.contains('\0')
        || key.contains('\\')
    {
        return Err(Error::UnsafeIndexKey(key.to_string()));
    }
    // Reject any `..` component or Windows drive prefix like `C:`
    // that would make `Path::join` escape the base.
    for component in std::path::Path::new(key).components() {
        match component {
            std::path::Component::ParentDir
            | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {
                return Err(Error::UnsafeIndexKey(key.to_string()));
            }
            std::path::Component::Normal(os) => {
                #[cfg(windows)]
                {
                    if let Some(s) = os.to_str()
                        && s.contains(':')
                    {
                        return Err(Error::UnsafeIndexKey(key.to_string()));
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = os;
                }
            }
            std::path::Component::CurDir => {}
        }
    }
    Ok(())
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
/// Reconcile a top-level `node_modules/<name>` entry against the
/// expected symlink target. Compares the link's *target* — a version
/// upgrade that leaves `.aube/<old-dep-path>/` resolvable on disk is
/// correctly classified as stale instead of silently keeping the old
/// symlink.
///
/// - `Ok(true)`  – existing entry is a symlink pointing at
///   `expected_target`; caller skips creation.
/// - `Ok(false)` – no entry exists, or a stale entry (wrong target,
///   dangling symlink, regular directory) has been best-effort
///   removed; caller should proceed to create the symlink.
///
/// Unix and Windows use different comparison strategies because
/// `create_dir_link` writes the target differently on each platform:
/// Unix preserves the relative target bytes-for-bytes as a POSIX
/// symlink, Windows normalizes to an absolute path before calling
/// `junction::create`. A plain `read_link == expected` check that
/// works on Unix would miss every warm run on Windows.
fn reconcile_top_level_link(link_path: &Path, expected_target: &Path) -> Result<bool, Error> {
    #[cfg(windows)]
    {
        // NTFS junctions store normalized absolute targets
        // (sometimes `\\?\`-prefixed), so comparing against the
        // relative `pathdiff::diff_paths` output the callers compute
        // would never match. Compare the canonical forms instead: if
        // the junction resolves to the same directory
        // `expected_target` points at, the link is fresh. Anything
        // else (dangling, wrong target, not a reparse point) falls
        // through to a best-effort reclaim.
        //
        // Canonicalize is ~5 syscalls on NTFS (open reparse, read
        // reparse data, close, query attrs ×2). With ~1000 top-level
        // links per warm install that's 5000 syscalls just for
        // expected_abs. Cache canonical forms keyed by the absolute
        // path so a second call to the same target returns
        // immediately.
        use std::sync::OnceLock;
        static CANON_CACHE: OnceLock<
            std::sync::RwLock<std::collections::HashMap<PathBuf, PathBuf>>,
        > = OnceLock::new();
        fn cached_canonicalize(p: &Path) -> std::io::Result<PathBuf> {
            let map = CANON_CACHE.get_or_init(Default::default);
            if let Some(hit) = map.read().expect("canon cache poisoned").get(p) {
                return Ok(hit.clone());
            }
            let canon = p.canonicalize()?;
            map.write()
                .expect("canon cache poisoned")
                .insert(p.to_path_buf(), canon.clone());
            Ok(canon)
        }
        let expected_abs = if expected_target.is_absolute() {
            expected_target.to_path_buf()
        } else {
            let parent = link_path.parent().unwrap_or_else(|| Path::new(""));
            parent.join(expected_target)
        };
        if let Ok(link_canon) = cached_canonicalize(link_path)
            && let Ok(exp_canon) = cached_canonicalize(&expected_abs)
            && link_canon == exp_canon
        {
            return Ok(true);
        }
        if link_path.symlink_metadata().is_err() {
            return Ok(false);
        }
        match std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path)) {
            Ok(()) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Error::Io(link_path.to_path_buf(), e)),
        }
    }
    #[cfg(not(windows))]
    {
        match std::fs::read_link(link_path) {
            Ok(existing) if existing == expected_target => Ok(true),
            Ok(_) => {
                // Wrong target — remove the stale symlink so the
                // caller's `create_dir_link` below doesn't EEXIST.
                let _ = std::fs::remove_dir(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => {
                // `read_link` failed with EINVAL (entry exists but
                // isn't a symlink — e.g. a regular directory left by
                // a prior hoisted install) or another error.
                // Best-effort reclaim so the create call lands on a
                // clean slot.
                let _ =
                    std::fs::remove_dir_all(link_path).or_else(|_| std::fs::remove_file(link_path));
                Ok(false)
            }
        }
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

/// Build a `dep_path → absolute on-disk target` map for every
/// `LocalSource::Link` in the graph. Returned `None` when the graph
/// has no link entries (vast majority of installs), so the materialize
/// hot path can short-circuit without a per-dep lookup.
pub fn build_nested_link_targets(
    project_dir: &Path,
    graph: &LockfileGraph,
) -> Option<BTreeMap<String, PathBuf>> {
    let map: BTreeMap<String, PathBuf> = graph
        .packages
        .iter()
        .filter_map(|(dp, pkg)| match pkg.local_source.as_ref() {
            Some(LocalSource::Link(rel)) => Some((dp.clone(), project_dir.join(rel))),
            _ => None,
        })
        .collect();
    if map.is_empty() { None } else { Some(map) }
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
    // Old code hand-rolled JSON with split(',') and trim_matches('"').
    // Breaks on any key or value containing a comma, escaped quote,
    // or backslash. Keys are name@version strings today but patch
    // content hashes are fine, and if pnpm ever extends the key
    // schema this blows up. Real JSON parser handles escaping.
    serde_json::from_str(s).ok()
}

/// Write the applied-patch sidecar.
///
/// Next install reads this to compute which `.aube/<dep_path>`
/// entries need re-materializing because their patch set changed.
/// Old code was `let _ = fs::write(...)`, dropped any IO error. If
/// write silently failed (disk full, read-only mount, perms), the
/// sidecar was missing on next install, and
/// wipe_changed_patched_entries did not know which entries to
/// re-link. Install reported success while node_modules had stale
/// patched content on disk. Return Result, caller logs loudly.
fn write_applied_patches(
    nm_dir: &Path,
    map: &std::collections::BTreeMap<String, String>,
) -> std::io::Result<()> {
    let path = nm_dir.join(".aube-applied-patches.json");
    let out = serde_json::to_string(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    aube_util::fs_atomic::atomic_write(&path, out.as_bytes())
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
        let key = pkg.spec_key();
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
fn is_safe_rel_component(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute()
        || p.has_root()
        || rel.starts_with('/')
        || rel.len() >= 2 && rel.as_bytes()[1] == b':'
    {
        return false;
    }
    p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

fn ensure_no_symlink_in_chain(pkg_dir: &Path, rel: &str) -> Result<(), String> {
    let mut cursor = pkg_dir.to_path_buf();
    for comp in Path::new(rel).components() {
        cursor.push(comp);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(format!("{}", cursor.display()));
                }
                // Junctions on Windows are `IO_REPARSE_TAG_MOUNT_POINT`
                // reparse points, not `IO_REPARSE_TAG_SYMLINK`, and
                // `FileType::is_symlink()` returns false for them.
                // Catch every reparse point via the file-attribute
                // bit so a junction can't sneak the patch out of the
                // package directory.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(format!("{}", cursor.display()));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("stat {}: {e}", cursor.display())),
        }
    }
    Ok(())
}

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
        // Refuse patch headers that escape the package directory.
        // A hostile diff with `b/../../etc/shadow` as the target
        // would otherwise let the patch step overwrite or delete
        // files outside the installed package. Same rules we apply
        // to tar entries over in aube-store (no absolute, no drive
        // prefix, no `..`, no backslash, no NUL).
        if !is_safe_rel_component(rel) {
            return Err(format!("patch file path escapes package: {rel:?}"));
        }
        // Walk every parent component of the target on disk and refuse
        // to follow any symlink or junction. Without this guard, a
        // package that planted a directory link inside its own tree
        // (or a workspace where the user has a symlinked dep dir)
        // would let `pkg_dir.join(rel)` resolve through the link, and
        // `atomic_write` would overwrite a file outside `pkg_dir`.
        // CVE-2018-1000156 (GNU patch) class.
        if let Err(e) = ensure_no_symlink_in_chain(pkg_dir, rel) {
            return Err(format!("patch target contains symlink: {e}"));
        }
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
        // git-style patches always use LF line endings, but published
        // tarballs frequently ship files with CRLF (Windows editors,
        // `core.autocrlf=true` checkouts). Diffy is byte-exact and
        // refuses to match CRLF context against LF hunk lines, so we
        // normalize the original to LF before applying and restore the
        // CRLF on write. pnpm's patch applier does the same thing.
        let was_crlf = original.contains("\r\n");
        let normalized = if was_crlf {
            original.replace("\r\n", "\n")
        } else {
            original
        };
        let parsed = diffy::Patch::from_str(&section.body)
            .map_err(|e| format!("failed to parse patch for {rel}: {e}"))?;
        let patched_lf = diffy::apply(&normalized, &parsed)
            .map_err(|e| format!("failed to apply patch for {rel}: {e}"))?;
        let patched = if was_crlf {
            // Promote bare `\n` to `\r\n`, then collapse any `\r\r\n`
            // back so a patch line containing a literal `\r` byte (rare
            // but legal for binary-ish text) doesn't gain a second CR.
            patched_lf.replace('\n', "\r\n").replace("\r\r\n", "\r\n")
        } else {
            patched_lf
        };
        // Break any reflink/hardlink to the global store before
        // writing the patched bytes — otherwise we'd silently mutate
        // every other project sharing this CAS file. Stage the write
        // through a sibling tempfile and `rename` into place so a
        // crash or Ctrl-C mid-patch cannot leave the package with
        // the original file unlinked and no replacement written.
        // POSIX `rename(2)` atomically replaces the destination, so
        // no pre-removal is needed and removing first would create
        // the exact TOCTOU window the rename is supposed to close.
        // Windows `MoveFileExW` fails when the destination exists,
        // so the unlink is gated behind `cfg(windows)`.
        #[cfg(windows)]
        {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("failed to unlink {}: {e}", target.display()))?;
            }
        }
        aube_util::fs_atomic::atomic_write(&target, patched.as_bytes()).map_err(|e| {
            format!(
                "failed to write patched file into place {}: {e}",
                target.display()
            )
        })?;
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
fn parse_diff_git_b_path(rest: &str) -> Option<String> {
    if let Some(after) = rest.strip_prefix("\"a/") {
        let end_a = after.find("\" \"b/")?;
        let after_b = &after[end_a + 5..];
        let close = after_b.rfind('"')?;
        return unescape_git_quoted(&after_b[..close]);
    }
    let body = rest.strip_prefix("a/")?;
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(" b/") {
        let abs = search_from + rel;
        let path_a = &body[..abs];
        let path_b = &body[abs + 3..];
        if path_a == path_b {
            return Some(path_b.to_string());
        }
        search_from = abs + 1;
    }
    body.find(" b/").map(|i| body[i + 3..].to_string())
}

fn unescape_git_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            return None;
        }
        match bytes[i + 1] {
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0C);
                i += 2;
            }
            b'v' => {
                out.push(0x0B);
                i += 2;
            }
            d0 @ b'0'..=b'3'
                if i + 3 < bytes.len()
                    && (b'0'..=b'7').contains(&bytes[i + 2])
                    && (b'0'..=b'7').contains(&bytes[i + 3]) =>
            {
                let n = ((d0 - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
                out.push(n);
                i += 4;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

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
        let stripped = line.trim_end_matches(['\n', '\r']);
        if let Some(rest) = stripped.strip_prefix("diff --git ") {
            // New file boundary — flush whatever we were collecting.
            flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
            in_body = false;
            // Parse `a/<path> b/<path>` and prefer the post-edit
            // (`b/`) path so renames land on the new name.
            current_path = parse_diff_git_b_path(rest);
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
                    body.push_str(stripped);
                    body.push('\n');
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
        body.push_str(stripped);
        body.push('\n');
    }
    flush(&mut out, &mut current_path, &mut body, &mut is_deletion);
    out
}

#[cfg(test)]
mod importer_classification_tests {
    use super::is_physical_importer;

    #[test]
    fn root_is_physical() {
        assert!(is_physical_importer("."));
    }

    #[test]
    fn workspace_paths_are_physical() {
        assert!(is_physical_importer("packages/dev/core"));
        assert!(is_physical_importer("apps/web"));
        assert!(is_physical_importer("libs/@scope/name"));
    }

    #[test]
    fn nested_peer_context_paths_are_virtual() {
        // pnpm v9 emits these for every peer-resolution view reachable
        // through the workspace symlink chain. They describe the graph,
        // they are not directories to populate.
        assert!(!is_physical_importer(
            "packages/dev/addons/node_modules/@dev/core"
        ));
        assert!(!is_physical_importer(
            "packages/a/node_modules/@s/b/node_modules/@s/c"
        ));
    }
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

    #[cfg(windows)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_junction_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        junction::create(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse junction-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_symlink_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse symlink-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

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

    #[test]
    fn crlf_patch_path_does_not_carry_carriage_return() {
        let patch = "diff --git a/index.js b/index.js\r\n\
                     --- a/index.js\r\n\
                     +++ b/index.js\r\n\
                     @@ -1 +1 @@\r\n\
                     -module.exports = 'old';\r\n\
                     +module.exports = 'new';\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("index.js"));
    }

    #[test]
    fn crlf_deletion_patch_recognized() {
        let patch = "diff --git a/removed.js b/removed.js\r\n\
                     deleted file mode 100644\r\n\
                     --- a/removed.js\r\n\
                     +++ /dev/null\r\n\
                     @@ -1 +0,0 @@\r\n\
                     -gone\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].is_deletion);
    }

    #[test]
    fn diff_git_path_with_space_b_substring() {
        let patch = "diff --git a/a b/c.js b/a b/c.js\n\
                     --- a/a b/c.js\n\
                     +++ b/a b/c.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("a b/c.js"));
    }

    #[test]
    fn diff_git_quoted_path_form() {
        let patch = "diff --git \"a/path with spaces.js\" \"b/path with spaces.js\"\n\
                     --- a/path with spaces.js\n\
                     +++ b/path with spaces.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("path with spaces.js"));
    }

    #[test]
    fn applies_lf_patch_against_crlf_file() {
        // Tarballs published from Windows editors ship CRLF text. pnpm
        // / git emit LF-only patches even against those files. Diffy is
        // byte-exact, so the apply path normalizes CRLF -> LF before
        // matching and restores CRLF on write.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();

        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,3 +1,3 @@\n\
                     \x20one\n\
                     -two\n\
                     +TWO\n\
                     \x20three\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"one\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn crlf_restore_preserves_embedded_cr_byte() {
        // A patch line that adds a literal `\r` byte mid-line must not
        // gain a second `\r` when we re-CRLF the output. Naive
        // `replace('\n', "\r\n")` would turn `\r\n` into `\r\r\n`; the
        // `\r\r\n` collapse undoes that.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\n").unwrap();
        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,2 +1,2 @@\n\
                     -one\n\
                     +has\rcr\n\
                     \x20two\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"has\rcr\r\ntwo\r\n");
    }

    #[test]
    fn diff_git_quoted_path_unescapes_git_escapes() {
        let path = parse_diff_git_b_path(r#""a/foo\".js" "b/foo\".js""#).expect("quoted parse");
        assert_eq!(path, "foo\".js");
        let path = parse_diff_git_b_path(r#""a/back\\slash.js" "b/back\\slash.js""#)
            .expect("backslash parse");
        assert_eq!(path, "back\\slash.js");
        let path = parse_diff_git_b_path("\"a/caf\\303\\251.js\" \"b/caf\\303\\251.js\"")
            .expect("octal parse");
        assert_eq!(path, "café.js");
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
    fn test_link_all_handles_self_referential_dep_at_different_version() {
        // `react_ujs@3.3.0` (and other publish-script artifacts)
        // declares its own name as a dep at a *different* version
        // (`react_ujs: ^2.7.1`). The transitive-symlink pass would
        // try to create a symlink at `node_modules/react_ujs`,
        // which is exactly where the package's own files live —
        // EEXIST. Skip self-name deps regardless of version so
        // these install cleanly. `require('<self>')` from inside
        // the package then resolves to its own files, matching how
        // npm / pnpm / yarn end up after their hoisting passes.
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let store = Store::at(dir.path().join("store/files"));

        let mut indices = BTreeMap::new();
        let host_index_js = store.import_bytes(b"/* react_ujs 3.3.0 */", false).unwrap();
        let host_pkg_json = store
            .import_bytes(b"{\"name\":\"react_ujs\",\"version\":\"3.3.0\"}", false)
            .unwrap();
        let mut host_index = BTreeMap::new();
        host_index.insert("index.js".to_string(), host_index_js);
        host_index.insert("package.json".to_string(), host_pkg_json);
        indices.insert("react_ujs@3.3.0".to_string(), host_index);

        let mut host_deps = BTreeMap::new();
        // Self-reference at a different version, the shape that
        // triggered the EEXIST bug.
        host_deps.insert("react_ujs".to_string(), "^2.7.1".to_string());

        let mut packages = BTreeMap::new();
        packages.insert(
            "react_ujs@3.3.0".to_string(),
            LockedPackage {
                name: "react_ujs".to_string(),
                version: "3.3.0".to_string(),
                integrity: None,
                dependencies: host_deps,
                dep_path: "react_ujs@3.3.0".to_string(),
                ..Default::default()
            },
        );

        let mut importers = BTreeMap::new();
        importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "react_ujs".to_string(),
                dep_path: "react_ujs@3.3.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let graph = LockfileGraph {
            importers,
            packages,
            ..Default::default()
        };

        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let stats = linker
            .link_all(&project_dir, &graph, &indices)
            .expect("install must succeed despite self-named dep");
        assert_eq!(stats.packages_linked, 1);
        let host_index =
            project_dir.join("node_modules/.aube/react_ujs@3.3.0/node_modules/react_ujs/index.js");
        assert!(host_index.exists(), "host package files must be present");
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
    fn test_link_file_fresh_reports_missing_cas_shard_and_invalidates_cache() {
        // Reproduces endevco/aube#393: a partially corrupt CAS leaves the
        // cached package index pointing at a missing shard. Materialize
        // must distinguish "source CAS file missing" from a generic ENOENT
        // and drop the stale index JSON so the next install re-imports
        // the tarball.
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        // Persist foo's index so invalidate_cached_index has something
        // to remove. Real installs save indices via the fetch path.
        let foo_index = indices.get("foo@1.0.0").unwrap();
        store.save_index("foo", "1.0.0", None, foo_index).unwrap();
        let cached_path = store.index_dir().join("foo@1.0.0.json");
        assert!(
            cached_path.exists(),
            "test setup: index cache must be written"
        );

        // Delete the CAS shard for foo's package.json (matches the
        // failure mode in #393 where one shard is missing while others
        // remain).
        let pkgjson_store_path = foo_index.get("package.json").unwrap().store_path.clone();
        std::fs::remove_file(&pkgjson_store_path).unwrap();

        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let graph = make_graph();
        let err = linker
            .link_all(&project_dir, &graph, &indices)
            .expect_err("link must fail when a referenced CAS shard is gone");
        assert!(
            matches!(&err, Error::MissingStoreFile { rel_path, .. } if rel_path == "package.json"),
            "expected MissingStoreFile {{ rel_path: \"package.json\" }}, got {err:?}"
        );

        // Side effect: cached index dropped, so the next install will
        // miss load_index and re-fetch instead of looping on the same
        // dead shard reference.
        assert!(
            !cached_path.exists(),
            "stale index cache must be invalidated on MissingStoreFile"
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_link_file_fresh_hardlink_short_circuits_when_source_missing() {
        // Hardlink path used to silently fall through to `std::fs::copy`
        // on ENOENT and emit a misleading "hardlink failed, falling back
        // to copy" trace, even though the real cause was the source
        // shard going missing. Short-circuit returns MissingStoreFile
        // directly so traces stay accurate.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("store/files"));
        let stored = store.import_bytes(b"hello", false).unwrap();
        // Capture the path before we move `stored` into link_file_fresh.
        let store_path = stored.store_path.clone();
        std::fs::remove_file(&store_path).unwrap();

        let dst_dir = dir.path().join("dst");
        std::fs::create_dir_all(&dst_dir).unwrap();
        let dst = dst_dir.join("hello.txt");

        let linker = Linker::new_with_gvs(&store, LinkStrategy::Hardlink, true);
        let err = linker
            .link_file_fresh(&stored, "hello.txt", &dst)
            .expect_err("source missing must fail");
        assert!(
            matches!(
                &err,
                Error::MissingStoreFile { store_path: p, rel_path } if p == &store_path && rel_path == "hello.txt"
            ),
            "expected MissingStoreFile from Hardlink branch, got {err:?}"
        );
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
    fn test_global_virtual_store_gets_hidden_hoist() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let virtual_store = store.virtual_store_dir();
        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        let mut graph = make_graph();
        graph
            .packages
            .get_mut("foo@1.0.0")
            .unwrap()
            .dependencies
            .clear();

        linker.link_all(&project_dir, &graph, &indices).unwrap();

        let project_hidden = project_dir.join("node_modules/.aube/node_modules/bar");
        assert!(project_hidden.symlink_metadata().unwrap().is_symlink());

        let global_hidden = virtual_store.join("node_modules/bar");
        assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

        let from_real_store = virtual_store.join("foo@1.0.0/node_modules/bar/index.js");
        assert!(
            !from_real_store.exists(),
            "bar is not a declared sibling of foo in this fixture"
        );
        let fallback = virtual_store.join("node_modules/bar/index.js");
        assert_eq!(
            std::fs::read_to_string(fallback).unwrap(),
            "module.exports = 'bar';"
        );
    }

    #[test]
    fn test_global_virtual_store_hidden_hoist_prunes_only_dead_entries() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let virtual_store = store.virtual_store_dir();
        let hidden = virtual_store.join("node_modules");
        std::fs::create_dir_all(&hidden).unwrap();
        let dotfile = hidden.join(".sentinel");
        std::fs::write(&dotfile, "shared").unwrap();
        let stale = hidden.join("stale");
        std::fs::write(&stale, "old").unwrap();
        let stale_scope = hidden.join("@stale-scope");
        std::fs::write(&stale_scope, "old").unwrap();
        let external_target = virtual_store.join("external@1.0.0/node_modules/external");
        std::fs::create_dir_all(&external_target).unwrap();
        let external_link = hidden.join("external");
        sys::create_dir_link(
            &pathdiff::diff_paths(&external_target, &hidden).unwrap(),
            &external_link,
        )
        .unwrap();
        let dead_link = hidden.join("dead");
        sys::create_dir_link(
            Path::new("../missing@1.0.0/node_modules/missing"),
            &dead_link,
        )
        .unwrap();

        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        linker
            .link_all(&project_dir, &make_graph(), &indices)
            .unwrap();

        assert_eq!(std::fs::read_to_string(dotfile).unwrap(), "shared");
        assert!(!stale.exists());
        assert!(stale_scope.symlink_metadata().is_err());
        assert!(external_link.symlink_metadata().unwrap().is_symlink());
        assert!(dead_link.symlink_metadata().is_err());
        assert!(hidden.join("bar").symlink_metadata().unwrap().is_symlink());
    }

    #[test]
    fn test_global_virtual_store_hidden_hoist_disabled_keeps_live_shared_links() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        let (store, indices) = setup_store_with_files(dir.path());
        let virtual_store = store.virtual_store_dir();
        let linker = Linker::new_with_gvs(&store, LinkStrategy::Copy, true);
        linker
            .link_all(&project_dir, &make_graph(), &indices)
            .unwrap();

        let global_hidden = virtual_store.join("node_modules/bar");
        assert!(global_hidden.symlink_metadata().unwrap().is_symlink());

        Linker::new_with_gvs(&store, LinkStrategy::Copy, true)
            .with_hoist(false)
            .link_all(&project_dir, &make_graph(), &indices)
            .unwrap();

        assert!(global_hidden.symlink_metadata().unwrap().is_symlink());
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

    /// Regression: a version bump keeps the same top-level name
    /// (`foo`) but must repoint `node_modules/foo` at the new
    /// `.aube/foo@<new>` entry. The old `.aube/foo@<old>/` is left
    /// on disk (no one sweeps the virtual store by name), so a
    /// plain `path.exists()` check would see a still-resolving
    /// stale symlink and keep it. The target-aware
    /// `reconcile_top_level_link` compares the expected target
    /// string and rewrites the link.
    #[test]
    fn test_link_all_repoints_symlink_after_version_bump() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let store = Store::at(dir.path().join("store/files"));

        // Install 1: foo@1.0.0 as the root's direct dep.
        let mut indices_v1 = BTreeMap::new();
        let foo_v1 = store
            .import_bytes(b"module.exports = 'foo@1';", false)
            .unwrap();
        let mut foo_v1_index = BTreeMap::new();
        foo_v1_index.insert("index.js".to_string(), foo_v1);
        indices_v1.insert("foo@1.0.0".to_string(), foo_v1_index);

        let mut graph_v1 = LockfileGraph::default();
        graph_v1.packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        graph_v1.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let linker = Linker::new(&store, LinkStrategy::Copy);
        linker
            .link_all(&project_dir, &graph_v1, &indices_v1)
            .unwrap();
        let foo_link = project_dir.join("node_modules/foo");
        assert!(foo_link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(foo_link.join("index.js")).unwrap(),
            "module.exports = 'foo@1';"
        );

        // Install 2: foo upgraded to 2.0.0. The `.aube/foo@1.0.0/`
        // tree stays on disk (nothing prunes the virtual store by
        // name), so the old `node_modules/foo` symlink still
        // resolves — a naive "does the target exist?" check would
        // keep it.
        let mut indices_v2 = BTreeMap::new();
        let foo_v2 = store
            .import_bytes(b"module.exports = 'foo@2';", false)
            .unwrap();
        let mut foo_v2_index = BTreeMap::new();
        foo_v2_index.insert("index.js".to_string(), foo_v2);
        indices_v2.insert("foo@2.0.0".to_string(), foo_v2_index);

        let mut graph_v2 = LockfileGraph::default();
        graph_v2.packages.insert(
            "foo@2.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "2.0.0".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                ..Default::default()
            },
        );
        graph_v2.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "foo".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );
        linker
            .link_all(&project_dir, &graph_v2, &indices_v2)
            .unwrap();

        // The top-level symlink must now resolve to foo@2.0.0's
        // bytes, not foo@1.0.0's.
        assert_eq!(
            std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
            "module.exports = 'foo@2';"
        );
    }

    /// Regression: `shamefully_hoist` hoists transitive deps to the
    /// top-level `node_modules/<name>`. When the hoisted version
    /// changes between installs (transitive bump), the previous
    /// implementation kept the stale symlink because
    /// `keep_or_reclaim_broken_symlink` only checked "does target
    /// resolve?" and the old `.aube/<old-dep-path>/` was still on
    /// disk. `reconcile_top_level_link` + the explicit
    /// direct-dep/claimed tracking in `hoist_remaining_into` together
    /// fix this.
    #[test]
    fn test_shamefully_hoist_repoints_after_transitive_version_bump() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let store = Store::at(dir.path().join("store/files"));

        // Install 1: root → bar@1.0.0 → foo@1.0.0 (transitive).
        let foo_v1 = store
            .import_bytes(b"module.exports = 'foo@1';", false)
            .unwrap();
        let mut foo_v1_idx = BTreeMap::new();
        foo_v1_idx.insert("index.js".to_string(), foo_v1);
        let bar_v1 = store
            .import_bytes(b"module.exports = 'bar@1';", false)
            .unwrap();
        let mut bar_v1_idx = BTreeMap::new();
        bar_v1_idx.insert("index.js".to_string(), bar_v1);
        let mut indices_v1 = BTreeMap::new();
        indices_v1.insert("foo@1.0.0".to_string(), foo_v1_idx);
        indices_v1.insert("bar@1.0.0".to_string(), bar_v1_idx);

        let mut graph_v1 = LockfileGraph::default();
        let mut bar_deps_v1 = BTreeMap::new();
        bar_deps_v1.insert("foo".to_string(), "1.0.0".to_string());
        graph_v1.packages.insert(
            "bar@1.0.0".to_string(),
            LockedPackage {
                name: "bar".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "bar@1.0.0".to_string(),
                dependencies: bar_deps_v1,
                ..Default::default()
            },
        );
        graph_v1.packages.insert(
            "foo@1.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "foo@1.0.0".to_string(),
                ..Default::default()
            },
        );
        graph_v1.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let linker = Linker::new(&store, LinkStrategy::Copy).with_shamefully_hoist(true);
        linker
            .link_all(&project_dir, &graph_v1, &indices_v1)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
            "module.exports = 'foo@1';",
            "install 1 should hoist foo@1.0.0"
        );

        // Install 2: bar@1.0.0 → foo@2.0.0 (transitive bump). The
        // stale `.aube/foo@1.0.0/` tree is still on disk (nothing
        // sweeps the virtual store by name), so the old hoisted
        // symlink would still resolve — the old `exists?` check
        // would silently keep it.
        let foo_v2 = store
            .import_bytes(b"module.exports = 'foo@2';", false)
            .unwrap();
        let mut foo_v2_idx = BTreeMap::new();
        foo_v2_idx.insert("index.js".to_string(), foo_v2);
        let mut indices_v2 = BTreeMap::new();
        // Reuse bar's materialized index from v1.
        let bar_v1_for_v2 = store
            .import_bytes(b"module.exports = 'bar@1';", false)
            .unwrap();
        let mut bar_v1_idx_v2 = BTreeMap::new();
        bar_v1_idx_v2.insert("index.js".to_string(), bar_v1_for_v2);
        indices_v2.insert("bar@1.0.0".to_string(), bar_v1_idx_v2);
        indices_v2.insert("foo@2.0.0".to_string(), foo_v2_idx);

        let mut graph_v2 = LockfileGraph::default();
        let mut bar_deps_v2 = BTreeMap::new();
        bar_deps_v2.insert("foo".to_string(), "2.0.0".to_string());
        graph_v2.packages.insert(
            "bar@1.0.0".to_string(),
            LockedPackage {
                name: "bar".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "bar@1.0.0".to_string(),
                dependencies: bar_deps_v2,
                ..Default::default()
            },
        );
        graph_v2.packages.insert(
            "foo@2.0.0".to_string(),
            LockedPackage {
                name: "foo".to_string(),
                version: "2.0.0".to_string(),
                dep_path: "foo@2.0.0".to_string(),
                ..Default::default()
            },
        );
        graph_v2.importers.insert(
            ".".to_string(),
            vec![DirectDep {
                name: "bar".to_string(),
                dep_path: "bar@1.0.0".to_string(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        linker
            .link_all(&project_dir, &graph_v2, &indices_v2)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(project_dir.join("node_modules/foo/index.js")).unwrap(),
            "module.exports = 'foo@2';",
            "install 2 should repoint the hoisted symlink to foo@2.0.0"
        );
    }

    // ---------------------------------------------------------------
    // `validate_index_key` rejects every shape of index key that
    // would make `base.join(key)` escape `base`. Primary defence is
    // in `aube-store::import_tarball`; this is the last-chance guard
    // before the linker actually writes to disk.
    // ---------------------------------------------------------------

    #[test]
    fn validate_index_key_accepts_normal_keys() {
        validate_index_key("index.js").unwrap();
        validate_index_key("lib/sub/a.js").unwrap();
        validate_index_key("package.json").unwrap();
        validate_index_key("a/b/c/d/e/f.js").unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn validate_index_key_accepts_posix_colon_filename() {
        validate_index_key("dist/__mocks__/package-json:version.d.ts").unwrap();
    }

    #[test]
    fn validate_index_key_rejects_empty() {
        assert!(matches!(
            validate_index_key(""),
            Err(Error::UnsafeIndexKey(_))
        ));
    }

    #[test]
    fn validate_index_key_rejects_leading_slash() {
        assert!(matches!(
            validate_index_key("/etc/passwd"),
            Err(Error::UnsafeIndexKey(_))
        ));
        assert!(matches!(
            validate_index_key("\\evil"),
            Err(Error::UnsafeIndexKey(_))
        ));
    }

    #[test]
    fn validate_index_key_rejects_parent_dir() {
        assert!(matches!(
            validate_index_key("../../etc/passwd"),
            Err(Error::UnsafeIndexKey(_))
        ));
        assert!(matches!(
            validate_index_key("lib/../../../etc"),
            Err(Error::UnsafeIndexKey(_))
        ));
    }

    #[test]
    fn validate_index_key_rejects_nul_and_backslash() {
        assert!(matches!(
            validate_index_key("lib\0evil"),
            Err(Error::UnsafeIndexKey(_))
        ));
        assert!(matches!(
            validate_index_key("lib\\..\\etc"),
            Err(Error::UnsafeIndexKey(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn validate_index_key_rejects_windows_drive() {
        assert!(matches!(
            validate_index_key("C:Windows"),
            Err(Error::UnsafeIndexKey(_))
        ));
    }
}
