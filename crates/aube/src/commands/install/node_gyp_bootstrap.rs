//! On-demand bootstrap of `node-gyp` into an aube-owned cache dir.
//!
//! Many npm packages ship a native addon and rely on `node-gyp` being
//! available on `PATH` during their `install` lifecycle — either
//! explicitly (`"install": "node-gyp rebuild"`), implicitly through
//! aube's `default_install_script` fallback when the package ships a
//! `binding.gyp` with no install/preinstall, or transitively via
//! tooling like `node-gyp-build` that shells out to `node-gyp`. pnpm
//! and npm solve this by bundling node-gyp with themselves; aube (a
//! Rust binary) bootstraps it lazily on first need.
//!
//! User precedence: if `node-gyp` is already resolvable on the
//! ambient `PATH` (system install, nvm, a shim in a test fixture),
//! [`ensure`] returns `None` and we stay out of the way — the user's
//! copy wins. Otherwise node-gyp is installed under
//! `<cache_dir>/tools/node-gyp/<bucket>/` and the returned `.bin`
//! dir is prepended to the lifecycle script's `PATH` *after* the
//! dep's own `.bin`.
//!
//! The install is performed by recursively invoking the current aube
//! binary with `install --ignore-scripts` inside a freshly-written
//! `package.json` that pins node-gyp. An `xx::fslock` lock keyed off
//! the tool dir serializes concurrent bootstraps across processes;
//! the fast-path existence check short-circuits every subsequent
//! invocation.
use miette::{IntoDiagnostic, WrapErr, miette};
use std::path::{Path, PathBuf};

/// Major-version pin. Bumping the bucket invalidates the cache and
/// triggers a re-bootstrap on the next install.
const BUCKET: &str = "v12";
/// Semver range passed to `aube install`. Keep aligned with `BUCKET`.
const SPEC: &str = "^12.0.0";

#[cfg(windows)]
const BINARY_NAMES: &[&str] = &["node-gyp.cmd", "node-gyp.exe", "node-gyp"];
#[cfg(not(windows))]
const BINARY_NAMES: &[&str] = &["node-gyp"];

fn node_gyp_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if node_gyp_bin_exists(&dir) {
            return true;
        }
    }
    false
}

/// True if `bin_dir` contains any of the platform's accepted
/// `node-gyp` shim filenames. On Windows npm installs `node-gyp.cmd`
/// (sometimes `.exe` alongside), so a bare-string check would always
/// miss the bootstrapped shim and the fast-path would never fire.
fn node_gyp_bin_exists(bin_dir: &Path) -> bool {
    BINARY_NAMES.iter().any(|name| bin_dir.join(name).exists())
}

fn tool_root() -> miette::Result<PathBuf> {
    let cache = aube_store::dirs::cache_dir()
        .ok_or_else(|| miette!("could not resolve cache dir for node-gyp bootstrap"))?;
    Ok(cache.join("tools").join("node-gyp"))
}

/// Returns `Some(bin_dir)` containing a freshly-bootstrapped `node-gyp`
/// when the ambient `PATH` doesn't already provide one, or `None` when
/// the user already has a copy on `PATH` — in which case we don't
/// touch their setup.
pub async fn ensure() -> miette::Result<Option<PathBuf>> {
    if node_gyp_on_path() {
        return Ok(None);
    }
    let root = tool_root()?;
    let tool_dir = root.join(BUCKET);
    let bin_dir = tool_dir.join("node_modules").join(".bin");
    if node_gyp_bin_exists(&bin_dir) {
        return Ok(Some(bin_dir));
    }
    let lock_key = root.join(format!("{BUCKET}.lock"));
    let tool_dir_blocking = tool_dir.clone();
    let bin_dir_blocking = bin_dir.clone();
    tokio::task::spawn_blocking(move || {
        bootstrap_blocking(&lock_key, &tool_dir_blocking, &bin_dir_blocking)
    })
    .await
    .into_diagnostic()
    .wrap_err("node-gyp bootstrap task panicked")??;
    Ok(Some(bin_dir))
}

fn bootstrap_blocking(lock_key: &Path, tool_dir: &Path, bin_dir: &Path) -> miette::Result<()> {
    std::fs::create_dir_all(tool_dir).into_diagnostic()?;
    let _lock = xx::fslock::FSLock::new(lock_key)
        .with_callback(|_| {
            tracing::info!("waiting for another aube process to finish bootstrapping node-gyp");
        })
        .lock()
        .map_err(|e| miette!("failed to acquire node-gyp bootstrap lock: {e}"))?;
    // Re-check under the lock: another process may have raced us.
    if node_gyp_bin_exists(bin_dir) {
        return Ok(());
    }
    let manifest = format!(
        r#"{{"name":"aube-tool-node-gyp","private":true,"dependencies":{{"node-gyp":"{SPEC}"}}}}"#
    );
    std::fs::write(tool_dir.join("package.json"), manifest).into_diagnostic()?;
    let exe = std::env::current_exe()
        .into_diagnostic()
        .wrap_err("could not locate current aube executable for node-gyp bootstrap")?;
    tracing::info!("bootstrapping node-gyp {SPEC} into {}", tool_dir.display());
    let status = std::process::Command::new(&exe)
        .args(["install", "--ignore-scripts", "--silent"])
        .current_dir(tool_dir)
        .status()
        .into_diagnostic()
        .wrap_err("failed to spawn recursive aube install for node-gyp bootstrap")?;
    if !status.success() {
        return Err(miette!(
            "recursive aube install failed while bootstrapping node-gyp (exit {:?}) — \
             pre-populate {} or run `aube install` once while online",
            status.code(),
            tool_dir.display()
        ));
    }
    if !node_gyp_bin_exists(bin_dir) {
        return Err(miette!(
            "node-gyp bootstrap completed but no shim found under {}",
            bin_dir.display()
        ));
    }
    Ok(())
}
