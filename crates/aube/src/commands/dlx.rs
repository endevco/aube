use super::install::{FrozenMode, InstallOptions};
use clap::{Args, CommandFactory};
use miette::{Context, IntoDiagnostic, miette};

#[derive(Debug, Args)]
// dlx forwards everything after `<command>` to the bin it runs, including
// `--help` and `--version`. Let clap auto-inject its own `-h`/`--help` and
// `--version` handlers and they'd silently swallow those flags before they
// reach the binary — users would see aube's help screen instead of the
// tool's. Disable clap's built-in flags on this subcommand.
//
// `aube dlx --help` on its own (no command) still prints aube's dlx help:
// `params` is optional and the handler intercepts a leading `--help` /
// `-h` before treating anything as a command.
#[command(disable_help_flag = true)]
pub struct DlxArgs {
    /// Command (binary) to run, followed by arguments to pass through to
    /// it.
    ///
    /// The first positional is the command; the rest are forwarded
    /// verbatim to the installed binary. Under `--shell-mode`/`-c` the
    /// positionals are joined and evaluated by `sh -c` instead of
    /// looked up in `node_modules/.bin`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub params: Vec<String>,
    /// Run the assembled command line through `sh -c` with
    /// `<scratch>/node_modules/.bin` prepended to `PATH`.
    ///
    /// Use this for pipelines, redirects, or env expansion (`aube dlx
    /// -p cowsay -c 'cowsay hello | tr a-z A-Z'`). Mirrors `pnpm dlx
    /// --shell-mode`.
    #[arg(short = 'c', long)]
    pub shell_mode: bool,
    /// Install a specific package (repeatable).
    ///
    /// Overrides inferring from the command.
    #[arg(short = 'p', long = "package")]
    pub package: Vec<String>,
}

