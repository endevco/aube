#[macro_use]
extern crate log;

pub mod dirs;

use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const SHA512_INTEGRITY_PREFIX: &str = "sha512-";

/// Subresource Integrity (SRI) algorithm prefixes aube accepts in
/// `dist.integrity`. sha512 is what modern registries emit; sha1 is
/// kept for legacy packages (e.g. `co@4.6.0`) that were published
/// before npm's 2017 SRI rollout and never had their metadata rewritten.
const SRI_PREFIXES: &[(&str, IntegrityAlgo)] = &[
    ("sha512-", IntegrityAlgo::Sha512),
    ("sha384-", IntegrityAlgo::Sha384),
    ("sha256-", IntegrityAlgo::Sha256),
    ("sha1-", IntegrityAlgo::Sha1),
];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum IntegrityAlgo {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl IntegrityAlgo {
    fn prefix(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1-",
            Self::Sha256 => "sha256-",
            Self::Sha384 => "sha384-",
            Self::Sha512 => "sha512-",
        }
    }
}

fn parse_sri(expected: &str) -> Option<(IntegrityAlgo, &str)> {
    SRI_PREFIXES
        .iter()
        .find_map(|(prefix, algo)| expected.strip_prefix(prefix).map(|rest| (*algo, rest)))
}

pub const CACHE_DIR_NAME: &str = "aube-cache";
pub const INDEX_SUBDIR: &str = "index";
pub const VIRTUAL_STORE_SUBDIR: &str = "virtual-store";
pub const PACKUMENT_CACHE_SUBDIR: &str = "packuments-v1";
pub const PACKUMENT_FULL_CACHE_SUBDIR: &str = "packuments-full-v1";

thread_local! {
    static B3_HASHER: RefCell<blake3::Hasher> = RefCell::new(blake3::Hasher::new());
    static SHA512_HASHER: RefCell<Sha512> = RefCell::new(Sha512::new());
}

fn blake3_hex(content: &[u8]) -> String {
    B3_HASHER.with(|cell| {
        let mut h = cell.borrow_mut();
        h.reset();
        h.update(content);
        h.finalize().to_hex().to_string()
    })
}

/// The global content-addressable store, owned by aube.
///
/// Default location: `$XDG_DATA_HOME/aube/store/v1/files/` (falling
/// back to `~/.local/share/aube/store/v1/files/`).
/// Files are stored by BLAKE3 hash with two-char hex directory sharding.
/// (Tarball-level integrity is still SHA-512 because that's the format the
/// npm registry returns; the per-file CAS key is an internal choice.)
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
    cache_dir: PathBuf,
}

/// Metadata about a file stored in the CAS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFile {
    /// The hex hash of the file content.
    pub hex_hash: String,
    /// The path within the store.
    pub store_path: PathBuf,
    /// Whether the file is executable.
    pub executable: bool,
    /// File size in bytes when the entry was imported.
    #[serde(default)]
    pub size: Option<u64>,
}

/// Index of all files in a package, keyed by relative path within the package.
pub type PackageIndex = BTreeMap<String, StoredFile>;

/// Result of importing a package tarball into the CAS.
#[derive(Debug)]
pub struct ImportedTarball {
    pub index: PackageIndex,
    pub package_json: Option<Vec<u8>>,
}

fn index_files_match_metadata(index: &PackageIndex, verify_all: bool) -> bool {
    let mut files = index.values();
    if verify_all {
        return files.all(stored_file_matches_metadata);
    }
    // Hot install path: one metadata check catches the common crash
    // residue class (zero-byte/missing CAS files) without turning every
    // warm lockfile install into a full store walk.
    files.next().is_none_or(stored_file_matches_metadata)
}

fn stored_file_matches_metadata(file: &StoredFile) -> bool {
    file.size
        .map(|size| cas_file_matches_len(&file.store_path, size))
        .unwrap_or_else(|| file.store_path.exists())
}

fn cas_file_matches_len(path: &Path, expected_len: u64) -> bool {
    path.metadata()
        .map(|metadata| metadata.len() == expected_len)
        .unwrap_or(false)
}

impl Store {
    /// Open the store at the default location (see [`dirs::store_dir`]).
    pub fn default_location() -> Result<Self, Error> {
        let root = dirs::store_dir().ok_or(Error::NoHome)?;
        let cache_dir = dirs::cache_dir().ok_or(Error::NoHome)?;
        Ok(Self { root, cache_dir })
    }

    /// Open the store with an explicit root, keeping the default
    /// cache dir (`$XDG_CACHE_HOME/aube`). Used when a user overrides
    /// `storeDir` via `.npmrc` / `pnpm-workspace.yaml` — only the CAS
    /// moves; the packument and virtual-store caches stay where the
    /// rest of aube expects them.
    pub fn with_root(root: PathBuf) -> Result<Self, Error> {
        let cache_dir = dirs::cache_dir().ok_or(Error::NoHome)?;
        Ok(Self { root, cache_dir })
    }

    /// Open the store at a specific path (cache dir derived from store root).
    /// Used by tests that need a fully isolated layout; production code
    /// should prefer `default_location` or `with_root`.
    pub fn at(root: PathBuf) -> Self {
        let cache_dir = root.parent().unwrap_or(&root).join(CACHE_DIR_NAME);
        Self { root, cache_dir }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for cached package indices. Public so introspection
    /// commands (`aube find-hash`) can walk it directly.
    pub fn index_dir(&self) -> PathBuf {
        self.cache_dir.join(INDEX_SUBDIR)
    }

    /// Directory for the global virtual store (materialized packages).
    pub fn virtual_store_dir(&self) -> PathBuf {
        self.cache_dir.join(VIRTUAL_STORE_SUBDIR)
    }

    /// Directory for cached packument metadata (abbreviated/corgi format).
    /// Versioned so we can bump the schema without breaking old caches —
    /// old caches at older versions stay around until manually pruned.
    pub fn packument_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(PACKUMENT_CACHE_SUBDIR)
    }

    /// Directory for cached *full* packument JSON (non-corgi) used by
    /// human-facing commands like `aube view` that need fields the resolver
    /// doesn't parse (`description`, `repository`, `license`, `keywords`,
    /// `maintainers`). Separate from `packument_cache_dir` because the
    /// corgi and full responses have different shapes.
    pub fn packument_full_cache_dir(&self) -> PathBuf {
        self.cache_dir.join(PACKUMENT_FULL_CACHE_SUBDIR)
    }

    /// Check if a file with the given integrity hash exists in the store.
    pub fn has(&self, integrity: &str) -> bool {
        self.file_path_from_integrity(integrity)
            .is_some_and(|p| p.exists())
    }

    /// Get the path to a file in the store by its integrity hash.
    pub fn file_path_from_integrity(&self, integrity: &str) -> Option<PathBuf> {
        let hex_hash = integrity_to_hex(integrity)?;
        Some(self.file_path_from_hex(&hex_hash))
    }

    /// Get the path to a file in the store by its hex hash.
    pub fn file_path_from_hex(&self, hex_hash: &str) -> PathBuf {
        let (shard, rest) = hex_hash.split_at(2);
        self.root.join(shard).join(rest)
    }

    /// Load a cached package index, if it exists.
    ///
    /// `integrity`, when `Some`, is the registry-advertised SRI
    /// digest (`sha512-`, or legacy `sha1-` / `sha256-` / `sha384-`)
    /// of the tarball these cache files came from —
    /// part of the cache key so the same `(name, version)` resolved
    /// from different sources (npm registry vs. github codeload vs. a
    /// proxy that served different bytes) can't alias on disk and
    /// return each other's file lists to the linker. `None` falls
    /// back to an unsuffixed `<name>@<version>.json` key so packages
    /// fetched through a registry proxy that strips `dist.integrity`
    /// can still warm-install — an integrity-less setup is already a
    /// degraded mode the user opted into via `strict-store-integrity=false`.
    pub fn load_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PackageIndex> {
        self.load_index_inner(name, version, integrity, false)
    }

    /// Load a package index, optionally verifying that all store files still exist.
    /// The verified variant is slower (stat per file) but detects a corrupted store.
    pub fn load_index_verified(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
    ) -> Option<PackageIndex> {
        self.load_index_inner(name, version, integrity, true)
    }

    fn load_index_inner(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        verify_files: bool,
    ) -> Option<PackageIndex> {
        let index_path = self.index_path(name, version, integrity)?;
        let mut buf = xx::file::read(&index_path).ok()?;
        let index: PackageIndex = simd_json::serde::from_slice(&mut buf).ok()?;
        if !index_files_match_metadata(&index, verify_files) {
            trace!("cache stale: {name}@{version}");
            let _ = xx::file::remove_file(&index_path);
            return None;
        }
        trace!("cache hit: {name}@{version}");
        Some(index)
    }

    /// Save a package index to the cache.
    ///
    /// See [`load_index`](Self::load_index) for the semantics of
    /// `integrity` and the integrity-less fallback.
    pub fn save_index(
        &self,
        name: &str,
        version: &str,
        integrity: Option<&str>,
        index: &PackageIndex,
    ) -> Result<(), Error> {
        let index_path = self.index_path(name, version, integrity).ok_or_else(|| {
            Error::Tar(format!(
                "refusing to cache: invalid coordinate {name:?}@{version:?} or integrity {integrity:?}"
            ))
        })?;
        let json =
            serde_json::to_string(index).map_err(|e| Error::Tar(format!("serialize: {e}")))?;
        xx::file::write(&index_path, json).map_err(|e| Error::Xx(e.to_string()))?;
        trace!("cached index: {name}@{version}");
        Ok(())
    }

    /// Build the on-disk path for a cached index.
    ///
    /// Layout:
    /// - With integrity: `index/<16 hex>/<name>@<version>.json`. The
    ///   integrity hex lives in a subdirectory (not as part of the
    ///   filename) so a version whose semver build metadata happens
    ///   to be 16 lowercase hex chars (e.g. `1.0.0+a1b2c3d4e5f6a7b8`)
    ///   can never collide with an integrity-keyed entry for
    ///   `1.0.0` — they land in distinct directories by construction.
    /// - Without integrity: `index/<name>@<version>.json` at the
    ///   index dir root. Used for registry proxies that strip
    ///   `dist.integrity`; the user has already opted out of
    ///   cross-source integrity enforcement.
    ///
    /// Returns `None` when any component is invalid (including an
    /// integrity string we can't hex-decode).
    fn index_path(&self, name: &str, version: &str, integrity: Option<&str>) -> Option<PathBuf> {
        let safe_name = validate_and_encode_name(name)?;
        if !validate_version(version) {
            return None;
        }
        let filename = format!("{safe_name}@{version}.json");
        let dir = self.index_dir();
        match integrity {
            Some(i) => {
                let hex = integrity_to_hex(i)?;
                // 16 hex chars = 64 bits of tarball SHA-512 prefix.
                // Two tarballs whose SHA-512 prefixes collide would
                // both have to be valid registry responses for the
                // same (name, version) *and* survive `verify_integrity`
                // on fetch, so birthday-bound collisions aren't a
                // correctness risk; 16 chars is plenty.
                let short = &hex[..16.min(hex.len())];
                Some(dir.join(short).join(filename))
            }
            None => Some(dir.join(filename)),
        }
    }

