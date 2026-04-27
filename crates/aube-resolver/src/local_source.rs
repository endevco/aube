use crate::{Error, ResolveTask};
use aube_lockfile::{LocalSource, LockedPackage};
use aube_registry::client::RegistryClient;
use std::collections::BTreeMap;

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
pub(crate) fn rebase_local(
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

/// Walk a gzipped npm tarball once and return the raw bytes of its
/// top-level `package.json` entry. The wrapper directory name varies
/// (`package/`, but also e.g. GitHub's `owner-repo-<sha>/`), so we
/// match on the entry's basename plus a 2-component depth check
/// rather than a hardcoded prefix. Errors come back as plain
/// `String`s so each caller can wrap them with its own package
/// identity in whatever error type it prefers — used by both the
/// `file:` tarball path (`read_local_manifest`) and the remote
/// tarball resolver (`resolve_remote_tarball`).
/// Hard upper bound on the bytes read from the gzipped tarball stream
/// while looking for `package.json`. A 64 MiB ceiling is far above any
/// real npm package and keeps a hostile gzip bomb from amplifying into
/// arbitrary RAM. Mirrors `aube-store::MAX_TARBALL_DECOMPRESSED_BYTES`
/// in spirit — the resolver path was missed in the original cap pass.
const MAX_RESOLVE_TARBALL_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESOLVE_PACKAGE_JSON_BYTES: u64 = 8 * 1024 * 1024;

fn read_tarball_package_json(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    // Cap on the DECOMPRESSED output of the gzip stream so a hostile
    // tarball with large dummy entries before `package.json` cannot
    // amplify the fixed compressed input window into arbitrary RAM.
    // `bytes.take` would only bound the compressed read, which the
    // decoder is free to expand without ceiling.
    let gz = flate2::read::GzDecoder::new(bytes);
    let capped = gz.take(MAX_RESOLVE_TARBALL_DECOMPRESSED_BYTES);
    let mut archive = tar::Archive::new(capped);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let entry_path = entry.path().map_err(|e| e.to_string())?.to_path_buf();
        if entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "package.json")
            && entry_path.components().count() == 2
        {
            let mut buf = Vec::new();
            entry
                .take(MAX_RESOLVE_PACKAGE_JSON_BYTES + 1)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            if buf.len() as u64 > MAX_RESOLVE_PACKAGE_JSON_BYTES {
                return Err("package.json exceeds 8 MiB cap".to_string());
            }
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
pub(crate) fn read_local_manifest(
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

    let pj: aube_manifest::PackageJson = {
        let mut buf = content.clone();
        simd_json::serde::from_slice(&mut buf).or_else(|_| serde_json::from_slice(&content))
    }
    .map_err(|e| Error::Registry(local.specifier(), e.to_string()))?;
    Ok((
        pj.name.unwrap_or_default(),
        pj.version.unwrap_or_else(|| "0.0.0".to_string()),
        pj.dependencies,
    ))
}

pub(crate) fn dep_path_for(name: &str, version: &str) -> String {
    format!("{name}@{version}")
}

/// Match specifier prefixes that resolve to a non-registry source
/// (`file:`, `link:`, or a git URL form). Used by the resolver to
/// decide whether to dispatch the local/git branch instead of the
/// normal version-range lookup.
pub(crate) fn is_non_registry_specifier(s: &str) -> bool {
    if s.starts_with("link:") {
        return true;
    }
    // Git first so `https://host/repo.git` dispatches the git branch
    // rather than the broader bare-http tarball branch below.
    if aube_lockfile::parse_git_spec(s).is_some() {
        return true;
    }
    // Any remaining bare `http(s)://` URL is a tarball URL, per npm
    // semantics — the `.tgz` suffix is not required.
    if aube_lockfile::LocalSource::looks_like_remote_tarball_url(s) {
        return true;
    }
    // `file:` is a local-path prefix only when it *isn't* also a git
    // URL form — parse_git_spec already matched `file://…/repo.git`
    // above, so anything that reaches here is treated as a path.
    s.starts_with("file:")
}

pub(crate) fn should_block_exotic_subdep(
    task: &ResolveTask,
    resolved: &BTreeMap<String, LockedPackage>,
    block_exotic_subdeps: bool,
) -> bool {
    block_exotic_subdeps
        && !task.is_root
        && !task
            .parent
            .as_ref()
            .and_then(|parent| resolved.get(parent))
            .is_some_and(|pkg| {
                matches!(
                    pkg.local_source,
                    Some(LocalSource::Directory(_)) | Some(LocalSource::Link(_))
                )
            })
}

/// Turn a raw `GitSource` (committish parsed from the user's
/// specifier, empty `resolved`) into a fully-resolved one by running
/// `git ls-remote`, then shallow-cloning to read the package's own
/// `package.json` for version + transitive deps. The clone lives in
/// a commit-keyed temp directory; install-time materialization will
/// either reuse the same directory or re-run the shallow clone.
pub(crate) async fn resolve_git_source(
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
    let subpath = git.subpath.clone();
    let name_owned = name.to_string();
    let (local, version, deps) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
        // `git_resolve_ref` resolves branch / tag / HEAD names to a
        // 40-char SHA via `ls-remote`, but passes hex prefixes (i.e.
        // user-written abbreviated commit SHAs) through unchanged
        // because abbreviated commits aren't advertised as refs. The
        // canonical full SHA is recovered from `git_shallow_clone`'s
        // post-checkout `rev-parse HEAD`, so the lockfile always
        // pins the full form.
        let resolve_seed = aube_store::git_resolve_ref(&url, committish.as_deref())
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        let (clone_dir, resolved) = aube_store::git_shallow_clone(&url, &resolve_seed, shallow)
            .map_err(|e| Error::Registry(name_owned.clone(), e.to_string()))?;
        // `&path:/<sub>` narrows the package root to a subdirectory
        // of the cloned repo (pnpm-compatible). The manifest, version,
        // and transitive deps all come from the subdir's
        // `package.json`, not the repo root's.
        let pkg_root = match &subpath {
            Some(sub) => clone_dir.join(sub),
            None => clone_dir.clone(),
        };
        let manifest_bytes = std::fs::read(pkg_root.join("package.json")).map_err(|e| {
            let where_ = subpath
                .as_deref()
                .map(|s| format!(" at /{s}"))
                .unwrap_or_default();
            Error::Registry(
                name_owned.clone(),
                format!("read package.json in clone{where_}: {e}"),
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
                subpath,
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
pub(crate) async fn resolve_remote_tarball(
    name: &str,
    tarball: &aube_lockfile::RemoteTarballSource,
    client: &RegistryClient,
) -> Result<(LocalSource, String, BTreeMap<String, String>), Error> {
    let bytes = client
        .fetch_tarball_bytes(&tarball.url)
        .await
        .map_err(|e| {
            Error::Registry(
                name.to_string(),
                format!("fetch {}: {e}", aube_util::url::redact_url(&tarball.url)),
            )
        })?;
    let name_owned = name.to_string();
    let url = aube_util::url::redact_url(&tarball.url);
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

#[cfg(test)]
mod cve_audit_tarball_bomb {
    use super::*;
    use std::io::Write;

    fn build_zero_tarball(uncompressed_size: usize) -> Vec<u8> {
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let payload = vec![0u8; uncompressed_size];
            let mut header = tar::Header::new_gnu();
            header.set_path("pkg/package.json").unwrap();
            header.set_size(payload.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &payload[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::best());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        gz
    }

    fn build_dummy_then_package_json(dummy_size: usize) -> Vec<u8> {
        let mut tar_buf: Vec<u8> = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            let dummy = vec![0u8; dummy_size];
            let mut h1 = tar::Header::new_gnu();
            h1.set_path("pkg/dummy.bin").unwrap();
            h1.set_size(dummy.len() as u64);
            h1.set_mode(0o644);
            h1.set_cksum();
            builder.append(&h1, &dummy[..]).unwrap();
            let manifest = b"{\"name\":\"x\",\"version\":\"0.0.1\"}";
            let mut h2 = tar::Header::new_gnu();
            h2.set_path("pkg/package.json").unwrap();
            h2.set_size(manifest.len() as u64);
            h2.set_mode(0o644);
            h2.set_cksum();
            builder.append(&h2, &manifest[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = Vec::new();
        {
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::best());
            enc.write_all(&tar_buf).unwrap();
            enc.finish().unwrap();
        }
        gz
    }

    #[test]
    fn read_tarball_package_json_rejects_decompression_bomb() {
        let bomb = build_zero_tarball(200 * 1024 * 1024);
        assert!(
            bomb.len() < 400 * 1024,
            "compressed bomb too large to call this an amplification: {}",
            bomb.len()
        );
        let result = read_tarball_package_json(&bomb);
        assert!(
            result.is_err(),
            "200 MiB decompressed payload must be rejected by the cap, got {:?}",
            result.as_ref().map(|b| b.len())
        );
    }

    #[test]
    fn read_tarball_package_json_rejects_dummy_entry_amplification() {
        let bomb = build_dummy_then_package_json(200 * 1024 * 1024);
        assert!(
            bomb.len() < 400 * 1024,
            "compressed multi-entry bomb too large: {}",
            bomb.len()
        );
        let result = read_tarball_package_json(&bomb);
        assert!(
            result.is_err(),
            "decompressed dummy entry preceding package.json must hit the output cap"
        );
    }
}