/// `aube dlx [-p <pkg>]... <command> [args...]`
///
/// Install one or more packages into a throwaway project and run a binary
/// from them. Matches pnpm's `pnpm dlx` / npm's `npx` surface.
///
/// Flow:
///   1. Create a fresh tempfile::TempDir project with a minimal package.json.
///   2. Run the normal install pipeline there under a CwdGuard that restores
///      the original cwd on drop — including the panic path — so a crash
///      inside install::run can't leave the process with its cwd pointed at
///      an already-removed scratch dir.
///   3. Exec `<tmp>/node_modules/.bin/<command>` from the user's original cwd.
///   4. tempfile removes the scratch dir on drop.
pub async fn run(args: DlxArgs) -> miette::Result<()> {
    let DlxArgs {
        params,
        package,
        shell_mode,
    } = args;

    // Bare `aube dlx` or `aube dlx --help` / `-h` prints aube's dlx help.
    // Once a command is present, any further flags (including `--help`)
    // belong to the installed binary.
    let first = params.first().map(String::as_str);
    if matches!(first, None | Some("--help" | "-h")) && package.is_empty() {
        crate::Cli::command()
            .find_subcommand_mut("dlx")
            .expect("dlx is a registered subcommand")
            .print_help()
            .map_err(|e| miette!("failed to render help: {e}"))?;
        println!();
        return Ok(());
    }

    // When only `-p` is given, dlx needs at least one arg to serve as the
    // bin name; without it, we don't know which binary to exec.
    let command = params
        .first()
        .cloned()
        .ok_or_else(|| miette!("dlx: missing command to run"))?;
    let bin_args: Vec<String> = params.iter().skip(1).cloned().collect();

    // Remember whether `-p` was given. With `-p` the user has named the
    // bin explicitly (`aube dlx -p which node-which`), so we run their
    // command verbatim. Without `-p` the command doubles as the package
    // name and we may need to cross-reference the installed package's
    // `bin` map — e.g. `@tanstack/cli` ships its bin under the name
    // `tanstack`, not `cli`.
    let explicit_package = !package.is_empty();

    // Derive the packages to install. `-p` wins; otherwise the command name
    // is the package name (the common `pnpm dlx <pkg>` case). Under
    // `--shell-mode` the first positional is a shell line, not a bin name,
    // so we fall back to the first whitespace-separated word for inference
    // when `-p` wasn't given — same as pnpm.
    let install_specs: Vec<String> = if package.is_empty() {
        if shell_mode {
            let first_word = command
                .split_whitespace()
                .next()
                .ok_or_else(|| miette!("dlx --shell-mode: missing command line to run"))?;
            vec![first_word.to_string()]
        } else {
            vec![command.clone()]
        }
    } else {
        package
    };

    // Bin name is only used in the non-shell path. Under shell-mode the
    // user assembles their own line and we run it through `sh -c`, so any
    // bin lookup is the shell's job.
    let bin_name = bin_name_for(&command);

    let tmp = tempfile::Builder::new()
        .prefix("aube-dlx-")
        .tempdir()
        .into_diagnostic()
        .wrap_err("failed to create dlx scratch dir")?;
    let project_dir = tmp.path().to_path_buf();

    // Minimal package.json. Version specs and dist-tags pass through as-is
    // — the resolver handles them exactly as it would from a real manifest.
    let deps: serde_json::Map<String, serde_json::Value> = install_specs
        .iter()
        .map(|spec| {
            let (name, version) = split_spec(spec);
            (
                name.to_string(),
                serde_json::Value::String(version.to_string()),
            )
        })
        .collect();
    let manifest = serde_json::json!({
        "name": "aube-dlx",
        "version": "0.0.0",
        "private": true,
        "dependencies": deps,
    });
    std::fs::write(
        project_dir.join("package.json"),
        serde_json::to_string_pretty(&manifest).into_diagnostic()?,
    )
    .into_diagnostic()
    .wrap_err("failed to write dlx package.json")?;

    // install::run pulls its project dir from std::env::current_dir(), which
    // is process-global state. The CwdGuard below captures the current dir,
    // switches into the scratch project for the duration of the install, and
    // restores the original on drop — so the exec path below, any error
    // diagnostic rendering, and even a panic unwinding past this frame all
    // observe the user's real cwd instead of a dir that's about to vanish.
    let prev_cwd = {
        let _cwd_guard = CwdGuard::switch_to(&project_dir)?;
        let install_result = super::install::run(dlx_install_options()).await;
        let prev = _cwd_guard.original.clone();
        install_result.wrap_err("dlx install failed")?;
        prev
        // _cwd_guard drops here, restoring cwd.
    };

    // Run from the user's original cwd so the invoked tool sees their
    // project, not the scratch dir — this matches pnpm dlx.
    //
    // Under `--shell-mode` we evaluate the joined positionals via `sh -c`
    // with the scratch project's `node_modules/.bin` prepended to PATH,
    // so pipelines/redirects work and the freshly installed bin
    // resolves first. Otherwise we exec the bin directly so its argv
    // round-trips bit-for-bit.
    let status = if shell_mode {
        // In shell-mode the positionals are literal shell-line fragments
        // that the caller explicitly wants sh to interpret (pipes,
        // redirects, subshells). Matches pnpm dlx --shell-mode and is
        // why this call does not shell-quote like aube exec --shell-mode
        // does. Join with space preserves that contract.
        let line = std::iter::once(command.as_str())
            .chain(bin_args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        // `dlx` installs into a scratch tempdir, which honors `modulesDir`
        // from the user's `~/.npmrc` / `aube-workspace.yaml` if it's set
        // globally. Read the same setting here so the scratch bin dir
        // matches where the install actually wrote the bins.
        let bin_dir = super::project_modules_dir(&project_dir).join(".bin");
        let new_path = aube_scripts::prepend_path(&bin_dir);
        let mut cmd = aube_scripts::spawn_shell(&line);
        cmd.env("PATH", &new_path)
            .current_dir(&prev_cwd)
            .stderr(aube_scripts::child_stderr())
            .status()
            .await
            .into_diagnostic()
            .wrap_err("failed to execute dlx shell command")?
    } else {
        let modules_dir = super::project_modules_dir(&project_dir);
        let bin_dir = modules_dir.join(".bin");
        // If `-p` wasn't given, the command doubles as the package name
        // and the bin is a best-guess derivation from it. Check the
        // installed package's `bin` field and prefer the actual bin name
        // it ships — e.g. `@tanstack/cli` ships `tanstack`, not `cli`.
        let resolved_bin_name = if !explicit_package && !bin_dir.join(&bin_name).exists() {
            resolve_bin_from_package(&modules_dir, &install_specs[0])
                .unwrap_or_else(|| bin_name.clone())
        } else {
            bin_name.clone()
        };
        let bin_path = bin_dir.join(&resolved_bin_name);
        if !bin_path.exists() {
            return Err(miette!(
                "dlx: binary not found after install: {resolved_bin_name}\n\
                 help: the package may ship the binary under a different name — try `aube dlx -p <package> <bin>`"
            ));
        }
        tokio::process::Command::new(&bin_path)
            .args(&bin_args)
            .current_dir(&prev_cwd)
            .stderr(aube_scripts::child_stderr())
            .status()
            .await
            .into_diagnostic()
            .wrap_err("failed to execute dlx binary")?
    };

    // tmp drops here, removing the scratch project.
    drop(tmp);

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn dlx_install_options() -> InstallOptions {
    let mut opts = InstallOptions::with_mode(FrozenMode::No);
    // `dlx` executes bins from a throwaway project and deletes that project
    // immediately. Keeping package materialization inside the scratch tree is
    // what lets Node walk through `node_modules/.aube/node_modules`, the
    // hidden hoist fallback used by CLIs with undeclared runtime imports.
    let gvs = super::global_virtual_store_flags();
    if gvs.is_set() {
        opts.cli_flags.extend(gvs.to_cli_flag_bag());
    } else {
        opts.cli_flags.push((
            "disable-global-virtual-store".to_string(),
            "false".to_string(),
        ));
    }
    opts
}

/// RAII guard that swaps the process cwd on construction and restores it
/// on drop — including when the enclosing scope unwinds due to a panic.
struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn switch_to(new_dir: &std::path::Path) -> miette::Result<Self> {
        let original = std::env::current_dir().into_diagnostic()?;
        std::env::set_current_dir(new_dir)
            .into_diagnostic()
            .wrap_err("failed to switch into dlx scratch dir")?;
        Ok(Self { original })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        // Best-effort: if restoring the cwd fails we can't meaningfully
        // recover, but we also don't want to double-panic from Drop.
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// Strip any `@version` suffix from a package spec, preserving `@scope/`
/// prefixes. Dlx defaults to the `latest` dist-tag when no version is given,
/// so the spec arm of `split_name_spec` becomes `"latest"` here.
fn split_spec(spec: &str) -> (&str, &str) {
    let (name, version) = super::split_name_spec(spec);
    (name, version.unwrap_or("latest"))
}

/// The binary name `aube dlx <cmd>` should resolve to — strip any version
/// suffix and any `@scope/` prefix, since `node_modules/.bin/` is flat and
/// scoped packages still land under their unscoped bin name.
fn bin_name_for(command: &str) -> String {
    let (name, _) = split_spec(command);
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// When the bin derived from the package name doesn't match the installed
/// package's actual bin, fall back to reading the package's `bin` field to
/// find the right name. Matches `npx`/`pnpm dlx` behavior so e.g.
/// `aube dlx @tanstack/cli create` works (ships its bin as `tanstack`, not
/// `cli`) and `aube dlx which` works (ships `node-which`).
///
/// `modules_dir` is the project's resolved virtual-modules directory — the
/// same one we derive the `.bin` path from, so a user with a custom
/// `modulesDir` still sees the fallback work. Returns `None` when we can't
/// make a confident pick; caller keeps the original inference and lets the
/// bin-missing error fire.
fn resolve_bin_from_package(modules_dir: &std::path::Path, install_spec: &str) -> Option<String> {
    let (pkg_name, _) = split_spec(install_spec);
    let pkg_json_path = modules_dir.join(pkg_name).join("package.json");
    let content = std::fs::read_to_string(&pkg_json_path).ok()?;
    let pkg_json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let bin = pkg_json.get("bin")?;
    let inferred = pkg_name.rsplit('/').next().unwrap_or(pkg_name);
    match bin {
        // String bin: npm always names it after the unscoped package name,
        // so this matches what `bin_name_for` already derived. Returning
        // it explicitly keeps the lookup path symmetric.
        serde_json::Value::String(_) => Some(inferred.to_string()),
        serde_json::Value::Object(bins) => {
            if bins.contains_key(inferred) {
                Some(inferred.to_string())
            } else if bins.len() == 1 {
                // Single bin under a different name — unambiguous pick.
                bins.keys().next().cloned()
            } else {
                // Multiple bins, none matching the package name. We don't
                // know which one the user wants; let them pick via `-p`.
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_spec_plain() {
        assert_eq!(split_spec("cowsay"), ("cowsay", "latest"));
    }

    #[test]
    fn split_spec_versioned() {
        assert_eq!(split_spec("cowsay@1.5.0"), ("cowsay", "1.5.0"));
    }

    #[test]
    fn split_spec_scoped() {
        assert_eq!(split_spec("@scope/foo"), ("@scope/foo", "latest"));
    }

    #[test]
    fn split_spec_scoped_versioned() {
        assert_eq!(split_spec("@scope/foo@2.0.0"), ("@scope/foo", "2.0.0"));
    }

    #[test]
    fn bin_name_strips_scope_and_version() {
        assert_eq!(bin_name_for("cowsay@1.5.0"), "cowsay");
        assert_eq!(bin_name_for("@scope/foo@2"), "foo");
        assert_eq!(bin_name_for("@scope/foo"), "foo");
    }

    #[test]
    fn dlx_install_disables_global_virtual_store() {
        let opts = dlx_install_options();
        let empty_npmrc = Vec::new();
        let empty_workspace = std::collections::BTreeMap::new();
        let empty_env = Vec::new();
        let ctx = aube_settings::ResolveCtx {
            npmrc: &empty_npmrc,
            workspace_yaml: &empty_workspace,
            env: &empty_env,
            cli: &opts.cli_flags,
        };
        assert_eq!(
            aube_settings::resolved::enable_global_virtual_store(&ctx),
            Some(false)
        );
    }

    fn write_pkg_json(modules_dir: &std::path::Path, pkg_name: &str, pkg_json: serde_json::Value) {
        let dir = modules_dir.join(pkg_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&pkg_json).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn resolve_bin_single_object_bin_picks_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "@tanstack/cli",
            serde_json::json!({
                "name": "@tanstack/cli",
                "bin": {"tanstack": "dist/bin.js"},
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "@tanstack/cli@latest"),
            Some("tanstack".to_string())
        );
    }

    #[test]
    fn resolve_bin_object_with_matching_key_prefers_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "foo",
            serde_json::json!({
                "name": "foo",
                "bin": {"foo": "x.js", "foo-helper": "y.js"},
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "foo"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn resolve_bin_object_multiple_no_match_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "foo",
            serde_json::json!({
                "name": "foo",
                "bin": {"a": "a.js", "b": "b.js"},
            }),
        );
        assert_eq!(resolve_bin_from_package(tmp.path(), "foo"), None);
    }

    #[test]
    fn resolve_bin_string_bin_returns_package_tail() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(
            tmp.path(),
            "@scope/foo",
            serde_json::json!({
                "name": "@scope/foo",
                "bin": "./x.js",
            }),
        );
        assert_eq!(
            resolve_bin_from_package(tmp.path(), "@scope/foo"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn resolve_bin_no_bin_field_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_pkg_json(tmp.path(), "foo", serde_json::json!({"name": "foo"}));
        assert_eq!(resolve_bin_from_package(tmp.path(), "foo"), None);
    }
}