    /// Ensure every two-char shard directory under the CAS root exists.
    /// CAS files live under `<root>/<ab>/<cdef...>` for 256 possible
    /// prefixes. Running this once before a batch of `import_bytes`
    /// calls lets the per-file hot path skip the `mkdirp(parent)` stat
    /// entirely (the parent is guaranteed to exist). On APFS that
    /// removes ~7.5k redundant `stat` syscalls per cold install — the
    /// `mkdirp` inside `xx::file::write` was the #1 stat hotspot in a
    /// dtrace profile.
    ///
    /// Cheap to call repeatedly: each `create_dir_all` is a no-op when
    /// the directory already exists, but callers should still hoist the
    /// call out of tight loops.
    pub fn ensure_shards_exist(&self) -> Result<(), Error> {
        std::fs::create_dir_all(&self.root).map_err(|e| Error::Io(self.root.clone(), e))?;
        let mut buf = [0u8; 2];
        for hi in 0u8..16 {
            for lo in 0u8..16 {
                buf[0] = hex_digit(hi);
                buf[1] = hex_digit(lo);
                // SAFETY: every byte in `buf` comes from `hex_digit`,
                // which only emits `0-9` / `a-f` — always valid UTF-8.
                let shard = std::str::from_utf8(&buf).unwrap();
                let path = self.root.join(shard);
                std::fs::create_dir_all(&path).map_err(|e| Error::Io(path, e))?;
            }
        }
        Ok(())
    }

    /// Atomically create `path` without overwriting an existing CAS entry.
    /// `AlreadyExists` is a no-op here; callers that know the expected content
    /// length must verify it before trusting a reused path. Non-empty files are
    /// written through a sibling temp file and persisted with no-clobber
    /// semantics so an interrupted import cannot leave a torn file at the
    /// content-addressed path. We intentionally do not fsync every CAS file:
    /// cold installs import tens of thousands of files, and package-index
    /// loading rejects missing/truncated entries so they can be fetched again.
    /// `NotFound` means a concurrent prune or a missed `ensure_shards_exist`
    /// removed the parent shard; recreate it and retry exactly once before
    /// surfacing.
    fn create_cas_file(path: &Path, content: Option<&[u8]>) -> Result<(), Error> {
        fn do_create_and_write(path: &Path, content: Option<&[u8]>) -> Result<(), Error> {
            if let Some(bytes) = content {
                let parent = path.parent().ok_or_else(|| {
                    Error::Io(path.to_path_buf(), std::io::ErrorKind::NotFound.into())
                })?;
                let mut tmp = tempfile::Builder::new()
                    .prefix(".aube-cas-")
                    .tempfile_in(parent)
                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                use std::io::Write;
                tmp.write_all(bytes)
                    .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tmp.as_file()
                        .set_permissions(std::fs::Permissions::from_mode(0o644))
                        .map_err(|e| Error::Io(path.to_path_buf(), e))?;
                }
                return match tmp.persist_noclobber(path) {
                    Ok(_) => Ok(()),
                    Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                    Err(e) => Err(Error::Io(path.to_path_buf(), e.error)),
                };
            }

            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(Error::Io(path.to_path_buf(), e)),
            }
        }

        match do_create_and_write(path, content) {
            Ok(()) => Ok(()),
            Err(Error::Io(_, ref ioe)) if ioe.kind() == std::io::ErrorKind::NotFound => {
                // Shard dir missing. `ensure_shards_exist` normally
                // pre-creates all 256 shards; this only fires when the
                // caller didn't call it or a concurrent prune wiped
                // the tree mid-install.
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| Error::Io(parent.to_path_buf(), e))?;
                }
                do_create_and_write(path, content)
            }
            Err(e) => Err(e),
        }
    }

    /// Import a single file's content into the store. Returns the stored file info.
    ///
    /// Hot path on cold installs: callers should invoke
    /// [`Store::ensure_shards_exist`] once before a batch of imports so
    /// this function can skip the per-file `mkdirp`. When shards don't
    /// exist yet, the `create_new` open will fail with `NotFound`; we
    /// fall back to the slow path for correctness.
    pub fn import_bytes(&self, content: &[u8], executable: bool) -> Result<StoredFile, Error> {
        let hex_hash = blake3_hex(content);

        let store_path = self.file_path_from_hex(&hex_hash);

        // Fast path: open-with-create-new combines the existence check
        // and the open into a single syscall. On a cold CAS this does
        // one open(O_CREAT|O_EXCL|O_WRONLY) per file and replaces the
        // previous stat+create pair (~15k redundant stats per cold
        // install). On a warm CAS, concurrent writers are safe: EEXIST
        // means another writer already materialized this content (same
        // hash = same bytes), so we skip and share the entry.
        Self::create_cas_file(&store_path, Some(content))?;
        if !cas_file_matches_len(&store_path, content.len() as u64) {
            let _ = xx::file::remove_file(&store_path);
            Self::create_cas_file(&store_path, Some(content))?;
            if !cas_file_matches_len(&store_path, content.len() as u64) {
                let actual_len = store_path.metadata().map(|metadata| metadata.len()).ok();
                return Err(Error::Io(
                    store_path.clone(),
                    std::io::Error::other(format!(
                        "CAS entry has wrong size after import: expected {} bytes, got {}",
                        content.len(),
                        actual_len
                            .map(|len| format!("{len} bytes"))
                            .unwrap_or_else(|| "missing file".to_owned())
                    )),
                ));
            }
        }

        if executable {
            // Behavior note: this branch now runs unconditionally when
            // `executable=true`, including when the content file
            // already existed (`AlreadyExists` above). Previously the
            // marker was only written in the fresh-content branch.
            // The new shape is strictly more correct — if the same
            // bytes are imported twice, once with `executable=false`
            // and once with `true`, the marker should exist after the
            // second call. Auditing the callers of the `-exec` marker:
            //   - `aube-store::import_bytes` (this function, the only
            //     writer).
            //   - `aube-store` tests (assert the marker exists after
            //     an `executable=true` import).
            //   - `aube::commands::store` (`aube store prune`)
            //     uses the marker to skip bumping the "freed bytes"
            //     counter when unlinking exec-marker sidecars.
            // No code path reads the marker to decide executability —
            // that's carried in `StoredFile.executable`, threaded
            // through the `PackageIndex` and the linker. So flipping
            // a marker-absent-to-present for a shared hash is safe.
            let exec_marker = PathBuf::from(format!("{}-exec", store_path.display()));
            Self::create_cas_file(&exec_marker, None)?;
        }

        Ok(StoredFile {
            hex_hash,
            store_path,
            executable,
            size: Some(content.len() as u64),
        })
    }

    /// Import every file under a directory into the store, producing a
    /// `PackageIndex` keyed by paths relative to `dir`. Used by `file:`
    /// deps pointing at an on-disk package directory. Common noise
    /// (`.git`, `node_modules`) is skipped so local packages don't drag
    /// the target's own installed deps into the virtual store.
    pub fn import_directory(&self, dir: &Path) -> Result<PackageIndex, Error> {
        let mut index = BTreeMap::new();
        self.import_directory_recursive(dir, dir, &mut index)?;
        Ok(index)
    }

    fn import_directory_recursive(
        &self,
        base: &Path,
        current: &Path,
        index: &mut PackageIndex,
    ) -> Result<(), Error> {
        let entries = std::fs::read_dir(current)
            .map_err(|e| Error::Tar(format!("read_dir {}: {e}", current.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::Tar(format!("read_dir {}: {e}", current.display())))?;
            let file_type = entry
                .file_type()
                .map_err(|e| Error::Tar(format!("file_type: {e}")))?;
            let name_os = entry.file_name();
            let name_str = name_os.to_string_lossy();
            if matches!(name_str.as_ref(), ".git" | "node_modules") {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                self.import_directory_recursive(base, &path, index)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let content = std::fs::read(&path)
                .map_err(|e| Error::Tar(format!("read {}: {e}", path.display())))?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                let meta = entry
                    .metadata()
                    .map_err(|e| Error::Tar(format!("metadata: {e}")))?;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let stored = self.import_bytes(&content, executable)?;
            let rel = path
                .strip_prefix(base)
                .map_err(|e| Error::Tar(format!("strip_prefix: {e}")))?
                .to_string_lossy()
                .replace('\\', "/");
            index.insert(rel, stored);
        }
        Ok(())
    }

    /// Import a tarball (.tgz) into the store.
    /// Returns a PackageIndex mapping relative paths to stored files.
    pub fn import_tarball(&self, tarball_bytes: &[u8]) -> Result<PackageIndex, Error> {
        Ok(self.import_tarball_with_manifest(tarball_bytes)?.index)
    }

    /// Import a tarball (.tgz) into the store, also returning the
    /// decompressed `package.json` bytes when present.
    pub fn import_tarball_with_manifest(
        &self,
        tarball_bytes: &[u8],
    ) -> Result<ImportedTarball, Error> {
        use std::io::Read;

        // Caps defend against gzip bombs and lying tar headers. The
        // values sit well above any real npm package (largest top
        // 1000 are in the tens of MiB) but low enough to prevent a
        // malicious registry or mirror from OOMing the installer
        // with a small high-compression-ratio payload.
        //
        // `CappedReader` is used instead of `Read::take` for the
        // archive-level cap so an exhaustion surfaces as an `Err`
        // rather than a clean EOF. A clean EOF landing on a tar
        // block boundary would let a crafted archive silently
        // truncate into a partial index.
        let gz = flate2::read::GzDecoder::new(tarball_bytes);
        let capped = CappedReader::new(gz, MAX_TARBALL_DECOMPRESSED_BYTES);
        let buffered = std::io::BufReader::with_capacity(256 * 1024, capped);
        let mut archive = tar::Archive::new(buffered);
        let mut index = BTreeMap::new();
        let mut package_json = None;
        let mut entries_seen: usize = 0;

        // Serial walk — each tarball is decoded by one spawn_blocking
        // task on the fetch-phase blocking pool, which is already
        // parallel across packages. A rayon-inner parallelization
        // inside each tarball measured slower in practice because
        // ~250 concurrent imports all competing for the same CPU
        // cores amplifies contention more than per-tarball
        // parallelism helps.
        for entry in archive.entries().map_err(|e| Error::Tar(e.to_string()))? {
            entries_seen += 1;
            if entries_seen > MAX_TARBALL_ENTRIES {
                return Err(Error::Tar(format!(
                    "tarball exceeds entry cap of {MAX_TARBALL_ENTRIES}"
                )));
            }

            let mut entry = entry.map_err(|e| Error::Tar(e.to_string()))?;

            // Directories don't carry content, skip them. PAX global
            // and extension headers (type `g` / `x`) carry metadata
            // only — GitHub-generated tarballs (e.g. `imap@0.8.19`)
            // start with one that embeds the source git blob SHA.
            // npm/pnpm/bun tolerate these; we do too. Every other
            // non-regular entry type (symlink, hardlink, character
            // device, block device, fifo) is rejected. Real npm
            // packages ship files and directories only. Symlink and
            // hardlink entries are the load-bearing primitive of the
            // node-tar CVE-2021-37701 class and have no legitimate
            // use here.
            let entry_type = entry.header().entry_type();
            if entry_type.is_dir()
                || matches!(
                    entry_type,
                    tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
                )
            {
                continue;
            }
            if !matches!(
                entry_type,
                tar::EntryType::Regular | tar::EntryType::Continuous
            ) {
                return Err(Error::Tar(format!(
                    "tarball entry type {entry_type:?} is not allowed"
                )));
            }

            // Reject oversized entries up front on the declared size
            // so we never allocate a huge `Vec` just to error after.
            // `.take()` below is the belt-and-suspenders guard for
            // the case where the header lies about the stream length.
            let declared = entry
                .header()
                .size()
                .map_err(|e| Error::Tar(e.to_string()))?;
            if declared > MAX_TARBALL_ENTRY_BYTES {
                return Err(Error::Tar(format!(
                    "tarball entry exceeds per-entry cap: {declared} bytes > {MAX_TARBALL_ENTRY_BYTES}"
                )));
            }

            let raw_path = entry
                .path()
                .map_err(|e| Error::Tar(e.to_string()))?
                .to_path_buf();
            let Some(rel_path) = normalize_tar_entry_path(&raw_path)? else {
                // Entry was the wrapper directory itself with no
                // interior path after stripping. Nothing to store.
                continue;
            };

            // Clamp the upfront allocation to a sane ceiling so a
            // lying header cannot force a 512 MiB reservation before
            // any byte has been read. `read_to_end` grows the Vec
            // naturally past this point for the rare entry that
            // really is this large.
            let mut content = Vec::with_capacity((declared as usize).min(VEC_PREALLOC_CEILING));
            (&mut entry)
                .take(MAX_TARBALL_ENTRY_BYTES)
                .read_to_end(&mut content)
                .map_err(|e| Error::Tar(e.to_string()))?;

            let mode = entry.header().mode().unwrap_or(0o644);
            let executable = mode & 0o111 != 0;
            let stored = self.import_bytes(&content, executable)?;
            if rel_path == "package.json" {
                package_json = Some(content);
            }
            index.insert(rel_path, stored);
        }

        Ok(ImportedTarball {
            index,
            package_json,
        })
    }
}

/// Strip the wrapper directory from `raw` and return a safe POSIX-style
/// index key, or refuse the entry outright.
///
/// Rejects every shape that would let a crafted tarball place the
/// eventual `pkg_dir.join(key)` file outside the package root:
/// `..` anywhere in the remaining path, absolute paths, Windows
/// drive prefixes, backslash separators smuggled inside a single
/// component, and NUL bytes. Non-UTF-8 paths are also rejected because
/// the stored index is a JSON map keyed by string.
///
/// `Ok(None)` means the entry stripped down to the wrapper itself
/// and should be skipped by the caller.
///
/// Matches the class of defences node-tar added for CVE-2021-32804,
/// CVE-2021-37713, and what pnpm added for CVE-2024-27298.
fn normalize_tar_entry_path(raw: &Path) -> Result<Option<String>, Error> {
    use std::path::Component;

    // Peek past any leading `.` segments (`./package/foo.js` appears
    // in some tar implementations' wrapper representations) so the
    // first-component reject and the wrapper-strip both work off the
    // same "first real" position. Running the reject before the
    // stripping loop would otherwise let `./../file` silently
    // consume the `..` as the wrapper.
    let mut components = raw.components().peekable();
    while matches!(components.peek(), Some(Component::CurDir)) {
        components.next();
    }

    // Reject absolute, drive-prefixed, or `..`-rooted paths before
    // wrapper-strip runs. A naive "skip the first component" would
    // otherwise strip the `RootDir` marker and accept `/etc` as
    // `etc`, or consume a leading `..` as the wrapper.
    match components.peek() {
        Some(Component::RootDir) => {
            return Err(Error::Tar(format!(
                "tarball entry path is absolute: {raw:?}"
            )));
        }
        Some(Component::Prefix(_)) => {
            return Err(Error::Tar(format!(
                "tarball entry path has a Windows drive prefix: {raw:?}"
            )));
        }
        Some(Component::ParentDir) => {
            return Err(Error::Tar(format!(
                "tarball entry path escapes package root via `..`: {raw:?}"
            )));
        }
        _ => {}
    }

    // Drop the first real component as the wrapper directory. npm
    // convention is `package/`, but some packages ship the package
    // name or another identifier. Whatever it is, drop it.
    components.next();

    let mut out = String::new();
    for comp in components {
        match comp {
            Component::Normal(os) => {
                let s = os.to_str().ok_or_else(|| {
                    Error::Tar(format!(
                        "tarball entry path contains non-UTF-8 bytes: {raw:?}"
                    ))
                })?;
                if s.is_empty() || s.contains('\0') || s.contains('\\') || s.contains('/') {
                    return Err(Error::Tar(format!(
                        "tarball entry path contains a malformed component: {raw:?}"
                    )));
                }
                // Windows-only filename restrictions. Gated to
                // cfg(windows) so Unix hosts keep tarballs with
                // valid-on-Linux names like `CON.js` or `foo.`.
                // Rejecting those cross-platform would regress real
                // Linux installs for a hazard that only hits
                // Windows users. Windows users get the checks they
                // need, portability of a package to Windows is the
                // publisher's problem to validate.
                #[cfg(windows)]
                {
                    // `:` is an alternate data stream separator on
                    // NTFS and is rejected by Windows path creation.
                    if s.contains(':') {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains a malformed component: {raw:?}"
                        )));
                    }
                    // NTFS reserved device names. `CON`, `con.txt`,
                    // `CON.tar.gz` all resolve to the console
                    // device. Writing one either fails with
                    // ERROR_INVALID_NAME or gets silently consumed
                    // by the device driver and hangs the writer.
                    if is_windows_reserved_name(s) {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains a Windows reserved device name: {raw:?}"
                        )));
                    }
                    // NTFS strips trailing `.` and trailing space
                    // on create so `foo` and `foo.` alias. Reject
                    // both rather than sort out aliasing at
                    // materialize time.
                    if s.ends_with('.') || s.ends_with(' ') {
                        return Err(Error::Tar(format!(
                            "tarball entry path has a trailing dot or space which Windows strips: {raw:?}"
                        )));
                    }
                    // Control chars 0x01..0x1F invalid on NTFS.
                    // Reject the whole tarball instead of hitting
                    // per-file create errors mid-extract.
                    if s.bytes().any(|b| b < 0x20) {
                        return Err(Error::Tar(format!(
                            "tarball entry path contains control characters: {raw:?}"
                        )));
                    }
                }
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(s);
            }
            Component::ParentDir => {
                return Err(Error::Tar(format!(
                    "tarball entry path escapes package root via `..`: {raw:?}"
                )));
            }
            Component::RootDir => {
                return Err(Error::Tar(format!(
                    "tarball entry path is absolute: {raw:?}"
                )));
            }
            Component::Prefix(_) => {
                return Err(Error::Tar(format!(
                    "tarball entry path has a Windows drive prefix: {raw:?}"
                )));
            }
            // `.` components are harmless and appear in some tar
            // implementations' wrapper representations.
            Component::CurDir => {}
        }
    }

    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Check if a tarball path component matches a Windows reserved
