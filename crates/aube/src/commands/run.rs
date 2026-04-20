use super::ensure_installed;
use aube_manifest::PackageJson;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::io::IsTerminal;
use std::path::Path;

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Script name.
    ///
    /// Omit on an interactive TTY to pick from `package.json`
    /// scripts.
    pub script: Option<String>,
    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
    /// Don't error if the script is missing from package.json
    #[arg(long)]
    pub if_present: bool,
    /// Continue recursive execution after a script fails.
    ///
    /// Parsed for pnpm compatibility; aube's sequential fanout still
    /// stops on first failure.
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
    /// Run the script in every matched workspace package
    /// concurrently, with unbounded parallelism.
    ///
    /// Pair with a filter (`-r` / `-F`) — single-package runs ignore
    /// it. First non-zero exit fails the whole run, but siblings are
    /// allowed to finish so their output isn't truncated.
    #[arg(long)]
    pub parallel: bool,
    /// Write a recursive run summary file.
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
    /// Sort recursive packages topologically.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, overrides_with = "no_sort")]
    pub sort: bool,
    /// Suppress aube's wrapper output while still showing script
    /// stdout/stderr.
    ///
    /// Short alias for the global `--silent` flag; long form is
    /// intentionally omitted to avoid shadowing the global `--silent`
    /// in clap's dispatch.
    #[arg(short = 's')]
    pub silent: bool,
    /// Recursive workspace concurrency.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, value_name = "N")]
    pub workspace_concurrency: Option<usize>,
}

/// Shared args for the lifecycle shortcut commands: `start`, `stop`, `test`,
/// `restart`.
///
/// These forward to a fixed script name so the script name itself
/// is implicit — only the trailing args and `--no-install` are configurable.
#[derive(Debug, Args)]
pub struct ScriptArgs {
    /// Arguments to pass to the script
    #[arg(trailing_var_arg = true)]
    pub args: Vec<String>,
    /// Skip auto-install check
    #[arg(long)]
    pub no_install: bool,
}

pub async fn run(
    run_args: RunArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let RunArgs {
        script,
        args,
        no_install,
        no_sort: _,
        if_present,
        parallel,
        no_bail: _,
        report_summary: _,
        reporter_hide_prefix: _,
        resume_from: _,
        reverse: _,
        silent,
        sort: _,
        workspace_concurrency: _,
    } = run_args;
    let silent = silent || super::global_output_flags().silent;
    let script = match script {
        Some(s) => s,
        None => prompt_for_script()?,
    };
    run_script_with(
        &script, &args, no_install, if_present, parallel, silent, &filter,
    )
    .await
}

/// Prompt the user to pick a script from the project root's
/// `package.json`. Called by `aube run` when no script name is passed.
/// Errors without prompting if stdin is not a TTY — automation should
/// always pass a script name explicitly.
///
/// Reads the manifest as raw JSON so scripts appear in
/// `package.json` definition order (pnpm parity); `PackageJson.scripts`
/// is a `BTreeMap`, which would sort them alphabetically. We then hand
/// off to `run_script_with`, which re-reads the manifest through the
/// typed path — the extra read is one `fs::read` and only happens on
/// the interactive prompt path, so the simpler call graph is worth it.
fn prompt_for_script() -> miette::Result<String> {
    let initial_cwd = crate::dirs::cwd()?;
    let cwd = crate::dirs::find_project_root(&initial_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            initial_cwd.display()
        )
    })?;
    let scripts = read_scripts_in_order(&cwd)?;
    if scripts.is_empty() {
        return Err(miette!(
            "no scripts defined in {}",
            cwd.join("package.json").display()
        ));
    }
    if !std::io::stdin().is_terminal() {
        let names: Vec<&str> = scripts.iter().map(|(n, _)| n.as_str()).collect();
        return Err(miette!(
            "aube run: script name required when stdin is not a TTY. Available scripts: {}",
            names.join(", ")
        ));
    }
    let mut picker = demand::Select::new("Select a script to run")
        .description("package.json scripts")
        .filterable(true);
    for (name, cmd) in &scripts {
        let label = format!("{name}: {cmd}");
        picker = picker.option(demand::DemandOption::new(name.clone()).label(&label));
    }
    match picker.run() {
        Ok(name) => Ok(name),
        // Ctrl-C / Esc cancels the prompt — exit silently with the
        // conventional SIGINT code rather than printing a miette error
        // for what was a deliberate user action.
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => std::process::exit(130),
        Err(e) => Err(e)
            .into_diagnostic()
            .wrap_err("failed to read script selection"),
    }
}

