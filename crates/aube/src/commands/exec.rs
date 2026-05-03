use super::ensure_installed;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::path::Path;

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Binary name
    pub bin: String,
    /// Arguments to pass to the binary
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Continue recursive execution after a command fails.
    ///
    /// Parsed for pnpm compatibility; aube currently stops on the
    /// first failure.
    #[arg(long)]
    pub no_bail: bool,
    /// Skip auto-install check
    #[arg(long)]
    pub no_install: bool,
    /// Disable topological sorting.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, overrides_with = "sort")]
    pub no_sort: bool,
    /// Run recursive workspace executions concurrently.
    #[arg(long)]
    pub parallel: bool,
    /// Write a recursive exec summary file.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub report_summary: bool,
    /// Hide package prefixes in recursive reporter output.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub reporter_hide_prefix: bool,
    /// Resume recursive execution from a package name.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, value_name = "PACKAGE")]
    pub resume_from: Option<String>,
    /// Run recursive packages in reverse order.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub reverse: bool,
    /// Run the command through `sh -c`.
    #[arg(short = 'c', long)]
    pub shell_mode: bool,
    /// Sort recursive packages topologically.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, overrides_with = "no_sort")]
    pub sort: bool,
    /// Recursive workspace concurrency.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, value_name = "N")]
    pub workspace_concurrency: Option<usize>,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    exec_args: ExecArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    exec_args.network.install_overrides();
    exec_args.lockfile.install_overrides();
    exec_args.virtual_store.install_overrides();
    let ExecArgs {
        bin,
        args,
        no_install,
        parallel,
        no_bail: _,
        no_sort: _,
        report_summary: _,
        reporter_hide_prefix: _,
        resume_from: _,
        reverse: _,
        shell_mode,
        sort: _,
        workspace_concurrency: _,
        lockfile: _,
        network: _,
        virtual_store: _,
    } = exec_args;
    let cwd = crate::dirs::project_root()?;

    ensure_installed(no_install).await?;

    if !filter.is_empty() {
        return run_filtered(&cwd, &bin, &args, shell_mode, parallel, &filter).await;
    }

    let bin_path = super::project_modules_dir(&cwd).join(".bin").join(&bin);
    exec_bin(&cwd, &bin_path, &bin, &args, shell_mode).await
}

