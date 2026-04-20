//! Lifecycle script runner for aube.
//!
//! **Security model**:
//! - Scripts from the **root package** (the project's own `package.json`)
//!   run by default. They're written by the user, so they're trusted the
//!   same way a user trusts `aube run <script>`.
//! - Scripts from **installed dependencies** (e.g. `node-gyp` postinstall
//!   from a native module) are SKIPPED by default. A package runs its
//!   lifecycle scripts only if the active [`BuildPolicy`] allows it —
//!   configured via `pnpm.allowBuilds` in `package.json`, `allowBuilds`
//!   in `pnpm-workspace.yaml`, or the escape-hatch
//!   `--dangerously-allow-all-builds` flag.
//! - `--ignore-scripts` forces everything off, matching pnpm/npm.

pub mod policy;

pub use policy::{AllowDecision, BuildPolicy, BuildPolicyError};

use aube_manifest::PackageJson;
use std::path::{Path, PathBuf};

/// Settings that affect every package-script shell aube spawns.
#[derive(Debug, Clone, Default)]
pub struct ScriptSettings {
    pub node_options: Option<String>,
    pub script_shell: Option<PathBuf>,
    pub unsafe_perm: Option<bool>,
    pub shell_emulator: bool,
}

static SCRIPT_SETTINGS: std::sync::OnceLock<std::sync::RwLock<ScriptSettings>> =
    std::sync::OnceLock::new();

fn script_settings_lock() -> &'static std::sync::RwLock<ScriptSettings> {
    SCRIPT_SETTINGS.get_or_init(|| std::sync::RwLock::new(ScriptSettings::default()))
}

/// Replace the process-wide script settings snapshot. CLI commands call
/// this after resolving `.npmrc` / workspace settings for the active
/// project.
pub fn set_script_settings(settings: ScriptSettings) {
    *script_settings_lock()
        .write()
        .expect("script settings lock poisoned") = settings;
}

fn script_settings() -> ScriptSettings {
    script_settings_lock()
        .read()
        .expect("script settings lock poisoned")
        .clone()
}

/// Prepend `bin_dir` to the current `PATH` using the platform's path
/// separator (`:` on Unix, `;` on Windows).
pub fn prepend_path(bin_dir: &Path) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries = vec![bin_dir.to_path_buf()];
    entries.extend(std::env::split_paths(&path));
    std::env::join_paths(entries).unwrap_or(path)
}

/// Spawn a shell command line. On Unix we go through `sh -c`, on
/// Windows through `cmd.exe /d /s /c` — matching what npm passes in
/// `@npmcli/run-script`.
///
/// On Windows, the script command line is appended with
/// [`std::os::windows::process::CommandExt::raw_arg`] instead of
/// the normal `.arg()` path. `.arg()` would run the string through
/// Rust's `CommandLineToArgvW`-oriented encoder, which wraps it in
/// `"..."` and escapes interior `"` as `\"` — but `cmd.exe` parses
/// command lines with a different set of rules and does not
/// understand `\"`, so a script like
/// `node -e "require('is-odd')(3)"` arrives mangled. `raw_arg`
/// hands the command line to `CreateProcessW` verbatim, so we
/// control the exact bytes cmd.exe sees. We wrap the whole script
/// in an outer pair of double quotes, which `/s` tells cmd.exe to
/// strip (just those outer quotes — the rest of the string is
/// preserved literally). This is the same trick
/// `@npmcli/run-script` and `node-cross-spawn` use.
pub fn spawn_shell(script_cmd: &str) -> tokio::process::Command {
    let settings = script_settings();
    #[cfg(unix)]
    {
        let mut cmd = tokio::process::Command::new(
            settings
                .script_shell
                .as_deref()
                .unwrap_or_else(|| Path::new("sh")),
        );
        cmd.arg("-c").arg(script_cmd);
        apply_script_settings_env(&mut cmd, &settings);
        cmd
    }
    #[cfg(windows)]
    {
        let mut cmd = tokio::process::Command::new(
            settings
                .script_shell
                .as_deref()
                .unwrap_or_else(|| Path::new("cmd.exe")),
        );
        if settings.script_shell.is_some() {
            cmd.arg("-c").arg(script_cmd);
        } else {
            // `/d` skips AutoRun, `/s` flips the quote-stripping rule
            // so only the *outer* `"..."` pair is removed, `/c` runs
            // the command and exits. Build the raw argv tail manually
            // so cmd.exe sees the original script bytes.
            cmd.raw_arg("/d /s /c \"").raw_arg(script_cmd).raw_arg("\"");
        }
        apply_script_settings_env(&mut cmd, &settings);
        cmd
    }
}

