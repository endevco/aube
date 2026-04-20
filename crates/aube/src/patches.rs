//! Shared helpers for the `patch` / `patch-commit` / `patch-remove`
//! commands and the install-time patch application path.
//!
//! Patches are stored alongside the project (default `patches/`) and
//! tracked in `package.json` under `pnpm.patchedDependencies` —
//! `{ "name@version": "patches/name@version.patch" }`. We mirror pnpm's
//! shape exactly so the field round-trips between the two tools.

use miette::{Context, IntoDiagnostic, Result, miette};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One resolved patch entry. The key is `name@version` (the same
/// string used as the `pnpm.patchedDependencies` map key), `path` is
/// the absolute path on disk, and `content` is the raw patch text the
/// linker applies.
#[derive(Debug, Clone)]
pub struct ResolvedPatch {
    pub key: String,
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    #[allow(dead_code)]
    pub path: PathBuf,
    pub content: String,
}

impl ResolvedPatch {
    /// Short hex digest of the patch content. Folded into the graph
    /// hash so a patched node lives at a different virtual-store path
    /// than the unpatched one.
    pub fn content_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.content.as_bytes());
        hex::encode(h.finalize())
    }
}

/// Split a `name@version` patch key into its parts. Mirrors
/// `commands::split_name_spec` but always requires a version (a bare
/// name is rejected — patches are always per-version).
pub fn split_patch_key(key: &str) -> Result<(String, String)> {
    let (name, ver) = if let Some(rest) = key.strip_prefix('@') {
        let slash = rest
            .find('/')
            .ok_or_else(|| miette!("invalid patch key {key:?}: scoped name missing slash"))?;
        let after = &rest[slash + 1..];
        let at = after
            .find('@')
            .ok_or_else(|| miette!("invalid patch key {key:?}: missing version"))?;
        let split = 1 + slash + 1 + at;
        (&key[..split], &key[split + 1..])
    } else {
        let at = key
            .find('@')
            .ok_or_else(|| miette!("invalid patch key {key:?}: missing version"))?;
        (&key[..at], &key[at + 1..])
    };
    if name.is_empty() || ver.is_empty() {
        return Err(miette!("invalid patch key {key:?}"));
    }
    Ok((name.to_string(), ver.to_string()))
}

/// Read every patch declared in the project's `package.json` and
/// return them keyed by `name@version`. Missing patch files become a
/// hard error — that matches pnpm, which refuses to install with a
/// declared-but-missing patch.
pub fn load_patches(cwd: &Path) -> Result<BTreeMap<String, ResolvedPatch>> {
    let manifest_path = cwd.join("package.json");
    if !manifest_path.exists() {
        return Ok(BTreeMap::new());
    }
    let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;

    let mut out = BTreeMap::new();
    for (key, rel) in manifest.pnpm_patched_dependencies() {
        let (name, version) = split_patch_key(&key)?;
        let path = cwd.join(&rel);
        let content = std::fs::read_to_string(&path)
            .into_diagnostic()
            .map_err(|e| {
                miette!(
                    "failed to read patch file {} for {key}: {e}",
                    path.display()
                )
            })?;
        out.insert(
            key.clone(),
            ResolvedPatch {
                key,
                name,
                version,
                path,
                content,
            },
        );
    }
    Ok(out)
}

/// Add or replace an entry in `pnpm.patchedDependencies`, preserving
/// the rest of `package.json`. Writes the file back with a trailing
/// newline.
pub fn upsert_patched_dependency(cwd: &Path, key: &str, rel_patch_path: &str) -> Result<()> {
    edit_patched_dependencies(cwd, |map| {
        map.insert(
            key.to_string(),
            serde_json::Value::String(rel_patch_path.to_string()),
        );
    })
}

/// Drop an entry from `pnpm.patchedDependencies`. Returns `true` if
/// the entry existed.
pub fn remove_patched_dependency(cwd: &Path, key: &str) -> Result<bool> {
    let mut existed = false;
    edit_patched_dependencies(cwd, |map| {
        existed = map.remove(key).is_some();
    })?;
    Ok(existed)
}