async fn run_filtered(
    cwd: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
    parallel: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let (_root, matched) = super::select_workspace_packages(cwd, filter, "exec")?;
    if parallel {
        if !shell_mode {
            for pkg in &matched {
                let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
                if !bin_path.exists() {
                    let name = pkg
                        .name
                        .as_deref()
                        .unwrap_or_else(|| pkg.dir.to_str().unwrap_or("<unknown>"));
                    return Err(miette!(
                        "binary not found in {name}: {bin}\nTry running `aube install` first, or check that the package providing '{bin}' is in its dependencies."
                    ));
                }
            }
        }

        let mut tasks: Vec<tokio::task::JoinHandle<miette::Result<std::process::ExitStatus>>> =
            Vec::with_capacity(matched.len());
        let mut task_names = Vec::with_capacity(matched.len());
        for pkg in matched {
            let name = pkg
                .name
                .clone()
                .unwrap_or_else(|| pkg.dir.display().to_string());
            let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
            let dir = pkg.dir.clone();
            let bin = bin.to_string();
            let args = args.to_vec();
            task_names.push(name);
            tasks.push(tokio::spawn(async move {
                exec_bin_status(&dir, &bin_path, &bin, &args, shell_mode).await
            }));
        }

        let mut first_err: Option<miette::Report> = None;
        let mut first_exit: Option<i32> = None;
        for (task, name) in tasks.into_iter().zip(task_names) {
            match task.await {
                Ok(Ok(status)) => {
                    if !status.success() && first_exit.is_none() {
                        let code = aube_scripts::exit_code_from_status(status);
                        first_exit = Some(code);
                        first_err =
                            Some(miette!("aube exec: `{bin}` failed in {name} (exit {code})"));
                    }
                }
                Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                Ok(Err(_)) => {}
                Err(e) if first_err.is_none() => first_err = Some(miette!("task panicked: {e}")),
                Err(_) => {}
            }
        }
        if let Some(code) = first_exit {
            std::process::exit(code);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        return Ok(());
    }

    for pkg in matched {
        let bin_path = super::project_modules_dir(&pkg.dir).join(".bin").join(bin);
        exec_bin(&pkg.dir, &bin_path, bin, args, shell_mode).await?;
    }
    Ok(())
}

pub(crate) async fn exec_bin(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
) -> miette::Result<()> {
    if !shell_mode && !bin_path.exists() {
        return Err(miette!(
            "binary not found: {bin}\nTry running `aube install` first, or check that the package providing '{bin}' is in your dependencies."
        ));
    }

    let mut command = if shell_mode {
        let line = std::iter::once(aube_scripts::shell_quote_arg(bin))
            .chain(args.iter().map(|arg| aube_scripts::shell_quote_arg(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        let bin_dir = super::project_modules_dir(cwd).join(".bin");
        let new_path = aube_scripts::prepend_path(&bin_dir);
        let mut cmd = aube_scripts::spawn_shell(&line);
        cmd.env("PATH", &new_path);
        cmd
    } else {
        let exec_path = resolve_exec_shim(bin_path);
        let mut cmd = tokio::process::Command::new(exec_path);
        cmd.args(args);
        cmd
    };
    let status = command
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr())
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute binary")?;

    if !status.success() {
        std::process::exit(aube_scripts::exit_code_from_status(status));
    }

    Ok(())
}

pub(crate) async fn exec_bin_status(
    cwd: &Path,
    bin_path: &Path,
    bin: &str,
    args: &[String],
    shell_mode: bool,
) -> miette::Result<std::process::ExitStatus> {
    if !shell_mode && !bin_path.exists() {
        return Err(miette!(
            "binary not found: {bin}\nTry running `aube install` first, or check that the package providing '{bin}' is in your dependencies."
        ));
    }

    let mut command = if shell_mode {
        let line = std::iter::once(aube_scripts::shell_quote_arg(bin))
            .chain(args.iter().map(|arg| aube_scripts::shell_quote_arg(arg)))
            .collect::<Vec<_>>()
            .join(" ");
        let bin_dir = super::project_modules_dir(cwd).join(".bin");
        let new_path = aube_scripts::prepend_path(&bin_dir);
        let mut cmd = aube_scripts::spawn_shell(&line);
        cmd.env("PATH", &new_path);
        cmd
    } else {
        let exec_path = resolve_exec_shim(bin_path);
        let mut cmd = tokio::process::Command::new(exec_path);
        cmd.args(args);
        cmd
    };
    command
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr())
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute binary")
}

/// Pick the executable variant of a `node_modules/.bin/<name>` shim.
///
/// On Unix the bare path is a sh shebang script and is what we want.
/// On Windows the linker writes `<name>.cmd`, `<name>.ps1`, and a bare
/// `<name>` sh shim. `Command::new` can launch the `.cmd` shim, but the
/// bare sh shim fails with OS error 193.
pub(crate) fn resolve_exec_shim(bin_path: &Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if bin_path.extension().is_none() {
            let cmd_path = bin_path.with_extension("cmd");
            if cmd_path.exists() {
                return cmd_path;
            }
        }
    }
    bin_path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::resolve_exec_shim;

    #[test]
    fn resolve_exec_shim_returns_bare_path_when_no_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("loner");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), bare);
    }

    #[cfg(windows)]
    #[test]
    fn resolve_exec_shim_prefers_cmd_sibling_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("cowsay");
        let cmd_shim = tmp.path().join("cowsay.cmd");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        std::fs::write(&cmd_shim, b"@echo off\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), cmd_shim);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_exec_shim_keeps_bare_path_on_unix() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("cowsay");
        let cmd_shim = tmp.path().join("cowsay.cmd");
        std::fs::write(&bare, b"#!/bin/sh\n").unwrap();
        std::fs::write(&cmd_shim, b"@echo off\n").unwrap();
        assert_eq!(resolve_exec_shim(&bare), bare);
    }
}