fn apply_script_settings_env(cmd: &mut tokio::process::Command, settings: &ScriptSettings) {
    // Strip credentials that aube itself owns before we spawn any
    // lifecycle script. AUBE_AUTH_TOKEN is aube's own registry login
    // token. No transitive postinstall has any business reading it.
    // NPM_TOKEN and NODE_AUTH_TOKEN stay untouched because release
    // flows ("npm publish" in a postpublish script) genuinely need
    // them. Matches what pnpm does today.
    cmd.env_remove("AUBE_AUTH_TOKEN");
    if let Some(node_options) = settings.node_options.as_deref() {
        cmd.env("NODE_OPTIONS", node_options);
    }
    if let Some(unsafe_perm) = settings.unsafe_perm {
        cmd.env(
            "npm_config_unsafe_perm",
            if unsafe_perm { "true" } else { "false" },
        );
    }
    if settings.shell_emulator {
        cmd.env("npm_config_shell_emulator", "true");
    }
}

/// Lifecycle hooks that `aube install` runs against the root package's
/// `scripts` field, in this order: `preinstall` → (dependencies link) →
/// `install` → `postinstall` → `prepare`. Matches pnpm / npm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleHook {
    PreInstall,
    Install,
    PostInstall,
    Prepare,
}

impl LifecycleHook {
    pub fn script_name(self) -> &'static str {
        match self {
            Self::PreInstall => "preinstall",
            Self::Install => "install",
            Self::PostInstall => "postinstall",
            Self::Prepare => "prepare",
        }
    }
}

/// Dependency lifecycle hooks, in the order aube runs them for each
/// allowlisted package. `prepare` is intentionally omitted — it's meant
/// for the root package and git-dep preparation, not installed tarballs.
pub const DEP_LIFECYCLE_HOOKS: [LifecycleHook; 3] = [
    LifecycleHook::PreInstall,
    LifecycleHook::Install,
    LifecycleHook::PostInstall,
];

/// Holds the real stderr fd saved before `aube` redirects fd 2 to
/// `/dev/null` under `--silent`. Child processes spawned through
/// `child_stderr()` get a fresh dup of this fd so their stderr still
/// reaches the user's terminal — `--silent` only silences aube's own
/// output, not the scripts / binaries it invokes (matches `pnpm
/// --loglevel silent`). A value of `-1` means silent mode is off and
/// children should inherit stderr normally.
#[cfg(unix)]
static SAVED_STDERR_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// Called once by `aube` after it saves + redirects fd 2. Passing
/// the caller-owned saved fd here means child processes spawned via
/// `child_stderr()` will write to the real terminal stderr instead of
/// `/dev/null`.
#[cfg(unix)]
pub fn set_saved_stderr_fd(fd: std::os::fd::RawFd) {
    SAVED_STDERR_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
}

/// Windows has no equivalent fd-based silencing plumbing: aube's
/// `SilentStderrGuard` is `libc::dup`/`libc::dup2` on fd 2, and those
/// calls are gated to unix in `aube`. The stub keeps the public
/// API shape identical so call sites compile unchanged.
#[cfg(not(unix))]
pub fn set_saved_stderr_fd(_fd: i32) {}

/// Returns a `Stdio` suitable for a child process's stderr. When silent
/// mode is active, this dups the saved real-stderr fd so the child
/// bypasses the `/dev/null` redirect on fd 2. Otherwise returns
/// `Stdio::inherit()`.
#[cfg(unix)]
pub fn child_stderr() -> std::process::Stdio {
    let fd = SAVED_STDERR_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return std::process::Stdio::inherit();
    }
    // SAFETY: `fd` was registered by `set_saved_stderr_fd` from a live
    // `dup` that `aube`'s `SilentStderrGuard` keeps open for the
    // duration of main. `BorrowedFd` only borrows, so this does not
    // transfer ownership.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    match borrowed.try_clone_to_owned() {
        Ok(owned) => std::process::Stdio::from(owned),
        Err(_) => std::process::Stdio::inherit(),
    }
}

