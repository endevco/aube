#[macro_use]
extern crate log;

pub mod dirs;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The global content-addressable store, owned by aube.
///
/// Default location: `~/.aube-store/v1/files/`
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
}

/// Index of all files in a package, keyed by relative path within the package.
pub type PackageIndex = BTreeMap<String, StoredFile>;

impl Store {
    /// Open the store at the default location (~/.aube-store/v1/files/).
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
        let cache_dir = root.parent().unwrap_or(&root).join("aube-cache");
        Self { root, cache_dir }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory for cached package indices. Public so introspection
    /// commands (`aube find-hash`) can walk it directly.
    pub fn index_dir(&self) -> PathBuf {
        self.cache_dir.join("index")
    }

    /// Directory for the global virtual store (materialized packages).
    pub fn virtual_store_dir(&self) -> PathBuf {
        self.cache_dir.join("virtual-store")
    }

    /// Directory for cached packument metadata (abbreviated/corgi format).
    /// Versioned so we can bump the schema without breaking old caches —
    /// old caches at older versions stay around until manually pruned.
    pub fn packument_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("packuments-v1")
    }

    /// Directory for cached *full* packument JSON (non-corgi) used by
    /// human-facing commands like `aube view` that need fields the resolver
    /// doesn't parse (`description`, `repository`, `license`, `keywords`,
    /// `maintainers`). Separate from `packument_cache_dir` because the
    /// corgi and full responses have different shapes.
    pub fn packument_full_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("packuments-full-v1")
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
    pub fn load_index(&self, name: &str, version: &str) -> Option<PackageIndex> {
        self.load_index_inner(name, version, false)
    }

    /// Load a package index, optionally verifying that all store files still exist.
    /// The verified variant is slower (stat per file) but detects a corrupted store.
    pub fn load_index_verified(&self, name: &str, version: &str) -> Option<PackageIndex> {
        self.load_index_inner(name, version, true)
    }

    fn load_index_inner(
        &self,
        name: &str,
        version: &str,
        verify_files: bool,
    ) -> Option<PackageIndex> {
        let safe_name = name.replace('/', "__");
        let index_path = self.index_dir().join(format!("{safe_name}@{version}.json"));
        let content = xx::file::read_to_string(&index_path).ok()?;
        let index: PackageIndex = serde_json::from_str(&content).ok()?;
        if verify_files {
            if !index.values().all(|f| f.store_path.exists()) {
                trace!("cache stale: {name}@{version}");
                let _ = xx::file::remove_file(&index_path);
                return None;
            }
        } else {
            // Quick sanity check: verify at least one file exists in the store
            if let Some(f) = index.values().next()
                && !f.store_path.exists()
            {
                trace!("cache stale: {name}@{version}");
                let _ = xx::file::remove_file(&index_path);
                return None;
            }
        }
        trace!("cache hit: {name}@{version}");
        Some(index)
    }

    /// Save a package index to the cache.
    pub fn save_index(&self, name: &str, version: &str, index: &PackageIndex) -> Result<(), Error> {
        let safe_name = name.replace('/', "__");
        let index_path = self.index_dir().join(format!("{safe_name}@{version}.json"));
        let json =
            serde_json::to_string(index).map_err(|e| Error::Tar(format!("serialize: {e}")))?;
        xx::file::write(&index_path, json).map_err(|e| Error::Xx(e.to_string()))?;
        trace!("cached index: {name}@{version}");
        Ok(())
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

    /// Import a single file's content into the store. Returns the stored file info.
    ///
    /// Hot path on cold installs: callers should invoke
    /// [`Store::ensure_shards_exist`] once before a batch of imports so
    /// this function can skip the per-file `mkdirp`. When shards don't
    /// exist yet, the `create_new` open will fail with `NotFound`; we
    /// fall back to the slow path for correctness.
    pub fn import_bytes(&self, content: &[u8], executable: bool) -> Result<StoredFile, Error> {
        let hex_hash = blake3::hash(content).to_hex().to_string();

        let store_path = self.file_path_from_hex(&hex_hash);

        // Fast path: open-with-create-new combines the existence check
        // and the open into a single syscall. On a cold CAS this does
        // one open(O_CREAT|O_EXCL|O_WRONLY) per file and replaces the
        // previous stat+create pair (~15k redundant stats per cold
        // install). On a warm CAS, concurrent writers are safe: EEXIST
        // means another writer already materialized this content (same
        // hash = same bytes), so we skip and share the entry.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&store_path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(content)
                    .map_err(|e| Error::Io(store_path.clone(), e))?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another writer already populated this content — skip.
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Shard dir missing. `ensure_shards_exist` pre-creates
                // all 256, so this only fires when the caller didn't
                // call it (or the shard tree was wiped mid-install).
                // Fall back to the slow path for correctness.
                xx::file::write(&store_path, content).map_err(|e| Error::Xx(e.to_string()))?;
            }
            Err(e) => return Err(Error::Io(store_path.clone(), e)),
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
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&exec_marker)
            {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    xx::file::write(&exec_marker, "").map_err(|e| Error::Xx(e.to_string()))?;
                }
                Err(e) => return Err(Error::Io(exec_marker, e)),
            }
        }

        Ok(StoredFile {
            hex_hash,
            store_path,
            executable,
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
        let gz = flate2::read::GzDecoder::new(tarball_bytes);
        let mut archive = tar::Archive::new(gz);
        let mut index = BTreeMap::new();

        // Serial walk — each tarball is decoded by one spawn_blocking
        // task on the fetch-phase blocking pool, which is already
        // parallel across packages. A rayon-inner parallelization
        // inside each tarball measured slower in practice because
        // ~250 concurrent imports all competing for the same CPU
        // cores amplifies contention more than per-tarball
        // parallelism helps.
        for entry in archive.entries().map_err(|e| Error::Tar(e.to_string()))? {
            let mut entry = entry.map_err(|e| Error::Tar(e.to_string()))?;

            if entry.header().entry_type().is_dir() {
                continue;
            }

            let raw_path = entry
                .path()
                .map_err(|e| Error::Tar(e.to_string()))?
                .to_path_buf();
            // Strip the first path component. npm convention is `package/`, but
            // some packages publish tarballs with the package name (or other names)
            // as the top-level directory. The first component is always the
            // wrapper and should be stripped regardless of its name.
            let rel_path = {
                let mut components = raw_path.components();
                components.next(); // skip the wrapper directory
                let stripped: PathBuf = components.collect();
                let s = if stripped.as_os_str().is_empty() {
                    raw_path.to_string_lossy().to_string()
                } else {
                    stripped.to_string_lossy().to_string()
                };
                // Package indices are keyed with `/` separators on every
                // platform so the lockfile and linker see identical keys
                // regardless of where the tarball was extracted.
                s.replace('\\', "/")
            };

            let mut content = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut content)
                .map_err(|e| Error::Tar(e.to_string()))?;

            let mode = entry.header().mode().unwrap_or(0o644);
            let executable = mode & 0o111 != 0;

            let stored = self.import_bytes(&content, executable)?;
            index.insert(rel_path, stored);
        }

        Ok(index)
    }
}

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