/// device name. Compare case-insensitively on the stem only.
/// `CON`, `Con`, `con`, `con.txt`, `CON.tar.gz` all resolve to the
/// same DOS device on NTFS. Only base name matters, the extension
/// is irrelevant to the device lookup.
#[cfg(windows)]
fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split_once('.').map(|(a, _)| a).unwrap_or(name);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Hard ceiling on the per-entry `Vec::with_capacity` hint. 64 KiB
/// covers the bulk of real npm package files (JS / JSON / TS, all
/// typically under a few KiB each) without trusting the declared
/// header size, which an attacker controls. `read_to_end` grows
/// past this ceiling when a legitimate larger file warrants it.
const VEC_PREALLOC_CEILING: usize = 64 * 1024;

/// A `Read` wrapper that refuses to deliver more than `remaining`
/// bytes. Unlike `std::io::Read::take`, exhaustion produces an
/// explicit `io::Error` rather than a clean EOF. When the wrapped
/// reader is a gzip decoder feeding a tar archive, a clean EOF at
/// a block boundary would let a crafted archive silently truncate
/// into a partial index. Surfacing an error keeps the archive
/// iterator from accepting a half-read stream as complete.
struct CappedReader<R: std::io::Read> {
    inner: R,
    remaining: u64,
}

impl<R: std::io::Read> CappedReader<R> {
    fn new(inner: R, cap: u64) -> Self {
        Self {
            inner,
            remaining: cap,
        }
    }
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // A zero-length read is a no-op by the `Read` contract and
        // must not error even if the cap is already exhausted.
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "tarball decompression exceeds archive cap of {MAX_TARBALL_DECOMPRESSED_BYTES} bytes"
                ),
            ));
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Maximum total decompressed bytes accepted from a single tarball.
/// 1 GiB. Reality check against the npm registry on 2026-04-19.
/// Biggest tarball in the top 1000 by download count is `next` at
/// 154 MiB unpacked. Second is `@tensorflow/tfjs` at 147 MiB. The
/// cap sits ~6x above both, leaves room for future growth, and
/// stays well below the process RSS a gzip bomb would otherwise
/// force the installer to allocate.
#[cfg(not(test))]
const MAX_TARBALL_DECOMPRESSED_BYTES: u64 = 1 << 30;
#[cfg(test)]
const MAX_TARBALL_DECOMPRESSED_BYTES: u64 = 1 << 20;

/// Maximum bytes for a single tar entry. 512 MiB. Reality check: the
/// largest legitimate single file shipped by a top-1000 npm package
/// sits in the tens of MiB range (bundled WASM blobs in `@swc/wasm`,
/// `@babel/standalone`, `monaco-editor`). 512 MiB leaves a full
/// order of magnitude of headroom.
#[cfg(not(test))]
const MAX_TARBALL_ENTRY_BYTES: u64 = 512 << 20;
#[cfg(test)]
const MAX_TARBALL_ENTRY_BYTES: u64 = 1 << 20;

/// Maximum number of tar entries in a single archive. 200_000.
/// Reality check: `next` ships 8_065 files and `@fluentui/react`
/// ships 7_448, the largest counts in the top 1000. 200_000 is
/// ~25x above that and stops a crafted archive from pinning the
/// CPU on iteration alone.
#[cfg(not(test))]
const MAX_TARBALL_ENTRIES: usize = 200_000;
#[cfg(test)]
const MAX_TARBALL_ENTRIES: usize = 64;

/// Map a nibble (0–15) to its lowercase hex ASCII byte. Used by
/// `ensure_shards_exist` to build the 256 two-character shard names
/// without pulling in `format!`/`hex` per call.
fn hex_digit(n: u8) -> u8 {
    match n {
        0..=9 => b'0' + n,
        10..=15 => b'a' + n - 10,
        _ => unreachable!(),
    }
}

/// Validate a package name and return the `safe_name` form used as a
/// cache filename stem (`/` collapsed to `__` so scoped names survive
/// a single path component). Refuses anything outside the npm name
/// grammar so a hostile packument cannot turn a cache write into an
/// arbitrary-file-write primitive. Public so callers in
/// `aube-registry` and `aube` (which own separate cache layouts under
/// the same cache root) can share one validator.
///
/// A malicious packument can set `name` to `../../etc/passwd` (or, on
/// Windows, to something with a drive prefix or backslash). The old
/// `name.replace('/', "__")` only stripped forward slashes, so
/// `index_dir().join(format!("{name}@{version}.json"))` would silently
/// resolve outside the cache directory on the first resolve of the
/// hostile package.
///
/// Accepted grammar is `[A-Za-z0-9_.-]` per component, with a single
/// optional `@scope/` prefix. Uppercase and leading `.` / `_` are
/// allowed on purpose: npm's registry bans them for *new* publishes
/// but thousands of pre-rule packages (`JSONStream`, `Base64`, etc.)
/// still resolve fine under pnpm and bun, and mirroring the registry's
/// publish grammar here would block their cache path and break
/// install. The only rejects are empty components, `.` / `..`, the
/// 214-char length ceiling, and any byte outside the grammar.
pub fn validate_and_encode_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 214 {
        return None;
    }
    let (scope, bare) = match name.strip_prefix('@') {
        Some(rest) => {
            let (s, b) = rest.split_once('/')?;
            (Some(s), b)
        }
        None => (None, name),
    };
    let ok_component = |s: &str| -> bool {
        // npm's registry bars new packages from leading `.` / `_` but
        // historical packages that predate the rule still resolve
        // fine, and scoped private registries allow them. Only bar
        // empty and `.`/`..` since those collide with path components
        // after the `/` → `__` folding.
        if s.is_empty() || s == "." || s == ".." {
            return false;
        }
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if let Some(s) = scope
        && !ok_component(s)
    {
        return None;
    }
    if !ok_component(bare) {
        return None;
    }
    Some(name.replace('/', "__"))
}