#[cfg(not(unix))]
pub fn child_stderr() -> std::process::Stdio {
    std::process::Stdio::inherit()
}

/// Run a single npm-style script line through `sh -c` with the usual
/// environment (`$PATH` extended with `node_modules/.bin`, `INIT_CWD`,
/// `npm_lifecycle_event`, `npm_package_name`, `npm_package_version`).
///
/// `extra_bin_dir` is an optional directory prepended to `PATH` *before*
/// the project-level `.bin`. Dep lifecycle scripts pass the dep's own
/// sibling `node_modules/.bin/` so transitive binaries (e.g.
/// `prebuild-install`, `node-gyp`) declared in the dep's
/// `dependencies` are reachable. Root scripts pass `None` — their
/// transitive bins are already hoisted into the project-level `.bin`.
///
/// Inherits stdio from the parent so the user sees script output live.
/// Returns Err on non-zero exit so install fails fast if a lifecycle
/// script breaks, matching pnpm.
pub async fn run_script(
    script_dir: &Path,
    project_root: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    script_name: &str,
    script_cmd: &str,
    extra_bin_dir: Option<&Path>,
) -> Result<(), Error> {
    // PATH prepends (most-local-first): optional `extra_bin_dir` for
    // dep-local transitive bins, then the project root's
    // `<modules_dir>/.bin`. For root scripts `script_dir ==
    // project_root` and `extra_bin_dir` is `None`, which matches the
    // old behavior. `modules_dir_name` honors pnpm's `modulesDir`
    // setting — defaults to `"node_modules"` at the call site, but a
    // workspace may have configured something else.
    let project_bin = project_root.join(modules_dir_name).join(".bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Some(dir) = extra_bin_dir {
        entries.push(dir.to_path_buf());
    }
    entries.push(project_bin);
    entries.extend(std::env::split_paths(&path));
    let new_path = std::env::join_paths(entries).unwrap_or(path);

    let mut cmd = spawn_shell(script_cmd);
    cmd.current_dir(script_dir)
        .stderr(child_stderr())
        .env("PATH", &new_path)
        .env("npm_lifecycle_event", script_name);

    // Pass INIT_CWD the way npm/pnpm do — the directory the user
    // invoked the package manager from, *not* the script's own cwd.
    // Native-module build tooling (node-gyp, prebuild-install, etc.)
    // reads INIT_CWD to locate the project root when caching binaries.
    // Preserve if already set by a parent aube invocation so nested
    // scripts see the outermost cwd.
    if std::env::var_os("INIT_CWD").is_none() {
        cmd.env("INIT_CWD", project_root);
    }

    if let Some(ref name) = manifest.name {
        cmd.env("npm_package_name", name);
    }
    if let Some(ref version) = manifest.version {
        cmd.env("npm_package_version", version);
    }

    tracing::debug!("lifecycle: {script_name} → {script_cmd}");
    let status = cmd
        .status()
        .await
        .map_err(|e| Error::Spawn(script_name.to_string(), e.to_string()))?;

    if !status.success() {
        return Err(Error::NonZeroExit {
            script: script_name.to_string(),
            code: status.code(),
        });
    }

    Ok(())
}

/// Run a lifecycle hook against the root package, if a script for it is
/// defined. Returns `Ok(false)` if the hook wasn't defined (no-op),
/// `Ok(true)` if it ran successfully.
///
/// The caller is responsible for gating on `--ignore-scripts`.
pub async fn run_root_hook(
    project_dir: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    hook: LifecycleHook,
) -> Result<bool, Error> {
    let name = hook.script_name();
    let Some(script_cmd) = manifest.scripts.get(name) else {
        return Ok(false);
    };
    run_script(
        project_dir,
        project_dir,
        modules_dir_name,
        manifest,
        name,
        script_cmd,
        None,
    )
    .await?;
    Ok(true)
}

