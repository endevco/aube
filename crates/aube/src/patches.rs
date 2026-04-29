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

/// True when `rel` is a project-relative patch path that stays within
/// the project root. Refuses absolute paths, Windows drive or UNC
/// prefixes, NUL bytes, and any `..` component. Used as a read-side
/// guard so a hostile manifest cannot point the patch loader at
/// arbitrary files (e.g. `/etc/passwd` or `\\server\share\secret`).
fn is_safe_patch_rel(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute() || p.has_root() {
        return false;
    }
    // Reject a leading drive letter (`C:foo`) that `is_absolute` does
    // not always catch on the non-Windows host that rendered the
    // lockfile.
    if rel.len() >= 2 && rel.as_bytes()[1] == b':' {
        return false;
    }
    p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
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
/// `pnpm-workspace.yaml` and return them keyed by `name@version`.
/// Workspace-yaml entries (pnpm v10+ canonical location) win over
/// `package.json` on key conflict. Missing patch files become a hard
/// error — that matches pnpm, which refuses to install with a
/// declared-but-missing patch.
pub fn load_patches(cwd: &Path) -> Result<BTreeMap<String, ResolvedPatch>> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();

    let manifest_path = cwd.join("package.json");
    if manifest_path.exists() {
        let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
            .map_err(miette::Report::new)
            .wrap_err("failed to read package.json")?;
        entries.extend(manifest.pnpm_patched_dependencies());
    }

    let ws_config = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to read pnpm-workspace.yaml")?;
    entries.extend(ws_config.patched_dependencies);

    let mut out = BTreeMap::new();
    for (key, rel) in entries {
        let (name, version) = split_patch_key(&key)?;
        // Refuse absolute paths and `..` traversal in the manifest-
        // declared patch path so a hostile `package.json` cannot
        // coerce `aube install` into reading an arbitrary file off
        // disk. The linker already guards the *apply* side with
        // `is_safe_rel_component`, and mirroring the same check on
        // the *read* side keeps the trust boundary uniform.
        if !is_safe_patch_rel(&rel) {
            return Err(miette!(
                "refusing unsafe patch path for {key}: {rel:?} (absolute, UNC, or contains `..`)"
            ));
        }
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

/// Where the next `aube patch-commit` write should land. pnpm v10
/// moved `patchedDependencies` out of `package.json` into the workspace
/// yaml so users can document patches with YAML comments — when a
/// project is already using the new home, append there. Otherwise fall
/// back to `package.json` for projects that predate the move.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchDestination {
    PackageJson,
    WorkspaceYaml(PathBuf),
}

/// Pick which file `aube patch-commit` should mutate. Precedence:
/// 1. Whichever file already declares any `patchedDependencies` wins.
///    A workspace-yaml entry wins on a tie because aube's
///    `load_patches` already gives the workspace yaml precedence on
///    key conflict, so writing there keeps the live entry in sync.
/// 2. Otherwise, prefer the workspace yaml when one exists on disk —
///    matches pnpm v10+ convention.
/// 3. Otherwise, default to `package.json`.
fn patch_destination(cwd: &Path) -> PatchDestination {
    let ws_target = aube_manifest::workspace::workspace_yaml_target(cwd);
    let ws_has_patches = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        .map(|c| !c.patched_dependencies.is_empty())
        .unwrap_or(false);
    if ws_has_patches {
        return PatchDestination::WorkspaceYaml(ws_target);
    }
    let pkg_has_patches = read_package_json_patched_dependencies(cwd)
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if pkg_has_patches {
        return PatchDestination::PackageJson;
    }
    if ws_target.exists() {
        return PatchDestination::WorkspaceYaml(ws_target);
    }
    PatchDestination::PackageJson
}

/// Add or replace an entry in `patchedDependencies`. The destination is
/// resolved by `patch_destination` — workspace yaml when the project is
/// already using pnpm v10+ conventions, `package.json` otherwise.
/// Returns the path that was rewritten so the caller can report it to
/// the user.
pub fn upsert_patched_dependency(cwd: &Path, key: &str, rel_patch_path: &str) -> Result<PathBuf> {
    match patch_destination(cwd) {
        PatchDestination::PackageJson => {
            edit_patched_dependencies(cwd, |map| {
                map.insert(
                    key.to_string(),
                    serde_json::Value::String(rel_patch_path.to_string()),
                );
            })?;
            Ok(cwd.join("package.json"))
        }
        PatchDestination::WorkspaceYaml(path) => {
            aube_manifest::workspace::upsert_workspace_patched_dependency(
                &path,
                key,
                rel_patch_path,
            )
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to write {}", path.display()))?;
            Ok(path)
        }
    }
}

/// Drop an entry from `patchedDependencies` in whichever file declares
/// it. Returns the files that were rewritten — empty when neither
/// location held the entry. Each side peeks before rewriting so a
/// remove that targets only one location doesn't churn the other (the
/// workspace yaml in particular can hold user-authored comments that
/// yaml_serde's round-trip would discard).
pub fn remove_patched_dependency(cwd: &Path, key: &str) -> Result<Vec<PathBuf>> {
    let mut rewritten = Vec::new();
    let ws_target = aube_manifest::workspace::workspace_yaml_target(cwd);
    if ws_target.exists()
        && aube_manifest::workspace::remove_workspace_patched_dependency(&ws_target, key)
            .map_err(miette::Report::new)
            .wrap_err_with(|| format!("failed to write {}", ws_target.display()))?
    {
        rewritten.push(ws_target);
    }
    if read_package_json_patched_dependencies(cwd)?.contains_key(key) {
        let mut existed = false;
        edit_patched_dependencies(cwd, |map| {
            existed = map.remove(key).is_some();
        })?;
        if existed {
            rewritten.push(cwd.join("package.json"));
        }
    }
    Ok(rewritten)
}

/// Read every declared `patchedDependencies` entry across both the
/// workspace yaml and `package.json`, with workspace-yaml entries
/// winning on key conflict (same precedence used by `load_patches`).
pub fn read_patched_dependencies(cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = read_package_json_patched_dependencies(cwd)?;
    let ws_config = aube_manifest::workspace::WorkspaceConfig::load(cwd)
        .map_err(miette::Report::new)
        .wrap_err("failed to read workspace yaml")?;
    out.extend(ws_config.patched_dependencies);
    Ok(out)
}

fn read_package_json_patched_dependencies(cwd: &Path) -> Result<BTreeMap<String, String>> {
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

    #[test]
    fn destination_prefers_workspace_yaml_when_it_already_holds_patches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        match patch_destination(dir.path()) {
            PatchDestination::WorkspaceYaml(p) => {
                assert_eq!(p, dir.path().join("pnpm-workspace.yaml"));
            }
            d => panic!("expected workspace yaml destination, got {d:?}"),
        }
    }

    #[test]
    fn destination_prefers_package_json_when_it_already_holds_patches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"pnpm\":{\"patchedDependencies\":{\"a@1.0.0\":\"patches/a@1.0.0.patch\"}}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        assert_eq!(patch_destination(dir.path()), PatchDestination::PackageJson);
    }

    #[test]
    fn destination_prefers_workspace_yaml_when_present_and_neither_holds_patches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        match patch_destination(dir.path()) {
            PatchDestination::WorkspaceYaml(p) => {
                assert_eq!(p, dir.path().join("pnpm-workspace.yaml"));
            }
            d => panic!("expected workspace yaml destination, got {d:?}"),
        }
    }

    #[test]
    fn destination_falls_back_to_package_json_when_no_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        assert_eq!(patch_destination(dir.path()), PatchDestination::PackageJson);
    }

    #[test]
    fn remove_returns_each_rewritten_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\"pnpm\":{\"patchedDependencies\":{\"a@1.0.0\":\"patches/a@1.0.0.patch\"}}}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "patchedDependencies:\n  \"a@1.0.0\": patches/a@1.0.0.patch\n",
        )
        .unwrap();
        let rewritten = remove_patched_dependency(dir.path(), "a@1.0.0").unwrap();
        let names: Vec<String> = rewritten
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, vec!["pnpm-workspace.yaml", "package.json"]);
    }

    #[test]
    fn remove_returns_empty_when_neither_file_holds_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'pkgs/*'\n",
        )
        .unwrap();
        let rewritten = remove_patched_dependency(dir.path(), "missing@9.9.9").unwrap();
        assert!(rewritten.is_empty());
    }
}