/// Verify that data matches an integrity hash (e.g., "sha512-<base64>").
/// Returns Ok(()) if valid, Err with details if mismatch.
pub fn verify_integrity(data: &[u8], expected: &str) -> Result<(), Error> {
    let Some(expected_b64) = expected.strip_prefix("sha512-") else {
        return Err(Error::Integrity(format!(
            "unsupported integrity format (expected sha512-...): {expected}"
        )));
    };

    let mut hasher = Sha512::new();
    hasher.update(data);
    let actual_bytes = hasher.finalize();

    use base64::Engine;
    let actual_b64 = base64::engine::general_purpose::STANDARD.encode(actual_bytes);

    if actual_b64 == expected_b64 {
        Ok(())
    } else {
        Err(Error::Integrity(format!(
            "integrity mismatch: expected sha512-{expected_b64}, got sha512-{actual_b64}"
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
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Tar(format!("invalid package.json: {e}")))?;
    let actual_name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let actual_version = v.get("version").and_then(|v| v.as_str()).unwrap_or("");
    if actual_name != expected_name || actual_version != expected_version {
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

/// Decode a pnpm-style `sha512-<base64>` integrity string into its raw
/// hex SHA-512 digest. Used by introspection commands that accept the
/// registry integrity format as an ergonomic input. Returns `None` if
/// the input isn't a well-formed integrity string.
pub fn integrity_to_hex(integrity: &str) -> Option<String> {
    let b64 = integrity.strip_prefix("sha512-")?;
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
        .map_err(|e| Error::Git(format!("spawn git ls-remote {url}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "git ls-remote {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
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
        // If the committish looks like a hex prefix but isn't a full
        // SHA, the user likely copy-pasted an abbreviated commit from
        // a git UI. ls-remote only lists advertised refs (branches /
        // tags), so an abbreviated commit never matches — surface a
        // clearer error instead of the generic "no ref matched".
        let looks_hex =
            want.len() >= 4 && want.len() < 40 && want.chars().all(|c| c.is_ascii_hexdigit());
        if looks_hex {
            return Err(Error::Git(format!(
                "git ls-remote {url}: `#{want}` looks like an abbreviated commit SHA — aube requires a full 40-character SHA, or a branch/tag name"
            )));
        }
        Err(Error::Git(format!(
            "git ls-remote {url}: no ref matched {want}"
        )))
    } else {
        head.or(main_branch)
            .or(master_branch)
            .or(first)
            .ok_or_else(|| Error::Git(format!("git ls-remote {url}: no refs advertised")))
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
pub fn git_shallow_clone(url: &str, commit: &str, shallow: bool) -> Result<PathBuf, Error> {
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
    let mut hasher = blake3::Hasher::new();
    hasher.update(url.as_bytes());
    hasher.update(b"\0");
    hasher.update(commit.as_bytes());
    let digest = hasher.finalize();
    let key: String = digest
        .as_bytes()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let commit_short = commit.get(..commit.len().min(12)).unwrap_or(commit);
    let target = std::env::temp_dir().join(format!("aube-git-{key}-{commit_short}"));

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
        && String::from_utf8_lossy(&out.stdout).trim() == commit
    {
        return Ok(target);
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
    let scratch = std::env::temp_dir().join(format!(
        "aube-git-{key}-{commit_short}.tmp.{}",
        std::process::id()
    ));
    if scratch.exists() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    std::fs::create_dir_all(&scratch).map_err(|e| Error::Io(scratch.clone(), e))?;

    let run_in = |dir: &Path, args: &[&str]| -> Result<(), Error> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| Error::Git(format!("spawn git {args:?}: {e}")))?;
        if !out.status.success() {
            return Err(Error::Git(format!(
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    };

    let do_clone = || -> Result<(), Error> {
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
        Ok(())
    };
    if let Err(e) = do_clone() {
        let _ = std::fs::remove_dir_all(&scratch);
        return Err(e);
    }

    // `rename` is atomic on the same filesystem. Two outcomes:
    //  - Target doesn't exist → we win and it's ours.
    //  - Target already exists (another process raced us, or there
    //    was a stale partial-failure stub above) → rename fails
    //    with ENOTEMPTY/EEXIST. Verify the existing target has our
    //    commit and reuse it; otherwise remove it and retry once.
    match std::fs::rename(&scratch, &target) {
        Ok(()) => Ok(target),
        Err(_) => {
            if target.join(".git").is_dir()
                && let Ok(out) = Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&target)
                    .output()
                && out.status.success()
                && String::from_utf8_lossy(&out.stdout).trim() == commit
            {
                let _ = std::fs::remove_dir_all(&scratch);
                return Ok(target);
            }
            // Stale target — clear and retry the rename. Any
            // remaining race here would be between two installs
            // both trying to replace a stale target, which is still
            // safe because each scratch is PID-scoped.
            let _ = std::fs::remove_dir_all(&target);
            std::fs::rename(&scratch, &target).map_err(|e| {
                let _ = std::fs::remove_dir_all(&scratch);
                Error::Git(format!("rename clone into place: {e}"))
            })?;
            Ok(target)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(integrity_to_hex("sha256-abc").is_none());
        assert!(integrity_to_hex("").is_none());
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

    #[test]
    fn test_index_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        let content = b"test file";
        let stored = store.import_bytes(content, false).unwrap();

        let mut index = BTreeMap::new();
        index.insert("index.js".to_string(), stored);

        store.save_index("test-pkg", "1.0.0", &index).unwrap();

        let loaded = store.load_index("test-pkg", "1.0.0");
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
        store.save_index("@scope/pkg", "1.0.0", &index).unwrap();
        let loaded = store.load_index("@scope/pkg", "1.0.0");
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

        store.save_index("pkg", "1.0.0", &index).unwrap();

        // Delete the actual store file to simulate staleness
        std::fs::remove_file(&store_path).unwrap();

        // Both load_index and load_index_verified detect missing files
        let loaded = store.load_index("pkg", "1.0.0");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_index_cache_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("files"));

        assert!(store.load_index("nonexistent", "1.0.0").is_none());
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
}