/// Read the current `pnpm.patchedDependencies` map from
/// `package.json`. Returns an empty map if the field is absent.
pub fn read_patched_dependencies(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let manifest_path = cwd.join("package.json");
    if !manifest_path.exists() {
        return Ok(BTreeMap::new());
    }
    let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    Ok(manifest.pnpm_patched_dependencies())
}

/// Read `package.json` as a generic `Value`, run `f` on the
/// `pnpm.patchedDependencies` object (creating it if needed), and
/// write the file back. Using a raw `Value` round-trip rather than
/// the typed `PackageJson` keeps unrelated keys, ordering, and
/// serialization details intact.
fn edit_patched_dependencies<F>(cwd: &Path, f: F) -> Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>),
{
    let manifest_path = cwd.join("package.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read package.json: {e}"))?;
    let mut value = aube_manifest::parse_json::<serde_json::Value>(&manifest_path, raw)
        .map_err(miette::Report::new)
        .wrap_err("failed to parse package.json")?;

    let obj = value
        .as_object_mut()
        .ok_or_else(|| miette!("package.json is not an object"))?;
    let pnpm = obj
        .entry("pnpm".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let pnpm_obj = pnpm
        .as_object_mut()
        .ok_or_else(|| miette!("`pnpm` field in package.json is not an object"))?;
    let patched = pnpm_obj
        .entry("patchedDependencies".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let patched_obj = patched
        .as_object_mut()
        .ok_or_else(|| miette!("`pnpm.patchedDependencies` is not an object"))?;

    f(patched_obj);

    // If the user removed the last patch, drop the now-empty
    // `patchedDependencies` (and `pnpm` itself when it's empty too)
    // so we don't leave noise in the manifest.
    if patched_obj.is_empty() {
        pnpm_obj.remove("patchedDependencies");
    }
    if pnpm_obj.is_empty() {
        obj.remove("pnpm");
    }

    let mut out = serde_json::to_string_pretty(&value)
        .into_diagnostic()
        .map_err(|e| miette!("failed to serialize package.json: {e}"))?;
    out.push('\n');
    std::fs::write(&manifest_path, out)
        .into_diagnostic()
        .map_err(|e| miette!("failed to write package.json: {e}"))?;
    Ok(())
}

/// Recursively copy `src` into `dst`, following file content but
/// preserving relative layout. Used by `aube patch` to snapshot a
/// package out of the virtual store into both a "source" reference
/// directory and a "user edit" directory.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)
        .into_diagnostic()
        .map_err(|e| miette!("failed to create {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", src.display()))?
    {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read entry under {}: {e}", src.display()))?;
        let ty = entry
            .file_type()
            .into_diagnostic()
            .map_err(|e| miette!("failed to stat {}: {e}", entry.path().display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&from, &to)?;
        } else if ty.is_symlink() {
            // Skip symlinks — packages we extract from the virtual
            // store can contain `node_modules` symlinks pointing into
            // sibling packages, which we don't want to drag into the
            // patch source dir.
            continue;
        } else {
            std::fs::copy(&from, &to).into_diagnostic().map_err(|e| {
                miette!("failed to copy {} -> {}: {e}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_simple() {
        let (n, v) = split_patch_key("is-positive@3.1.0").unwrap();
        assert_eq!(n, "is-positive");
        assert_eq!(v, "3.1.0");
    }

    #[test]
    fn split_scoped() {
        let (n, v) = split_patch_key("@babel/core@7.0.0").unwrap();
        assert_eq!(n, "@babel/core");
        assert_eq!(v, "7.0.0");
    }

    #[test]
    fn split_missing_version_errors() {
        assert!(split_patch_key("is-positive").is_err());
        assert!(split_patch_key("@babel/core").is_err());
    }
}