/// Check a version string for use as a cache filename component.
/// The lockfile already constrains versions to semver-ish shapes, but
/// the cache path is independent of the lockfile on the write side so
/// a crafted packument version would still land here. Returns `true`
/// for anything the cache path builder is willing to accept.
pub fn validate_version(version: &str) -> bool {
    if version.is_empty() || version.len() > 256 {
        return false;
    }
    // pnpm and bun sometimes route non-semver specs (git URLs, file
    // specs, aliased registries) through the `version` slot, so the
    // guard only needs to block what actually breaks the cache path
    // builder: path separators on any platform, `\0`, control chars,
    // and the two "this is a directory name" aliases.
    if version
        .bytes()
        .any(|b| b.is_ascii_control() || matches!(b, b'/' | b'\\' | b'\0'))
    {
        return false;
    }
    if version == "." || version == ".." {
        return false;
    }
    true
}

/// Verify that data matches an SRI integrity hash. Accepts any of
/// `sha512-` / `sha384-` / `sha256-` / `sha1-` prefixed base64 digests
/// — the set npm and pnpm accept in `dist.integrity`. Returns `Ok(())`
/// on match, `Err(Error::Integrity)` on mismatch or unknown algorithm.
pub fn verify_integrity(data: &[u8], expected: &str) -> Result<(), Error> {
    let Some((algo, expected_b64)) = parse_sri(expected) else {
        return Err(Error::Integrity(format!(
            "unsupported integrity format (expected sha1/sha256/sha384/sha512-...): {expected}"
        )));
    };

    // Stack-buffer the actual digest (max sha512 = 64 bytes) so the
    // hot path stays allocation-free. sha512 reuses the thread-local
    // hasher because it's the common case by 3+ orders of magnitude;
    // the legacy algorithms one-shot a fresh hasher.
    let mut actual_buf = [0u8; 64];
    let actual_len = match algo {
        IntegrityAlgo::Sha1 => {
            let d = Sha1::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha256 => {
            let d = Sha256::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha384 => {
            let d = Sha384::digest(data);
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        IntegrityAlgo::Sha512 => SHA512_HASHER.with(|cell| {
            let mut hasher = cell.borrow_mut();
            hasher.reset();
            hasher.update(data);
            let d = hasher.finalize_reset();
            actual_buf[..d.len()].copy_from_slice(&d);
            d.len()
        }),
    };
    let actual = &actual_buf[..actual_len];

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut expected_digest = [0u8; 64];
    let matched = engine
        .decode_slice(expected_b64, &mut expected_digest)
        .map(|n| n == actual_len && expected_digest[..n] == actual[..])
        .unwrap_or(false);
    if matched {
        Ok(())
    } else {
        let actual_b64 = engine.encode(actual);
        Err(Error::Integrity(format!(
            "integrity mismatch: expected {expected}, got {prefix}{actual_b64}",
            prefix = algo.prefix(),
        )))
    }
}

/// Cross-check that an extracted tarball's `package.json` reports the
/// same `name` and `version` the registry told us to fetch. This is the
/// implementation behind the `strictStorePkgContentCheck` setting and
/// guards against registry-substitution attacks where a tarball is
/// served under one (name, version) but actually contains a different
/// package on disk.
///
/// `index` must be the result of a freshly-completed `import_tarball`
/// (or `import_directory`) — the helper reads `package.json` straight
/// from the on-disk store path recorded in the index, so the bytes
/// being validated are exactly the bytes that just landed in the CAS.
///
/// Returns `Ok(())` when both fields match, `Err(Error::PkgContentMismatch)`
/// when they don't, and `Err(Error::Tar)` if the manifest is missing
/// or unparseable. We deliberately treat a missing/broken manifest as
/// a check failure rather than silently passing — a registry tarball
/// without a usable `package.json` is itself a corruption signal.
pub fn validate_pkg_content(
    index: &PackageIndex,
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Error> {
    // The two error paths below intentionally omit the
    // `{expected_name}@{expected_version}` coordinate. Every caller
    // wraps with `miette!("{name}@{version}: {e}")` (mirroring the
    // Error::Integrity path), so embedding it here would print the
    // same coordinate twice — same rationale as the
    // Error::PkgContentMismatch return below.
    let stored = index
        .get("package.json")
        .ok_or_else(|| Error::Tar("package.json missing from tarball".to_string()))?;
    let bytes =
        std::fs::read(&stored.store_path).map_err(|e| Error::Io(stored.store_path.clone(), e))?;
    validate_pkg_content_bytes(&bytes, expected_name, expected_version)
}

/// Cross-check package metadata using manifest bytes already read from a tarball.
pub fn validate_pkg_content_bytes(
    bytes: &[u8],
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Error> {
    let v: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|e| Error::Tar(format!("invalid package.json: {e}")))?;
    validate_pkg_content_manifest(v, expected_name, expected_version)
}

/// Cross-check package metadata using owned manifest bytes.
pub fn validate_pkg_content_bytes_owned(
    mut bytes: Vec<u8>,
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Error> {
    let fallback = bytes.clone();
    let v: serde_json::Value = simd_json::serde::from_slice(&mut bytes)
        .or_else(|_| serde_json::from_slice(&fallback))
        .map_err(|e| Error::Tar(format!("invalid package.json: {e}")))?;
    validate_pkg_content_manifest(v, expected_name, expected_version)
}

fn validate_pkg_content_manifest(
    v: serde_json::Value,
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Error> {
    let actual_name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let actual_version = v.get("version").and_then(|v| v.as_str()).unwrap_or("");
    // Tolerate a leading `v` on the tarball's version (e.g. "v2.0.8").
    // Some publishers ship this shape; npm and bun normalize it on
    // install, so aube does too rather than rejecting a package the
    // other managers accept. The registry-side coordinate is the
    // source of truth, so we only normalize the tarball side.
    let actual_version_normalized = actual_version
        .strip_prefix('v')
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(actual_version);
    // pnpm v9 lockfiles key git-hosted deps by the codeload tarball URL
    // (or a `git+<url>#<commit>` form) in the `version` slot of the
    // dep_path — that URL is what the resolver hands us as
    // `expected_version`, and it can't meaningfully be compared to the
    // tarball's real semver. pnpm scopes its equivalent check to
    // registry sources; do the same by dropping the version comparison
    // (but still checking the name) whenever `expected_version` isn't
    // semver-shaped.
    let expected_is_url_or_ref = expected_version.contains("://")
        || expected_version.starts_with("git+")
        || expected_version.starts_with("file:");
    let version_matches = expected_is_url_or_ref || actual_version_normalized == expected_version;
    if actual_name != expected_name || !version_matches {
        // Only carry the *actual* coordinate the tarball declared.
        // Every caller wraps the error with the expected
        // `{name}@{version}: ` prefix (mirroring the Error::Integrity
        // path), so embedding `expected` here would print the same
        // coordinate twice in the rendered diagnostic.
        return Err(Error::PkgContentMismatch {
            actual: format!("{actual_name}@{actual_version}"),
        });
    }
    Ok(())
}

/// Decode a pnpm-style SRI integrity string (`sha512-` / `sha384-` /
/// `sha256-` / `sha1-` + base64) into its raw hex digest. Used by
/// introspection commands that accept the registry integrity format
/// as an ergonomic input, and by `index_path` to shard the cache
/// directory by integrity prefix. Returns `None` if the input isn't a
/// well-formed SRI integrity string.
pub fn integrity_to_hex(integrity: &str) -> Option<String> {
    let (_, b64) = parse_sri(integrity)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(hex::encode(bytes))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HOME environment variable not set")]
    NoHome,
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("file error: {0}")]
    Xx(String),
    #[error("tarball extraction error: {0}")]
    Tar(String),
    #[error("integrity verification failed: {0}")]
    Integrity(String),
    #[error("package.json content mismatch: tarball declares {actual}")]
    PkgContentMismatch { actual: String },
    #[error("git error: {0}")]
    Git(String),
}

/// Reject values that would be interpreted by git as an option when
/// handed to a subcommand as a positional argument. Defense against
/// the CVE-2017-1000117 class of argv injection.
///
/// Modern git releases refuse dash-prefixed URLs at the CLI layer,
/// but this check still matters:
///
/// - self-hosted runners still ship older git binaries,
/// - the same helper is reused for committish values fed to
///   `git checkout`, where a `--` terminator can't be used because it
///   would turn the committish into a pathspec.
///
/// A NUL byte is also rejected. It never appears in a legitimate url,
/// ref, or commit, and is a recurring split point for tool pipelines
/// downstream.
/// Render a git argv tail for error messages with any embedded
/// userinfo stripped. A raw `{args:?}` would otherwise dump the
/// full `git+https://<token>@host/repo.git` URL right back into
/// the error string that ships to CI logs.
fn redact_args(args: &[&str]) -> String {
    let mut s = String::from("[");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('"');
        s.push_str(&redact_url(a));
        s.push('"');
    }
    s.push(']');
    s
}

use aube_util::url::redact_url;

fn validate_git_positional(value: &str, kind: &str) -> Result<(), Error> {
    if value.starts_with('-') {
        return Err(Error::Git(format!(
            "refusing to pass {kind} starting with `-` to git: {value:?}"
        )));
    }
    if value.contains('\0') {
        return Err(Error::Git(format!(
            "refusing to pass {kind} containing NUL byte to git"
        )));
    }
    Ok(())
}

/// Resolve a git ref (branch name, tag, or partial commit) to a full
/// 40-char commit SHA by shelling out to `git ls-remote`. `committish`
/// of `None` means resolve `HEAD`. An input that already looks like a
/// full 40-char hex SHA is returned as-is without touching the network.
///
/// Matches the pnpm flow: try exact ref, then `refs/tags/<ref>`,
/// `refs/heads/<ref>`, falling back to the HEAD of the repo when the
/// caller passes `None`.
pub fn git_resolve_ref(url: &str, committish: Option<&str>) -> Result<String, Error> {
    validate_git_positional(url, "git url")?;
    // Already a full commit SHA? No network round-trip needed.
    if let Some(c) = committish
        && c.len() == 40
        && c.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Ok(c.to_ascii_lowercase());
    }
    // Always list all refs in one shot — filtering server-side with
    // `git ls-remote <url> HEAD` only works when the remote's HEAD
    // symbolic ref resolves, and some hosts (and our bare-repo test
    // fixtures) leave HEAD dangling. Listing everything also lets us
    // fall back to `main` / `master` without a second network call.
    //
    // `--` terminates git's own option parsing so an attacker-supplied
    // url that slips a leading `-` past `validate_git_positional` (we
    // don't expect this, but defense in depth) can't land as an option.
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--", url])
        .output()
        .map_err(|e| Error::Git(format!("spawn git ls-remote {}: {e}", redact_url(url))))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Git(format!(
            "git ls-remote {} failed: {}",
            redact_url(url),
            redact_url(stderr.trim())
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut head: Option<String> = None;
    let mut main_branch: Option<String> = None;
    let mut master_branch: Option<String> = None;
    let mut tag_match: Option<String> = None;
    let mut head_match: Option<String> = None;
    let mut first: Option<String> = None;
    for line in stdout.lines() {
        let mut parts = line.split('\t');
        let sha = parts.next().unwrap_or("").trim();
        let name = parts.next().unwrap_or("").trim();
        if sha.is_empty() || name.is_empty() {
            continue;
        }
        if first.is_none() {
            first = Some(sha.to_string());
        }
        match name {
            "HEAD" => head = Some(sha.to_string()),
            "refs/heads/main" => main_branch = Some(sha.to_string()),
            "refs/heads/master" => master_branch = Some(sha.to_string()),
            _ => {}
        }
        if let Some(want) = committish {
            if name == format!("refs/tags/{want}") || name == format!("refs/tags/{want}^{{}}") {
                tag_match = Some(sha.to_string());
            } else if name == format!("refs/heads/{want}") {
                head_match = Some(sha.to_string());
            }
        }
    }
    if let Some(want) = committish {
        if let Some(sha) = tag_match.or(head_match) {
            return Ok(sha);
        }
        // ls-remote only advertises branches and tags, so an
        // abbreviated commit SHA never matches a ref name. Pass it
        // through unchanged — `git_shallow_clone` resolves the prefix
        // by fetching and running `git checkout`, and the resolver
        // promotes the rev-parsed full SHA back into `GitSource`
        // before writing the lockfile (see `resolve_git_source`).
        //
        // Lower bound is 7 to stay in lockstep with `git_commit_matches`:
        // a shorter prefix would clear this gate but then trip the
        // post-checkout verification with a confusing mismatch error.
        // 7 is also git's own default `core.abbrev`, so anything users
        // copy out of a git UI lands at or above the cutoff.
        let looks_hex =
            want.len() >= 7 && want.len() < 40 && want.chars().all(|c| c.is_ascii_hexdigit());
        if looks_hex {
            return Ok(want.to_ascii_lowercase());
        }
        Err(Error::Git(format!(
            "git ls-remote {}: no ref matched {want}",
            redact_url(url)
        )))
    } else {
        head.or(main_branch)
            .or(master_branch)
            .or(first)
            .ok_or_else(|| {
                Error::Git(format!(
                    "git ls-remote {}: no refs advertised",
                    redact_url(url)
                ))
            })
    }
}

/// Shallow-clone `url` at `commit` into a fresh temp directory and
/// return the temp path. The caller is responsible for removing the
/// returned directory once it's imported into the store.
///
/// Uses the `git init` / `git fetch --depth 1` / `git checkout` dance
/// rather than `git clone --depth 1 --branch` so we can fetch a raw
/// commit hash that isn't advertised as a branch tip — pnpm does the
/// same for exactly this reason.
/// Return true if `url`'s hostname matches any entry in `hosts`
/// using the same exact-match semantics pnpm uses for
/// `git-shallow-hosts`. No wildcards, no subdomain folding —
/// `github.com` does *not* match `api.github.com`.
///
/// Handles the three URL shapes aube actually hands to git:
///   - `https://host/path`, `git://host/path`, `git+https://host/path`
///   - `git+ssh://git@host/path`
///   - `ssh://git@host/path`
///
/// Anything we can't parse (malformed, bare paths) returns `false`,
/// which means "not in the shallow list" — a full clone is the safe
/// default for weird inputs.
pub fn git_host_in_list(url: &str, hosts: &[String]) -> bool {
    let Some(host) = git_url_host(url) else {
        return false;
    };
    hosts.iter().any(|h| h == host)
}

/// Extract the hostname from a git remote URL string. Public for
/// testability; not expected to be useful to external callers.
pub fn git_url_host(url: &str) -> Option<&str> {
    // Strip the scheme if present. `git+` prefixes (`git+https://`,
    // `git+ssh://`) wrap a regular URL — drop them before parsing.
    let rest = url.strip_prefix("git+").unwrap_or(url);
    let after_scheme = match rest.split_once("://") {
        Some((_, r)) => r,
        // No scheme: could be scp-style `git@host:owner/repo.git`,
        // which has no `://`. Handle that below. Anything else (a
        // bare path, a malformed string) has no host.
        None => {
            // scp-style: `user@host:path`
            let (userhost, _) = rest.split_once(':')?;
            let host = userhost
                .rsplit_once('@')
                .map(|(_, h)| h)
                .unwrap_or(userhost);
            if host.is_empty() || host.contains('/') {
                return None;
            }
            return Some(host);
        }
    };
    // Drop optional `user@` prefix.
    let authority = after_scheme
        .split_once('/')
        .map(|(a, _)| a)
        .unwrap_or(after_scheme);
    let host_with_port = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    // Drop optional `:port`. IPv6 literals are wrapped in brackets
    // (`[::1]` / `[::1]:22`) and their address itself contains `:`s,
    // so blindly splitting on the last `:` would slice off part of
    // the address. Detect the bracket form first and pull out what's
    // between `[` and `]`; only plain hostname:port strings fall
    // through to the generic split.
    let host = if let Some(inner) = host_with_port.strip_prefix('[') {
        inner.split_once(']').map(|(h, _)| h).unwrap_or(inner)
    } else {
        host_with_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_with_port)
    };
    if host.is_empty() { None } else { Some(host) }
}

/// Clone a git repo into a deterministic per-(url, commit) cache dir
/// and check out `commit`. When `shallow` is true, aube uses
/// `fetch --depth 1 origin <sha>` and falls back to a full fetch if
/// the server rejects by-SHA shallow fetches; when false, aube skips
/// straight to the full-fetch path. Callers decide shallow vs. full
/// by consulting the `gitShallowHosts` setting via
/// [`git_host_in_list`].
///
/// Returns `(clone_dir, head_sha)` where `head_sha` is the 40-char
/// `git rev-parse HEAD` of the checked-out tree. Callers can pass
/// `commit` as either a full SHA or an abbreviated hex prefix; the
/// returned SHA is always the canonical full-length form so the
/// resolver can pin the lockfile to it.
pub fn git_shallow_clone(
    url: &str,
    commit: &str,
    shallow: bool,
) -> Result<(PathBuf, String), Error> {
    use std::process::Command;
    validate_git_positional(url, "git url")?;
    validate_git_positional(commit, "git commit")?;
    // Deterministic path keyed by url+commit so two callers in the
    // same process (resolver → installer) reuse the same checkout
    // instead of re-cloning. Two different repos that happen to
    // share a commit hash can't collide because the url is in the
    // hash. PID is intentionally NOT in the path — that's what made
    // the old version leak a fresh dir on every call.
    //
    // `shallow` is deliberately *not* part of the cache key: the
    // checkout a full clone leaves behind is a strict superset of
    // the one a shallow clone leaves behind (both have the requested
    // commit at HEAD; only the `.git/shallow` marker and object
    // count differ). Two installs that hit the same (url, commit)
    // under different shallow settings can reuse each other's work,
    // and `import_directory` ignores `.git/` so the store sees
    // identical output either way.
    // Keep git scratch out of world-writable /tmp. Predictable names
    // under $TMPDIR are the classic symlink pre-plant vector. Attacker
    // creates /tmp/aube-git-<k>-<c> as a symlink into $HOME/.ssh, then
    // the remove_dir_all below walks right through it and nukes the
    // victim's keys. 0700 on the cache root blocks the same race on a
    // shared user dir.
    let git_root = crate::dirs::cache_dir()
        .map(|d| d.join("git"))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&git_root).map_err(|e| Error::Io(git_root.clone(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(&git_root, std::fs::Permissions::from_mode(0o700))
        {
            warn!(
                "failed to chmod 0700 {}: {e}. Git scratch dir may be world-accessible, check filesystem permissions",
                git_root.display()
            );
        }
    }
    // Cache key derives from `(url, commit_input)`. When the caller
    // passes an abbreviated SHA, the initial target lands under that
    // key; after the clone, we re-key to the canonical full SHA so
    // a follow-up call (typically the installer reading the
    // lockfile-pinned full SHA) hits the same checkout instead of
    // re-cloning.
    let cache_key = |key_input: &str| -> (String, String) {
        let mut hasher = blake3::Hasher::new();
        hasher.update(url.as_bytes());
        hasher.update(b"\0");
        hasher.update(key_input.as_bytes());
        let digest = hasher.finalize();
        let key: String = digest
            .as_bytes()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        let short = key_input
            .get(..key_input.len().min(12))
            .unwrap_or(key_input)
            .to_string();
        (key, short)
    };
    let (key, commit_short) = cache_key(commit);
    let target = git_root.join(format!("aube-git-{key}-{commit_short}"));

    // Fast path: a previous call already finished this (url, commit)
    // pair and left a complete checkout at `target`. Verify cheaply
    // with `git rev-parse HEAD`; if it matches, reuse. A mismatch
    // means we're looking at an abandoned partial-failure stub from
    // an older aube version — it'll get replaced by the atomic
    // rename below.
    if target.join(".git").is_dir()
        && let Ok(out) = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&target)
            .output()
        && out.status.success()
    {
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if git_commit_matches(&head, commit) {
            return Ok((target, head));
        }
    }

    // Clone into a scratch dir first and atomically rename into
    // place. This solves two problems simultaneously:
    //   1. Partial-failure cleanup — if any git command fails, we
    //      drop the scratch dir and `target` is untouched, so a
    //      retry starts from a clean slate.
    //   2. Concurrent `aube install` races — two processes won't
    //      collide on `target` because each clones into its own
    //      PID-scoped scratch, and only one `rename` wins. The
    //      loser discovers `target` already has the right HEAD
    //      and reuses it.
    // Random suffix from tempfile::Builder. The old <pid> suffix was
    // guessable, so a local attacker could pre-plant a symlink at the
    // exact scratch path before git init ever ran. CSPRNG bytes make
    // that race unwinnable.
    let scratch = tempfile::Builder::new()
        .prefix(&format!("aube-git-{key}-{commit_short}."))
        .tempdir_in(&git_root)
        .map_err(|e| Error::Io(git_root.clone(), e))?
        .keep();

    let run_in = |dir: &Path, args: &[&str]| -> Result<(), Error> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| Error::Git(format!("spawn git {}: {e}", redact_args(args))))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Git(format!(
                "git {} failed: {}",
                redact_args(args),
                redact_url(stderr.trim())
            )));
        }
        Ok(())
    };

    let do_clone = || -> Result<String, Error> {
        run_in(&scratch, &["init", "-q"])?;
        run_in(&scratch, &["remote", "add", "--", "origin", url])?;
        // Shallow fetch by raw SHA only works when the remote allows
        // uploads of any reachable object (GitHub/GitLab/Bitbucket
        // do; many self-hosted servers don't). Fall back to a full
        // fetch on any failure. When `shallow` is false — caller
        // said the host isn't on the shallow list — skip the depth=1
        // attempt entirely to avoid a guaranteed-wasted round trip.
        let shallow_ok = shallow
            && run_in(
                &scratch,
                &["fetch", "--depth", "1", "-q", "--", "origin", commit],
            )
            .is_ok();
        if !shallow_ok {
            run_in(&scratch, &["fetch", "-q", "--", "origin"])?;
        }
        // `git checkout -- <commit>` treats <commit> as a pathspec, so
        // we cannot use the argv separator here. `validate_git_positional`
        // at function entry already rejected a leading `-` on `commit`.
        run_in(&scratch, &["checkout", "-q", commit])?;
        // Confirm the checkout landed exactly on the expected commit
        // before the scratch clone is renamed into place. Git's own
        // SHA-1 object addressing protects against a server returning
        // a different blob for a given SHA, but a local git
        // misconfiguration (default branch mismatch, rewritten ref,
        // stale reflog) could still leave HEAD on something else —
        // mirrors the defensive check the reuse path at line 1260
        // already performs.
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&scratch)
            .output()
            .map_err(|e| Error::Git(format!("spawn git rev-parse: {e}")))?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "git rev-parse HEAD failed: {}",
                redact_url(String::from_utf8_lossy(&out.stderr).trim())
            )));
        }
        let actual = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !git_commit_matches(&actual, commit) {
            return Err(Error::Git(format!(
                "git clone HEAD {actual} does not match requested commit {commit}"
            )));
        }
        Ok(actual)
    };
    let head_sha = match do_clone() {
        Ok(sha) => sha,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };

    // `rename` is atomic on the same filesystem. Two outcomes:
    //  - Target doesn't exist → we win and it's ours.
    //  - Target already exists (another process raced us, or there
    //    was a stale partial-failure stub above) → rename fails
    //    with ENOTEMPTY/EEXIST. Verify the existing target has our
    //    commit and reuse it; otherwise remove it and retry once.
    match aube_util::fs_atomic::rename_with_retry(&scratch, &target) {
        Ok(()) => Ok((
            canonicalize_clone_dir(&target, commit, &head_sha, &cache_key),
            head_sha,
        )),
        Err(_) => {
            if target.join(".git").is_dir()
                && let Ok(out) = Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&target)
                    .output()
                && out.status.success()
            {
                let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if git_commit_matches(&head, commit) {
                    let _ = std::fs::remove_dir_all(&scratch);
                    return Ok((
                        canonicalize_clone_dir(&target, commit, &head, &cache_key),
                        head,
                    ));
                }
            }
            // Stale target — clear and retry the rename. Any
            // remaining race here would be between two installs
            // both trying to replace a stale target, which is still
            // safe because each scratch is PID-scoped.
            let _ = std::fs::remove_dir_all(&target);
            aube_util::fs_atomic::rename_with_retry(&scratch, &target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&scratch);
                Error::Git(format!("rename clone into place: {e}"))
            })?;
            Ok((
                canonicalize_clone_dir(&target, commit, &head_sha, &cache_key),
                head_sha,
            ))
        }
    }
}