/// Read `package.json` and return its `scripts` entries in the order
/// they appear in the file. Relies on the workspace's
/// `serde_json/preserve_order` feature — `serde_json::Value`'s object
/// variant is an `IndexMap` there, so object iteration preserves
/// insertion order. Entries with non-string values (invalid per npm
/// but we don't want to choke on them here) are skipped.
fn read_scripts_in_order(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let path = cwd.join("package.json");
    let bytes = std::fs::read(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    let Some(serde_json::Value::Object(obj)) = value.get("scripts") else {
        return Ok(Vec::new());
    };
    Ok(obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect())
}

pub(crate) async fn run_script(
    script: &str,
    args: &[String],
    no_install: bool,
    if_present: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let silent = super::global_output_flags().silent;
    run_script_with(script, args, no_install, if_present, false, silent, filter).await
}

pub(crate) async fn run_script_with(
    script: &str,
    args: &[String],
    no_install: bool,
    if_present: bool,
    parallel: bool,
    silent: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let initial_cwd = crate::dirs::cwd()?;
    // Walk upward to the nearest `package.json` so `aube run` from a
    // subdirectory picks up the project root's scripts, matching pnpm.
    let cwd = crate::dirs::find_project_root(&initial_cwd).ok_or_else(|| {
        miette!(
            "no package.json found in {} or any parent directory",
            initial_cwd.display()
        )
    })?;
    let enable_pre_post_scripts = configure_script_settings_for_project(&cwd)?;

    if !filter.is_empty() {
        return run_script_filtered(
            &cwd,
            script,
            args,
            no_install,
            if_present,
            parallel,
            silent,
            filter,
            enable_pre_post_scripts,
        )
        .await;
    }

    let manifest = load_manifest(&cwd)?;
    if !manifest.scripts.contains_key(script) {
        if if_present {
            return Ok(());
        }
        return Err(miette!("script not found: {script}"));
    }

    ensure_installed(no_install).await?;
    exec_script_chain(&cwd, &manifest, script, args, enable_pre_post_scripts).await
}

/// Fan out a script over workspace packages matched by `filter`. Runs
/// sequentially — packages are visited in directory-discovery order, each
/// gets its own `ensure_installed` check, and the first non-zero exit
/// aborts the fanout. Parallel execution is a deliberate follow-up: it
/// requires output multiplexing that collides with the progress UI.
#[allow(clippy::too_many_arguments)]
async fn run_script_filtered(
    cwd: &Path,
    script: &str,
    args: &[String],
    no_install: bool,
    if_present: bool,
    parallel: bool,
    silent: bool,
    filter: &aube_workspace::selector::EffectiveFilter,
    enable_pre_post_scripts: bool,
) -> miette::Result<()> {
    let workspace_pkgs = aube_workspace::find_workspace_packages(cwd)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "aube run: --filter requires a pnpm-workspace.yaml at {}",
            cwd.display()
        ));
    }

    let matched = aube_workspace::selector::select_workspace_packages(cwd, &workspace_pkgs, filter)
        .map_err(|e| miette!("invalid --filter selector: {e}"))?;

    if matched.is_empty() {
        return Err(miette!(
            "aube run: filter {filter:?} did not match any workspace package"
        ));
    }

    // Install once at the workspace root before fanning out — the
    // isolated linker already materializes every workspace package's
    // deps in a single pass, so per-package reinstalls would just
    // re-check the same lockfile N times.
    ensure_installed(no_install).await?;

    if parallel {
        // Unbounded parallel fanout. Spawn every matched package at
        // once and let them all finish so the slowest task's output
        // isn't clipped by an earlier failure. Use `exec_script_status`
        // — `exec_script` calls `std::process::exit` on a non-zero
        // exit, which would kill sibling tasks mid-run and make the
        // collection loop below unreachable.
        //
        // Validate every package *before* spawning anything: an
        // `Err` return after some handles already exist would drop
        // them, and tokio does not cancel detached tasks — the
        // orphaned shell children (`sh -c` on Unix, `cmd.exe /D /S
        // /C` on Windows) would keep running until
        // `std::process::exit` tore them down, which can corrupt
        // partial artifacts mid-write.
        let runnable: Vec<_> = matched
            .into_iter()
            .filter_map(|pkg| {
                if pkg.manifest.scripts.contains_key(script) {
                    Some(Ok(pkg))
                } else if if_present {
                    None
                } else {
                    let name = pkg
                        .name
                        .clone()
                        .unwrap_or_else(|| pkg.dir.display().to_string());
                    Some(Err(miette!(
                        "aube run: package {name} has no `{script}` script"
                    )))
                }
            })
            .collect::<miette::Result<Vec<_>>>()?;

        let mut tasks: Vec<tokio::task::JoinHandle<miette::Result<std::process::ExitStatus>>> =
            Vec::with_capacity(runnable.len());
        let mut task_names: Vec<String> = Vec::with_capacity(runnable.len());
        for pkg in runnable {
            let name = pkg
                .name
                .clone()
                .unwrap_or_else(|| pkg.dir.display().to_string());
            if !silent {
                tracing::info!("aube run: {name} -> {script} (parallel)");
            }
            let script = script.to_string();
            let args = args.to_vec();
            let dir = pkg.dir.clone();
            let manifest = pkg.manifest.clone();
            task_names.push(name);
            tasks.push(tokio::spawn(async move {
                exec_script_status_chain(&dir, &manifest, &script, &args, enable_pre_post_scripts)
                    .await
            }));
        }
        let mut first_err: Option<miette::Report> = None;
        let mut first_exit: Option<i32> = None;
        for (t, name) in tasks.into_iter().zip(task_names) {
            match t.await {
                Ok(Ok(status)) => {
                    if !status.success() && first_exit.is_none() {
                        first_exit = Some(status.code().unwrap_or(1));
                        first_err = Some(miette!(
                            "aube run: `{script}` failed in {name} (exit {})",
                            status.code().unwrap_or(1)
                        ));
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

    for pkg in &matched {
        let name = pkg
            .name
            .as_deref()
            .unwrap_or_else(|| pkg.dir.to_str().unwrap_or("(unnamed)"));
        if !pkg.manifest.scripts.contains_key(script) {
            if if_present {
                continue;
            }
            return Err(miette!("aube run: package {name} has no `{script}` script"));
        }
        if !silent {
            tracing::info!("aube run: {name} -> {script}");
        }
        exec_script_chain(
            &pkg.dir,
            &pkg.manifest,
            script,
            args,
            enable_pre_post_scripts,
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn load_manifest(cwd: &Path) -> miette::Result<PackageJson> {
    PackageJson::from_path(&cwd.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")
}

fn configure_script_settings_for_project(cwd: &Path) -> miette::Result<bool> {
    let npmrc_entries = aube_registry::config::load_npmrc_entries(cwd);
    let (_, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env_snapshot = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env: &env_snapshot,
        cli: &[],
    };
    let enable_pre_post_scripts = aube_settings::resolved::enable_pre_post_scripts(&ctx);
    super::configure_script_settings(&ctx);
    Ok(enable_pre_post_scripts)
}

/// Run `script` if it exists in `manifest.scripts`. Returns `true` if the
/// script was found and executed, `false` if it was missing. Errors only
/// when the script ran but exited non-zero (via `exit`).
pub(crate) async fn exec_optional(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
) -> miette::Result<bool> {
    if !manifest.scripts.contains_key(script) {
        return Ok(false);
    }
    exec_script(cwd, manifest, script, args).await?;
    Ok(true)
}

pub(crate) async fn exec_script(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
) -> miette::Result<()> {
    let cmd = manifest
        .scripts
        .get(script)
        .ok_or_else(|| miette!("script not found: {script}"))?;

    let shell_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };

    let bin_dir = super::project_modules_dir(cwd).join(".bin");
    let new_path = aube_scripts::prepend_path(&bin_dir);

    let status = aube_scripts::spawn_shell(&shell_cmd)
        .env("PATH", &new_path)
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr())
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute script")?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

pub(crate) async fn exec_script_chain(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    enable_pre_post_scripts: bool,
) -> miette::Result<()> {
    if enable_pre_post_scripts {
        let pre = format!("pre{script}");
        exec_optional(cwd, manifest, &pre, &[]).await?;
    }
    exec_script(cwd, manifest, script, args).await?;
    if enable_pre_post_scripts {
        let post = format!("post{script}");
        exec_optional(cwd, manifest, &post, &[]).await?;
    }
    Ok(())
}

/// Same shell as `exec_script` but returns the `ExitStatus` instead of
/// `std::process::exit`-ing on failure. Used by the `--parallel` path,
/// which needs to collect every task's outcome before deciding how to
/// terminate.
pub(crate) async fn exec_script_status(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
) -> miette::Result<std::process::ExitStatus> {
    let cmd = manifest
        .scripts
        .get(script)
        .ok_or_else(|| miette!("script not found: {script}"))?;
    let shell_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{cmd} {}", args.join(" "))
    };
    let bin_dir = super::project_modules_dir(cwd).join(".bin");
    let new_path = aube_scripts::prepend_path(&bin_dir);
    aube_scripts::spawn_shell(&shell_cmd)
        .env("PATH", &new_path)
        .current_dir(cwd)
        .stderr(aube_scripts::child_stderr())
        .status()
        .await
        .into_diagnostic()
        .wrap_err("failed to execute script")
}

pub(crate) async fn exec_script_status_chain(
    cwd: &Path,
    manifest: &PackageJson,
    script: &str,
    args: &[String],
    enable_pre_post_scripts: bool,
) -> miette::Result<std::process::ExitStatus> {
    if enable_pre_post_scripts {
        let pre = format!("pre{script}");
        if manifest.scripts.contains_key(&pre) {
            let status = exec_script_status(cwd, manifest, &pre, &[]).await?;
            if !status.success() {
                return Ok(status);
            }
        }
    }
    let status = exec_script_status(cwd, manifest, script, args).await?;
    if !status.success() {
        return Ok(status);
    }
    if enable_pre_post_scripts {
        let post = format!("post{script}");
        if manifest.scripts.contains_key(&post) {
            return exec_script_status(cwd, manifest, &post, &[]).await;
        }
    }
    Ok(status)
}
