//! Process-wide directory lookups.
//!
//! `cwd()` returns the logical command working directory. It starts as
//! `std::env::current_dir()`, but in-process command fanout can retarget
//! it with [`set_cwd`] instead of spawning a fresh `aube` process just to
//! get clean global state.

use miette::{IntoDiagnostic, miette};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

static CWD: RwLock<Option<PathBuf>> = RwLock::new(None);

/// Return the process's current working directory, resolving it via
/// `std::env::current_dir()` on first call and caching the result.
/// Returns an owned `PathBuf` as a drop-in for the previous inline
/// `std::env::current_dir().into_diagnostic()?` pattern.
pub fn cwd() -> miette::Result<PathBuf> {
    if let Some(p) = CWD.read().expect("cwd lock poisoned").as_ref() {
        return Ok(p.clone());
    }

    let mut cwd = CWD.write().expect("cwd lock poisoned");
    if let Some(p) = cwd.as_ref() {
        return Ok(p.clone());
    }
    let p = std::env::current_dir().into_diagnostic()?;
    Ok(cwd.insert(p).clone())
}

/// Walk upward from `start` looking for the nearest directory that
/// contains a `package.json`. Returns the directory path, or `None` if
/// no ancestor has one. Used by `install` and `run` so subdirectories
/// of a project (e.g. `repo/docs`) resolve to the project root,
/// matching pnpm's behavior of walking up when run outside a project
/// directory.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if dir.join("package.json").is_file() {
            return Some(dir.to_path_buf());
        }
    }
    None
}

/// Walk upward from `start` looking for the nearest workspace root.
///
/// A workspace root is any ancestor containing `aube-workspace.yaml` or
/// `pnpm-workspace.yaml`. The aube-owned name wins at read time elsewhere,
/// but discovery only needs to know whether either file marks the root.
pub fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        if dir.join("aube-workspace.yaml").exists() || dir.join("pnpm-workspace.yaml").exists() {
            Some(dir.to_path_buf())
        } else {
            None
        }
    })
}

/// Return the nearest project root at or above the cached cwd.
///
/// Commands that operate on the current project should use this
/// instead of [`cwd`] so running from a subdirectory targets the same
/// package root as `install` and `run`.
pub fn project_root() -> miette::Result<PathBuf> {
    let initial_cwd = cwd()?;
    find_project_root(&initial_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            initial_cwd.display()
        )
    })
}

/// Return the nearest project root, falling back to the cached cwd when
/// no ancestor contains `package.json`.
///
/// This is for commands that can also operate outside a package tree
/// but should still inherit project config when launched from a
/// subdirectory, such as `fetch` and registry/config helpers.
pub fn project_root_or_cwd() -> miette::Result<PathBuf> {
    let initial_cwd = cwd()?;
    Ok(find_project_root(&initial_cwd).unwrap_or(initial_cwd))
}

/// Retarget the logical cwd to an explicit path.
pub fn set_cwd(path: &Path) -> miette::Result<()> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().into_diagnostic()?.join(path)
    };
    *CWD.write().expect("cwd lock poisoned") = Some(path);
    Ok(())
}