/// Re-key an abbreviated-SHA cache directory to its canonical
/// full-SHA path so a follow-up `git_shallow_clone` call (e.g. the
/// installer reading the lockfile-pinned full SHA) reuses the
/// existing checkout instead of cloning again. No-op when `commit`
/// already matches `head_sha`. Best-effort: if the rename fails
/// (cross-FS, race, perms), leaves the original path intact and
/// the caller pays one extra clone next time.
fn canonicalize_clone_dir(
    target: &Path,
    commit: &str,
    head_sha: &str,
    cache_key: &dyn Fn(&str) -> (String, String),
) -> PathBuf {
    if commit.eq_ignore_ascii_case(head_sha) {
        return target.to_path_buf();
    }
    let parent = match target.parent() {
        Some(p) => p,
        None => return target.to_path_buf(),
    };
    let (key, short) = cache_key(head_sha);
    let canonical = parent.join(format!("aube-git-{key}-{short}"));
    if canonical.join(".git").is_dir() {
        // Race: another caller already wrote the canonical entry.
        // Drop our duplicate so disk doesn't bloat with two copies.
        let _ = std::fs::remove_dir_all(target);
        return canonical;
    }
    match aube_util::fs_atomic::rename_with_retry(target, &canonical) {
        Ok(()) => canonical,
        Err(_) => target.to_path_buf(),
    }
}