/// Single source of truth for the implicit `node-gyp rebuild`
/// fallback: returns `Some("node-gyp rebuild")` when the package ships
/// a `binding.gyp` at its root AND the manifest leaves both `install`
/// and `preinstall` empty (either one is the author's explicit
/// opt-out from the default).
///
/// `has_binding_gyp` is passed by the caller so this helper is
/// agnostic to *how* presence was detected — the install pipeline
/// stats the materialized package dir, while `aube ignored-builds`
/// reads the store `PackageIndex` since the package may not be
/// linked into `node_modules` yet. Both paths must agree on the gate
/// condition, so they both go through this.
pub fn implicit_install_script(
    manifest: &PackageJson,
    has_binding_gyp: bool,
) -> Option<&'static str> {
    if !has_binding_gyp {
        return None;
    }
    if manifest
        .scripts
        .contains_key(LifecycleHook::Install.script_name())
        || manifest
            .scripts
            .contains_key(LifecycleHook::PreInstall.script_name())
    {
        return None;
    }
    Some("node-gyp rebuild")
}

/// Default `install` command for a materialized dependency directory.
/// Thin wrapper around [`implicit_install_script`] that supplies
/// `has_binding_gyp` by stat'ing `<package_dir>/binding.gyp`.
pub fn default_install_script(package_dir: &Path, manifest: &PackageJson) -> Option<&'static str> {
    implicit_install_script(manifest, package_dir.join("binding.gyp").is_file())
}

/// True if [`run_dep_hook`] would actually execute something for this
/// package across any of the dependency lifecycle hooks. Callers use
/// this to skip fan-out work for packages that have nothing to run —
/// including the implicit `node-gyp rebuild` default.
pub fn has_dep_lifecycle_work(package_dir: &Path, manifest: &PackageJson) -> bool {
    if DEP_LIFECYCLE_HOOKS
        .iter()
        .any(|h| manifest.scripts.contains_key(h.script_name()))
    {
        return true;
    }
    default_install_script(package_dir, manifest).is_some()
}

/// Run a lifecycle hook against an installed dependency's package
/// directory. Mirrors [`run_root_hook`] but spawns inside `package_dir`
/// (the actual linked package directory, e.g.
/// `node_modules/.aube/<dep_path>/node_modules/<name>`). The manifest
/// is the dependency's own `package.json`, *not* the project root's.
///
/// `dep_modules_dir` is the dep's sibling `node_modules/` — i.e.
/// `package_dir`'s parent for unscoped packages, or `package_dir`'s
/// grandparent for scoped (`@scope/name`). `<dep_modules_dir>/.bin`
/// is prepended to `PATH` so the dep's postinstall can spawn tools
/// declared in its own `dependencies` (the transitive-bin case —
/// `prebuild-install`, `node-gyp`, `napi-postinstall`). The install
/// driver writes shims there via `link_dep_bins`; `rebuild` mirrors
/// the same pass.
///
/// For the `install` hook specifically, if the manifest leaves both
/// `install` and `preinstall` empty but the package has a top-level
/// `binding.gyp`, this falls back to running `node-gyp rebuild` — the
/// node-gyp default that npm and pnpm both honor so native modules
/// without a prebuilt binary still compile on install.
///
/// The caller is responsible for gating on `BuildPolicy` and
/// `--ignore-scripts`. Returns `Ok(false)` if the hook wasn't defined.
pub async fn run_dep_hook(
    package_dir: &Path,
    dep_modules_dir: &Path,
    project_root: &Path,
    modules_dir_name: &str,
    manifest: &PackageJson,
    hook: LifecycleHook,
) -> Result<bool, Error> {
    let name = hook.script_name();
    let script_cmd: &str = match manifest.scripts.get(name) {
        Some(s) => s.as_str(),
        None => match hook {
            LifecycleHook::Install => match default_install_script(package_dir, manifest) {
                Some(s) => s,
                None => return Ok(false),
            },
            _ => return Ok(false),
        },
    };
    let dep_bin_dir = dep_modules_dir.join(".bin");
    run_script(
        package_dir,
        project_root,
        modules_dir_name,
        manifest,
        name,
        script_cmd,
        Some(&dep_bin_dir),
    )
    .await?;
    Ok(true)
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to spawn script {0}: {1}")]
    Spawn(String, String),
    #[error("script `{script}` exited with code {code:?}")]
    NonZeroExit { script: String, code: Option<i32> },
}