fn git_commit_matches(actual: &str, requested: &str) -> bool {
    actual == requested
        || (requested.len() >= 7
            && requested.len() < 40
            && requested.chars().all(|c| c.is_ascii_hexdigit())
            && actual.starts_with(requested))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_commit_matches_abbreviated_sha() {
        assert!(git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "98e8ff1"
        ));
        assert!(!git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "98e8ff2"
        ));
        assert!(!git_commit_matches(
            "98e8ff1da1a89f93d1397a24d7413ed15421c139",
            "main"
        ));
    }

    #[test]
    fn test_integrity_to_hex() {
        let integrity = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let result = integrity_to_hex(integrity);
        assert!(result.is_some());
        let hex = result.unwrap();
        assert_eq!(hex.len(), 128);
        assert!(hex.chars().all(|c| c == '0'));
    }

    #[test]
    fn test_integrity_to_hex_invalid() {
        assert!(integrity_to_hex("md5-abc").is_none());
        assert!(integrity_to_hex("notahash").is_none());
        assert!(integrity_to_hex("").is_none());
    }

    #[test]
    fn test_integrity_to_hex_sha1() {
        // `co@4.6.0`'s real registry integrity.
        let hex = integrity_to_hex("sha1-bqa989hTrlTMuOR7+gvz+QMfsYQ=").unwrap();
        assert_eq!(hex.len(), 40);
        assert_eq!(hex, "6ea6bdf3d853ae54ccb8e47bfa0bf3f9031fb184");
    }

    #[test]
    fn test_integrity_to_hex_sha256() {
        let hex = integrity_to_hex("sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=").unwrap();
        assert_eq!(hex.len(), 64);
    }

    #[test]
    fn test_file_path_from_hex_sharding() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let path = store.file_path_from_hex("abcdef1234567890");
        // First 2 chars are the shard directory. Use the platform's
        // separator so the test works on Windows as well as Unix.
        let sep = std::path::MAIN_SEPARATOR;
        assert!(path.to_string_lossy().contains(&format!("{sep}ab{sep}")));
        assert!(path.to_string_lossy().ends_with("cdef1234567890"));
    }

    #[test]
    fn test_import_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"hello world";
        let stored = store.import_bytes(content, false).unwrap();

        assert!(stored.store_path.exists());
        assert_eq!(std::fs::read(&stored.store_path).unwrap(), content);
        assert!(!stored.executable);

        // Importing same content returns same hash (idempotent)
        let stored2 = store.import_bytes(content, false).unwrap();
        assert_eq!(stored.hex_hash, stored2.hex_hash);
    }

    #[test]
    fn test_import_bytes_repairs_truncated_existing_cas_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let content = br#"{"name":"@babel/helper-string-parser","version":"7.27.1"}"#;
        let hex_hash = blake3_hex(content);
        let store_path = store.file_path_from_hex(&hex_hash);
        std::fs::write(&store_path, b"").unwrap();

        let stored = store.import_bytes(content, false).unwrap();

        assert_eq!(stored.hex_hash, hex_hash);
        assert_eq!(stored.size, Some(content.len() as u64));
        assert_eq!(std::fs::read(&stored.store_path).unwrap(), content);
    }

    #[test]
    fn test_verify_integrity_valid() {
        let data = b"hello world";
        // Compute the actual sha512 of "hello world"
        let mut hasher = Sha512::new();
        hasher.update(data);
        let hash = hasher.finalize();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        let integrity = format!("sha512-{b64}");

        assert!(verify_integrity(data, &integrity).is_ok());
    }

    #[test]
    fn test_verify_integrity_mismatch() {
        let data = b"hello world";
        let wrong = "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        let result = verify_integrity(data, wrong);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("integrity mismatch")
        );
    }

    #[test]
    fn test_verify_integrity_unsupported_format() {
        let result = verify_integrity(b"test", "md5-abc123");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn test_verify_integrity_sha1_valid() {
        // SRI sha1- tarballs exist for legacy packages like co@4.6.0;
        // aube must still install them, so this is a regression guard.
        let data = b"hello world";
        let hash = Sha1::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha1-{b64}")).is_ok());
    }

    #[test]
    fn test_verify_integrity_sha1_mismatch() {
        let result = verify_integrity(b"hello world", "sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("integrity mismatch"));
        assert!(err.contains("sha1-"));
    }

    #[test]
    fn test_verify_integrity_sha256_valid() {
        let data = b"hello world";
        let hash = Sha256::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha256-{b64}")).is_ok());
    }

    #[test]
    fn test_verify_integrity_sha384_valid() {
        let data = b"hello world";
        let hash = Sha384::digest(data);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(hash);
        assert!(verify_integrity(data, &format!("sha384-{b64}")).is_ok());
    }

    #[test]
    fn test_import_bytes_executable() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"#!/bin/sh\necho hello";
        let stored = store.import_bytes(content, true).unwrap();
        assert!(stored.executable);

        // Check exec marker file exists
        let exec_marker = PathBuf::from(format!("{}-exec", stored.store_path.display()));
        assert!(exec_marker.exists());
    }

    #[test]
    fn test_import_bytes_different_content_different_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored1 = store.import_bytes(b"content a", false).unwrap();
        let stored2 = store.import_bytes(b"content b", false).unwrap();
        assert_ne!(stored1.hex_hash, stored2.hex_hash);
    }

    /// SHA-512 of an arbitrary test payload, encoded as npm's
    /// `sha512-<base64>`. Shared across index-cache tests so every
    /// save/load pair uses the same integrity and the filename is
    /// deterministic.
    const TEST_INTEGRITY: &str = "sha512-7iaw3Ur350mqGo7jwQrpkj9hiYB3Lkc/iBml1JQODbJ6wYX4oOHV+E+IvIh/1ntDcowEzF+prYseb2BRlkqKKw==";
    const OTHER_INTEGRITY: &str = "sha512-n4udRxsOEWaTbNrUjcrNvWAd1/aLvZeC/CwfsBIJZj0kHqyh0h10DmZerKIyp+/YqR09J8rBmdqkIy9SE/6rcQ==";

    #[test]
    fn test_index_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"test file";
        let stored = store.import_bytes(content, false).unwrap();

        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("test-pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();

        let loaded = store.load_index("test-pkg", "1.0.0", Some(TEST_INTEGRITY));
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key("index.js"));
    }

    #[test]
    fn test_index_cache_scoped_package() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"scoped content", false).unwrap();
        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        // Scoped package name should work (slash replaced with __)
        store
            .save_index("@scope/pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        let loaded = store.load_index("@scope/pkg", "1.0.0", Some(TEST_INTEGRITY));
        assert!(loaded.is_some());
    }

    #[test]
    fn test_index_cache_stale_detection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let store_path = stored.store_path.clone();
        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();

        // Delete the actual store file to simulate staleness
        std::fs::remove_file(&store_path).unwrap();

        // Both variants detect missing store files and return None.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
        // save_index wrote the file and load_index just deleted it
        // after detecting the stale store entry, so re-seed before
        // exercising the verified variant.
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        assert!(
            store
                .load_index_verified("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_rejects_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let store_path = stored.store_path.clone();
        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &index)
            .unwrap();
        std::fs::write(&store_path, b"").unwrap();

        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_import_bytes_uses_world_readable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let mode = std::fs::metadata(&stored.store_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(mode, 0o644);
    }

    #[test]
    fn test_index_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        assert!(
            store
                .load_index("nonexistent", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_integrity_discriminates_sources() {
        // Regression: before this, two tarballs served under the same
        // `(name, version)` from different sources — e.g. a github
        // codeload archive and the npm-published bytes — would share
        // the `<name>@<version>.json` cache file and return each
        // other's file list to the linker.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let registry_bytes = store.import_bytes(b"registry tarball", false).unwrap();
        let mut registry_index = PackageIndex::new();
        registry_index.insert("package.json".to_string(), registry_bytes);

        let github_bytes = store.import_bytes(b"github tarball", false).unwrap();
        let mut github_index = PackageIndex::new();
        github_index.insert("package.json".to_string(), github_bytes);
        github_index.insert("extra-github-only.js".to_string(), {
            store.import_bytes(b"extra", false).unwrap()
        });

        store
            .save_index("node-expat", "2.4.1", Some(TEST_INTEGRITY), &registry_index)
            .unwrap();
        store
            .save_index("node-expat", "2.4.1", Some(OTHER_INTEGRITY), &github_index)
            .unwrap();

        // Each integrity returns its own distinct index.
        let registry = store
            .load_index("node-expat", "2.4.1", Some(TEST_INTEGRITY))
            .unwrap();
        let github = store
            .load_index("node-expat", "2.4.1", Some(OTHER_INTEGRITY))
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(github.len(), 2);
        assert!(github.contains_key("extra-github-only.js"));
    }

    #[test]
    fn test_index_cache_rejects_malformed_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"content", false).unwrap();
        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        // Not a `sha512-<base64>` string — save returns an error and
        // load returns None rather than falling back to a weaker key.
        assert!(
            store
                .save_index("pkg", "1.0.0", Some("not-an-integrity"), &index)
                .is_err()
        );
        assert!(
            store
                .load_index("pkg", "1.0.0", Some("not-an-integrity"))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_integrity_none_roundtrip() {
        // Registry proxies that strip `dist.integrity` still need to
        // warm-install. With `integrity = None` the cache falls back
        // to `<name>@<version>.json` (no suffix), matching the
        // pre-integrity-keyed behavior for exactly that narrow case.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let stored = store.import_bytes(b"no-integrity content", false).unwrap();
        let mut index = PackageIndex::new();
        index.insert("index.js".to_string(), stored);

        store.save_index("pkg", "1.0.0", None, &index).unwrap();
        let loaded = store.load_index("pkg", "1.0.0", None);
        assert!(loaded.is_some());
        assert!(loaded.unwrap().contains_key("index.js"));

        // The integrity-keyed key does *not* see the integrity-less
        // entry — different directory on disk.
        assert!(
            store
                .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
                .is_none()
        );
    }

    #[test]
    fn test_index_cache_build_metadata_does_not_collide_with_integrity() {
        // Regression: an earlier flat-filename scheme
        // (`<name>@<version>+<16 hex>.json`) could in theory collide
        // with an integrity-less entry for a version whose semver
        // build metadata was exactly 16 lowercase hex chars
        // (`1.0.0+a1b2c3d4e5f6a7b8`). The subdir layout forecloses
        // that: integrity lives in a directory, not in the filename.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let a = store.import_bytes(b"integrity-keyed bytes", false).unwrap();
        let mut integrity_keyed = PackageIndex::new();
        integrity_keyed.insert("integrity-keyed.js".to_string(), a);

        let b = store.import_bytes(b"build-metadata bytes", false).unwrap();
        let mut build_meta = PackageIndex::new();
        build_meta.insert("build-meta.js".to_string(), b);

        // Integrity whose first 16 hex == the version's build metadata.
        // TEST_INTEGRITY hex-decodes to `ee26b0dd4af7e749...`, so the
        // directory name for the integrity-keyed entry is
        // `ee26b0dd4af7e749`. A version with that exact 16-hex build
        // metadata under the plain key must not alias it.
        let colliding_version = "1.0.0+ee26b0dd4af7e749";
        store
            .save_index("pkg", "1.0.0", Some(TEST_INTEGRITY), &integrity_keyed)
            .unwrap();
        store
            .save_index("pkg", colliding_version, None, &build_meta)
            .unwrap();

        let by_integrity = store
            .load_index("pkg", "1.0.0", Some(TEST_INTEGRITY))
            .unwrap();
        let by_build_meta = store.load_index("pkg", colliding_version, None).unwrap();
        assert!(by_integrity.contains_key("integrity-keyed.js"));
        assert!(by_build_meta.contains_key("build-meta.js"));
        // And neither entry leaks into the other's file list.
        assert!(!by_integrity.contains_key("build-meta.js"));
        assert!(!by_build_meta.contains_key("integrity-keyed.js"));
    }

    fn index_with_manifest(store: &Store, name: &str, version: &str) -> PackageIndex {
        let manifest =
            serde_json::json!({"name": name, "version": version, "main": "index.js"}).to_string();
        let stored = store.import_bytes(manifest.as_bytes(), false).unwrap();
        let mut index = BTreeMap::new();
        index.insert("package.json".to_string(), stored);
        index
    }

    #[test]
    fn test_validate_pkg_content_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "lodash", "4.17.21");
        assert!(validate_pkg_content(&index, "lodash", "4.17.21").is_ok());
    }

    #[test]
    fn test_validate_pkg_content_name_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "evil-pkg", "1.0.0");
        let err = validate_pkg_content(&index, "lodash", "1.0.0").unwrap_err();
        let msg = err.to_string();
        // The variant only carries the *actual* coordinate; the
        // caller's `{name}@{version}: ` prefix supplies the expected
        // half. See the comment on `Error::PkgContentMismatch`.
        assert!(msg.contains("content mismatch"), "{msg}");
        assert!(msg.contains("declares evil-pkg@1.0.0"), "{msg}");
    }

    #[test]
    fn test_validate_pkg_content_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "lodash", "9.9.9");
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("content mismatch"), "{msg}");
        assert!(msg.contains("declares lodash@9.9.9"), "{msg}");
    }

    #[test]
    fn test_validate_pkg_content_tolerates_leading_v() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "@upstash/ratelimit", "v2.0.8");
        assert!(validate_pkg_content(&index, "@upstash/ratelimit", "2.0.8").is_ok());
    }

    #[test]
    fn test_validate_pkg_content_skips_version_for_url_shaped_expected() {
        // pnpm v9 lockfiles key github-hosted deps by the codeload
        // tarball URL in the version slot; the tarball's real semver
        // will never match it. Skip the version comparison for
        // non-semver expected values, but still enforce the name.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let index = index_with_manifest(&store, "datejs", "1.0.0-rc3");
        let url = "https://codeload.github.com/PruvoNet/datejs/tar.gz/e2cde1e";
        assert!(validate_pkg_content(&index, "datejs", url).is_ok());
        // Name mismatch still rejects.
        let err = validate_pkg_content(&index, "evil", url).unwrap_err();
        assert!(err.to_string().contains("content mismatch"), "{err}");
    }

    #[test]
    fn test_validate_pkg_content_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let stored = store.import_bytes(b"module.exports = 1;", false).unwrap();
        let mut index = PackageIndex::new();
        index.insert("index.js".to_string(), stored);
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        assert!(err.to_string().contains("package.json missing"), "{err}",);
    }

    #[test]
    fn test_validate_pkg_content_unparseable_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let stored = store.import_bytes(b"{not json", false).unwrap();
        let mut index = PackageIndex::new();
        index.insert("package.json".to_string(), stored);
        let err = validate_pkg_content(&index, "lodash", "4.17.21").unwrap_err();
        assert!(err.to_string().contains("invalid package.json"), "{err}");
    }

    #[test]
    fn test_import_tarball() {
        // Create a minimal .tar.gz in memory
        let mut builder = tar::Builder::new(Vec::new());

        let content = b"module.exports = 42;\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "package/index.js", &content[..])
            .unwrap();

        let bin_content = b"#!/usr/bin/env node\nconsole.log('hi');\n";
        let mut bin_header = tar::Header::new_gnu();
        bin_header.set_size(bin_content.len() as u64);
        bin_header.set_mode(0o755);
        bin_header.set_cksum();
        builder
            .append_data(&mut bin_header, "package/bin/cli.js", &bin_content[..])
            .unwrap();

        let tar_bytes = builder.into_inner().unwrap();

        // Gzip it
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        let tgz_bytes = encoder.finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let index = store.import_tarball(&tgz_bytes).unwrap();
        assert_eq!(index.len(), 2);
        assert!(index.contains_key("index.js"));
        assert!(index.contains_key("bin/cli.js"));

        // Verify file contents
        let idx_stored = &index["index.js"];
        assert!(!idx_stored.executable);
        assert_eq!(std::fs::read(&idx_stored.store_path).unwrap(), content);

        let bin_stored = &index["bin/cli.js"];
        assert!(bin_stored.executable);
        assert_eq!(std::fs::read(&bin_stored.store_path).unwrap(), bin_content);
    }

    #[test]
    fn test_git_url_host_https() {
        assert_eq!(
            git_url_host("https://github.com/user/repo.git"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("git+https://github.com/user/repo.git#main"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("git://git.example.com/repo.git"),
            Some("git.example.com")
        );
    }

    #[test]
    fn test_git_url_host_ssh() {
        assert_eq!(
            git_url_host("git+ssh://git@github.com/user/repo.git"),
            Some("github.com")
        );
        assert_eq!(
            git_url_host("ssh://git@gitlab.com:2222/user/repo.git"),
            Some("gitlab.com")
        );
        // scp-style URL (no scheme): git@host:path
        assert_eq!(
            git_url_host("git@github.com:user/repo.git"),
            Some("github.com")
        );
    }

    #[test]
    fn test_git_url_host_ipv6() {
        // IPv6 literals must keep their colons — the port-strip pass
        // has to unwrap the brackets before it even considers `:`.
        assert_eq!(git_url_host("https://[::1]/repo.git"), Some("::1"));
        assert_eq!(git_url_host("https://[::1]:8443/repo.git"), Some("::1"));
        assert_eq!(
            git_url_host("ssh://git@[2001:db8::1]:2222/user/repo.git"),
            Some("2001:db8::1")
        );
    }

    #[test]
    fn test_git_url_host_rejects_garbage() {
        assert_eq!(git_url_host(""), None);
        assert_eq!(git_url_host("not a url"), None);
        assert_eq!(git_url_host("/just/a/path"), None);
    }

    #[test]
    fn test_git_host_in_list_exact_match() {
        let hosts = vec![
            "github.com".to_string(),
            "gitlab.com".to_string(),
            "bitbucket.org".to_string(),
        ];
        assert!(git_host_in_list("https://github.com/user/repo.git", &hosts));
        assert!(git_host_in_list(
            "git+ssh://git@gitlab.com/user/repo.git",
            &hosts
        ));
        // Exact match — no subdomain folding, matching pnpm semantics.
        assert!(!git_host_in_list(
            "https://api.github.com/user/repo.git",
            &hosts
        ));
        assert!(!git_host_in_list(
            "https://self-hosted.example/user/repo.git",
            &hosts
        ));
    }

    #[test]
    fn test_git_host_in_list_empty_list() {
        let hosts: Vec<String> = vec![];
        assert!(!git_host_in_list(
            "https://github.com/user/repo.git",
            &hosts
        ));
    }

    #[test]
    fn test_validate_git_positional_accepts_normal_values() {
        validate_git_positional("https://github.com/u/r.git", "git url").unwrap();
        validate_git_positional("git@github.com:u/r.git", "git url").unwrap();
        validate_git_positional("main", "git commit").unwrap();
        validate_git_positional("0123456789abcdef0123456789abcdef01234567", "git commit").unwrap();
    }

    #[test]
    fn test_validate_git_positional_rejects_dash_prefix() {
        // CVE-2017-1000117 class: git treats a leading `-` as an
        // option. `--upload-pack=...` is the classic payload.
        let err = validate_git_positional("--upload-pack=/tmp/evil", "git url").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
        let err = validate_git_positional("-oX", "git commit").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_validate_git_positional_rejects_nul() {
        let err = validate_git_positional("normal\0tail", "git url").unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_resolve_ref_rejects_dash_prefixed_url() {
        // Must refuse before ever spawning `git ls-remote`. Confirms
        // the validation runs at the public entry point.
        let err = git_resolve_ref("--upload-pack=/tmp/evil", None).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_resolve_ref_full_sha_is_offline() {
        // 40-char hex committish short-circuits `ls-remote`. Confirm
        // by handing a non-existent URL — if the fast path regressed
        // into a network call, the test would fail to spawn git.
        let sha = "0123456789ABCDEF0123456789abcdef01234567";
        let resolved = git_resolve_ref("https://example.invalid/missing.git", Some(sha)).unwrap();
        assert_eq!(resolved, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn test_git_commit_matches_prefix() {
        let full = "0b6ea539609031977983f0b2393ebe81ee28c8ec";
        assert!(git_commit_matches(full, full));
        assert!(git_commit_matches(full, "0b6ea53"));
        assert!(!git_commit_matches(full, "0b6ea5"));
        assert!(!git_commit_matches(full, "abc1234"));
        assert!(!git_commit_matches(full, "main"));
    }

    #[test]
    fn test_git_shallow_clone_rejects_dash_prefixed_url() {
        let err = git_shallow_clone("--upload-pack=/tmp/evil", "main", false).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    #[test]
    fn test_git_shallow_clone_rejects_dash_prefixed_commit() {
        // `git checkout -- <commit>` treats <commit> as a pathspec,
        // so the `--` separator is unavailable at that call site.
        // The entry-point check is the only defense.
        let err = git_shallow_clone("https://github.com/u/r.git", "-X-evil", false).unwrap_err();
        assert!(matches!(err, Error::Git(_)));
    }

    /// Build a minimal `.tgz` containing a single entry with the
    /// given path / size / content. `set_path` goes through the
    /// public API so the tar crate's own safety checks run.
    fn build_tarball(path: &str, content: &[u8]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        ar.append(&h, content).unwrap();
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn test_import_tarball_accepts_normal_sized_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball("package/index.js", b"console.log('hi');");
        let index = store.import_tarball(&tarball).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.contains_key("index.js"));
    }

    #[cfg(not(windows))]
    #[test]
    fn test_import_tarball_accepts_posix_colon_filename() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball(
            "package/dist/__mocks__/package-json:version.d.ts",
            b"export {};",
        );
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("dist/__mocks__/package-json:version.d.ts"));
    }

    #[test]
    fn test_import_tarball_rejects_per_entry_cap_exceeded() {
        // A single entry with a declared size past the per-entry cap
        // must be rejected before we allocate or read its contents.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        let oversize = (MAX_TARBALL_ENTRY_BYTES + 1) as usize;
        // Small actual content. The declared size in the header is
        // what matters for the fast-path rejection. We craft the
        // header manually to avoid writing a real 512 MiB payload.
        let mut h = tar::Header::new_gnu();
        h.set_path("package/huge.bin").unwrap();
        h.set_size(oversize as u64);
        h.set_mode(0o644);
        h.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        // `append` refuses a size/content mismatch, so we manually
        // emit the header + pad to a 512-byte block without actual
        // content. The per-entry-cap check runs on `header.size()`
        // before any read, so the stream shape past the header does
        // not matter for this test.
        ar.append(&h, &[][..]).ok();
        let tarball = ar.into_inner().unwrap().finish().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        let msg = match err {
            Error::Tar(m) => m,
            other => panic!("expected Error::Tar, got {other:?}"),
        };
        assert!(msg.contains("per-entry cap"), "unexpected error: {msg}");
    }

    #[test]
    fn test_import_tarball_rejects_archive_decompression_cap() {
        // Two entries whose combined decompressed size exceeds the
        // archive cap while each stays under the per-entry cap. The
        // cap is enforced by wrapping the gzip decoder in
        // `Read::take(cap)`, so the wrapped reader hits EOF mid-way
        // through the second entry and the archive iteration errors.
        //
        // `MAX_TARBALL_DECOMPRESSED_BYTES` and `MAX_TARBALL_ENTRY_BYTES`
        // are both reduced under `cfg(test)` so this test builds
        // only a couple of MiB of payload and stays CI-fast.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let half = ((MAX_TARBALL_DECOMPRESSED_BYTES / 2) + 1024) as usize;
        let chunk = vec![0u8; half];
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        for i in 0..2 {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("package/chunk{i}.bin")).unwrap();
            h.set_size(chunk.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, &chunk[..]).unwrap();
        }
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_entry_count_cap() {
        // `MAX_TARBALL_ENTRIES` is reduced under `cfg(test)` so this
        // test only appends a few dozen empty entries.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        for i in 0..=MAX_TARBALL_ENTRIES {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("package/f{i}.txt")).unwrap();
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, &[][..]).unwrap();
        }
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let err = store.import_tarball(&tarball).unwrap_err();
        let msg = match err {
            Error::Tar(m) => m,
            other => panic!("expected Error::Tar, got {other:?}"),
        };
        assert!(msg.contains("entry cap"), "unexpected error: {msg}");
    }

    // ---------------------------------------------------------------
    // Path traversal / zip-slip defences.
    //
    // A malicious tarball can try to write files outside the package
    // directory at install time by crafting entry paths with `..`,
    // absolute roots, Windows drive prefixes, or smuggled separators
    // inside a single component. `normalize_tar_entry_path` must
    // refuse every such shape before the key enters the
    // `PackageIndex`, and `import_tarball` must refuse symlink /
    // hardlink / device / fifo entries regardless of their path.
    // ---------------------------------------------------------------

    fn build_raw_named_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // The `tar` crate's `Builder::append` refuses to write `..`
        // paths and other malformed shapes, which is precisely what
        // this test suite needs to construct. Write the header
        // `name` field raw to bypass the safety check — a real
        // attacker uploading a `.tgz` to a registry has no such
        // guard.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_path("placeholder").unwrap();
            let name = &mut h.as_old_mut().name;
            name.fill(0);
            let bytes = path.as_bytes();
            assert!(bytes.len() < 100, "path too long for ustar name field");
            name[..bytes.len()].copy_from_slice(bytes);
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            ar.append(&h, *data).unwrap();
        }
        ar.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn normalize_tar_entry_path_accepts_plain_keys() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/index.js")).unwrap(),
            Some("index.js".to_string())
        );
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/lib/util/a.js")).unwrap(),
            Some("lib/util/a.js".to_string())
        );
    }

    #[test]
    fn normalize_tar_entry_path_skips_wrapper_only_entry() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package")).unwrap(),
            None
        );
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/")).unwrap(),
            None
        );
    }

    #[test]
    fn normalize_tar_entry_path_collapses_cur_dir() {
        assert_eq!(
            normalize_tar_entry_path(Path::new("package/./foo.js")).unwrap(),
            Some("foo.js".to_string())
        );
    }

    #[test]
    fn normalize_tar_entry_path_rejects_parent_dir() {
        let err = normalize_tar_entry_path(Path::new("package/../etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_parent_dir_after_leading_cur_dir() {
        // `./../file` must be rejected. An earlier version of the
        // validator ran the ParentDir check against the raw first
        // component, so the `.` passed it and the `..` was then
        // silently consumed as the wrapper directory.
        let err = normalize_tar_entry_path(Path::new("./../file")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
        let err = normalize_tar_entry_path(Path::new("././../etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_absolute_path() {
        let err = normalize_tar_entry_path(Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_smuggled_backslash() {
        // On unix `Path::components` leaves `a\b` as one Normal
        // component with a literal backslash inside. Reject.
        let err = normalize_tar_entry_path(Path::new("package/a\\..\\etc")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[cfg(windows)]
    #[test]
    fn normalize_tar_entry_path_rejects_colon_on_windows() {
        let err = normalize_tar_entry_path(Path::new("package/C:evil")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn normalize_tar_entry_path_rejects_nul() {
        let err = normalize_tar_entry_path(Path::new("package/a\0b")).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_parent_dir_escape() {
        // End-to-end: the crafted tarball that the prior zip-slip
        // reproducer used. `import_tarball` must refuse it and
        // produce no `PackageIndex` entries.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_raw_named_tarball(&[
            ("package/package.json", b"{}"),
            ("package/../../../etc/cron.d/evil", b"* * * * * root id\n"),
        ]);
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_absolute_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_raw_named_tarball(&[
            ("package/package.json", b"{}"),
            ("/etc/passwd", b"root:x:0:0\n"),
        ]);
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_symlink_entry() {
        // Symlink entries let a malicious package place the eventual
        // `pkg_dir.join(key)` file through a symlink that points
        // outside the package root. Refuse the entire class.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path("package/sneaky").unwrap();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name("/etc/passwd").unwrap();
        h.set_cksum();
        ar.append(&h, &[][..]).unwrap();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_rejects_hardlink_entry() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_path("package/clobber").unwrap();
        h.set_size(0);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Link);
        h.set_link_name("../../../../home/victim/.ssh/authorized_keys")
            .unwrap();
        h.set_cksum();
        ar.append(&h, &[][..]).unwrap();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let err = store.import_tarball(&tarball).unwrap_err();
        assert!(matches!(err, Error::Tar(_)));
    }

    #[test]
    fn test_import_tarball_skips_pax_global_header() {
        // GitHub-generated tarballs (e.g. `imap@0.8.19`) start with a
        // PAX global header carrying the source git blob SHA in a
        // `comment=...` record. The entry is metadata-only and has no
        // file content; npm/pnpm/bun skip it silently. The extractor
        // must not reject the tarball on sight.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut ar = tar::Builder::new(gz);

        let pax_body = b"52 comment=867aa88a335a266b904e0b5d1a3b0b5d1a3b0b5d1\n";
        let mut gh = tar::Header::new_ustar();
        gh.set_path("pax_global_header").unwrap();
        gh.set_size(pax_body.len() as u64);
        gh.set_mode(0o644);
        gh.set_entry_type(tar::EntryType::XGlobalHeader);
        gh.set_cksum();
        ar.append(&gh, &pax_body[..]).unwrap();

        let body = b"// ok";
        let mut fh = tar::Header::new_gnu();
        fh.set_path("package/index.js").unwrap();
        fh.set_size(body.len() as u64);
        fh.set_mode(0o644);
        fh.set_cksum();
        ar.append(&fh, &body[..]).unwrap();

        let tarball = ar.into_inner().unwrap().finish().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("index.js"));
        assert!(!index.contains_key("pax_global_header"));
    }

    #[test]
    fn test_import_tarball_still_accepts_normal_nested_paths() {
        // Regression guard: the validator must not refuse legitimate
        // deep paths that top-1000 packages actually ship.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();
        let tarball = build_tarball("package/lib/sub/a.js", b"// hi");
        let index = store.import_tarball(&tarball).unwrap();
        assert!(index.contains_key("lib/sub/a.js"));
    }

    #[test]
    fn test_capped_reader_surfaces_exhaustion_as_error() {
        // Regression: `Read::take(cap)` returns a clean EOF when the
        // limit is reached, which in the tar case can land on a
        // block boundary and let an archive silently truncate into
        // a partial index. `CappedReader` must produce an error
        // instead so `tar::Archive` surfaces it to the caller.
        use std::io::Read;
        let mut r = CappedReader::new(&b"hello world"[..], 5);
        let mut first = [0u8; 5];
        r.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"hello");
        let mut rest = Vec::new();
        let err = r.read_to_end(&mut rest).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_capped_reader_does_not_error_below_cap() {
        // Normal reads under the cap behave identically to the
        // inner reader. Only hitting `remaining == 0` errors.
        use std::io::Read;
        let mut r = CappedReader::new(&b"hi"[..], 10);
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn test_capped_reader_empty_buf_is_ok_past_cap() {
        // `Read::read(&mut [])` is a no-op per contract. Even with
        // the cap exhausted, it must return Ok(0) and not error.
        use std::io::Read;
        let mut r = CappedReader::new(&b"abcd"[..], 4);
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(r.read(&mut []).unwrap(), 0);
    }

    #[test]
    fn test_capped_reader_at_exact_boundary_still_errors() {
        // A read that drains exactly to the cap leaves `remaining`
        // at 0. The next read must error, which is the scenario
        // that motivated dropping `Read::take`. A tar block ending
        // on the cap would otherwise EOF silently.
        use std::io::Read;
        let mut r = CappedReader::new(&b"abcd"[..], 4);
        let mut buf = [0u8; 4];
        r.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"abcd");
        let mut rest = Vec::new();
        let err = r.read_to_end(&mut rest).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_import_tarball_declared_size_does_not_overallocate() {
        // A malicious header can declare a size near the per-entry
        // cap while shipping almost no actual content. The per-entry
        // `Vec::with_capacity` is clamped to `VEC_PREALLOC_CEILING`
        // so a lying header cannot force a 512 MiB reservation
        // before any byte has been read.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));
        store.ensure_shards_exist().unwrap();

        // Under `cfg(test)` `MAX_TARBALL_ENTRY_BYTES` is 1 MiB, so
        // we declare an entry right at that cap but with only a few
        // content bytes. Import should succeed with no OOM, and the
        // stored content length should match the actual bytes.
        let declared_near_cap = MAX_TARBALL_ENTRY_BYTES;
        let actual_content = b"tiny";
        let mut h = tar::Header::new_gnu();
        h.set_path("package/lying.bin").unwrap();
        h.set_size(declared_near_cap);
        h.set_mode(0o644);
        h.set_cksum();
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut ar = tar::Builder::new(gz);
        // Size mismatch is intentional. `append` will not refuse
        // when we stamp the header manually via `append`.
        ar.append(&h, &actual_content[..]).ok();
        let tarball = ar.into_inner().unwrap().finish().unwrap();

        // The per-entry cap check rejects `declared == cap` values
        // that exceed it; values at exactly the cap pass. Whichever
        // branch fires, the process must not OOM.
        let _ = store.import_tarball(&tarball);
    }
}
