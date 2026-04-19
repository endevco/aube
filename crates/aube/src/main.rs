mod commands;
mod dirs;
mod engines;
mod patches;
mod pnpmfile;
mod progress;
mod state;
mod update_check;

use clap::{Parser, Subcommand, ValueEnum};
use miette::{Context, IntoDiagnostic, miette};
use std::ffi::OsString;
use std::path::PathBuf;
use tracing_subscriber::prelude::*;

/// Inspect `argv[0]` and, when invoked as a multicall shim (`aubr`, `aubx`),
/// rewrite the argv so clap sees the equivalent `aube run …` / `aube dlx …`.
/// Shims are installed as hardlinks (or copies on Windows) that point at the
/// same `aube` executable; dispatch happens purely at runtime via basename.
fn rewrite_multicall_argv(mut args: Vec<OsString>) -> Vec<OsString> {
    let Some(argv0) = args.first() else {
        return args;
    };
    let stem = std::path::Path::new(argv0)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("aube")
        .to_ascii_lowercase();
    let subcommand = match stem.as_str() {
        "aubr" => "run",
        "aubx" => "dlx",
        _ => return args,
    };
    args[0] = OsString::from("aube");
    args.insert(1, OsString::from(subcommand));
    args
}

#[derive(Parser)]
#[command(name = "aube", about = "A fast Node.js package manager", version)]
pub(crate) struct Cli {
    /// Change to directory before running (like `make -C` or `mise --cd`)
    #[arg(short = 'C', long = "dir", visible_aliases = ["cd", "prefix"], global = true, value_name = "DIR")]
    dir: Option<std::path::PathBuf>,

    /// Scope command execution to workspace packages matching PATTERN.
    ///
    /// Supports exact names (`my-pkg`), globs (`@scope/*`, `*-plugin`),
    /// paths (`./packages/api`), graph selectors (`pkg...`, `...pkg`),
    /// git-ref selectors (`[origin/main]`), and exclusions (`!pkg`).
    /// Repeatable; matches are OR-ed.
    ///
    /// Currently honored by `run`, `test`, `start`, `stop`, `restart`,
    /// `install`, `exec`, `list`, `publish`, `deploy`, `add`, `remove`,
    /// `update`, `why`, and implicit-script invocations.
    #[arg(short = 'F', long, global = true, value_name = "PATTERN")]
    filter: Vec<String>,

    /// Run the command across every workspace package.
    ///
    /// Equivalent to `--filter=*`; if `--filter` is also given,
    /// `--recursive` is a no-op and the explicit filter wins. Honored
    /// by the same commands as `--filter`.
    #[arg(short = 'r', long, global = true)]
    recursive: bool,

    /// Enable verbose/debug logging (shortcut for `--loglevel debug`)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Group workspace command output after each package finishes.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential, so output is already grouped.
    #[arg(long, global = true, conflicts_with = "stream")]
    aggregate_output: bool,

    /// Force colored output even when stderr is not a TTY.
    ///
    /// Overrides `NO_COLOR` / `CLICOLOR=0`. Mutually exclusive with
    /// `--no-color`.
    #[arg(long, global = true, conflicts_with = "no_color")]
    color: bool,

    /// Error when a workspace selector matches no packages.
    ///
    /// Accepted globally; selected commands already fail on empty matches.
    #[arg(long, global = true)]
    fail_if_no_match: bool,

    /// Production-only variant of `--filter`.
    ///
    /// Same selector grammar as `--filter`, but graph walks (`pkg...`,
    /// `...pkg`) only follow `dependencies` / `optionalDependencies` /
    /// `peerDependencies` edges — `devDependencies` (and packages
    /// reachable solely through them) are skipped. Non-graph forms
    /// (exact name, glob, path, `[git-ref]`) behave identically to
    /// `--filter`. Repeatable; can be combined with `--filter`.
    #[arg(long, global = true, value_name = "PATTERN")]
    filter_prod: Vec<String>,

    /// Error if the lockfile drifts from package.json.
    ///
    /// Accepted on every command for pnpm parity; aube commands that
    /// trigger an install (directly or via auto-install) pick this up
    /// through the process-wide flag snapshot.
    #[arg(long, global = true, conflicts_with_all = ["no_frozen_lockfile", "prefer_frozen_lockfile"])]
    frozen_lockfile: bool,

    /// Ignore workspace discovery for commands that support workspace fanout.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true)]
    ignore_workspace: bool,

    /// Include the workspace root in recursive workspace operations.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(long, global = true)]
    include_workspace_root: bool,

    /// Set the log level. Logs at or above this level are shown.
    #[arg(long, global = true, value_name = "LEVEL", value_enum)]
    loglevel: Option<LogLevel>,

    /// Disable colored output.
    ///
    /// Overrides `FORCE_COLOR` / `CLICOLOR_FORCE` and sets `NO_COLOR=1`
    /// so downstream libraries (miette, clx, child processes) all see
    /// the same choice.
    #[arg(long, global = true)]
    no_color: bool,

    /// Always re-resolve, even if the lockfile is up to date.
    ///
    /// Global counterpart to the same `install` flag.
    #[arg(long, global = true, conflicts_with_all = ["frozen_lockfile", "prefer_frozen_lockfile"])]
    no_frozen_lockfile: bool,

    /// Use the lockfile when fresh, re-resolve when stale.
    ///
    /// Global counterpart to the same `install` flag.
    #[arg(long, global = true, conflicts_with_all = ["frozen_lockfile", "no_frozen_lockfile"])]
    prefer_frozen_lockfile: bool,

    /// Override the default registry URL for this invocation.
    ///
    /// Mirrors pnpm's `--registry=<url>` placement. Can appear before
    /// or after subcommands; commands that already expose their own
    /// `--registry` flag (publish, login, view, …) still win for their
    /// own traffic.
    #[arg(long, global = true, value_name = "URL")]
    registry: Option<String>,

    /// Output format: default, append-only, ndjson, silent.
    ///
    /// `default` renders the progress UI when stderr is a TTY;
    /// `append-only` disables the progress UI in favor of plain
    /// line-at-a-time logs; `ndjson` swaps the tracing fmt layer for
    /// the JSON formatter (one JSON object per log event on stderr)
    /// and is what tooling wrappers should consume; `silent`
    /// suppresses all non-error output (alias for `--loglevel silent`).
    #[arg(long, global = true, value_name = "NAME", value_enum)]
    reporter: Option<ReporterType>,

    /// Suppress all non-error output (alias for `--loglevel silent`)
    #[arg(long, global = true)]
    silent: bool,

    /// Stream workspace command output as each child process writes it.
    ///
    /// Accepted for pnpm compatibility; aube's workspace fanout is
    /// currently sequential.
    #[arg(long, global = true, conflicts_with = "aggregate_output")]
    stream: bool,

    /// Route lifecycle and workspace command output through stderr.
    ///
    /// Accepted for pnpm compatibility.
    #[arg(long, global = true)]
    use_stderr: bool,

    /// Prefer workspace packages when resolving dependencies.
    ///
    /// Parsed for pnpm compatibility; aube already resolves workspace
    /// packages when a workspace is present.
    #[arg(long, global = true)]
    workspace_packages: bool,

    /// Run from the workspace root regardless of the current package.
    #[arg(long, global = true)]
    workspace_root: bool,

    /// Automatically answer yes to prompts.
    ///
    /// Parsed for pnpm compatibility; aube does not currently prompt
    /// on these paths.
    #[arg(short = 'y', long, global = true)]
    yes: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Silent,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub(crate) enum ReporterType {
    Default,
    AppendOnly,
    Ndjson,
    Silent,
}

impl LogLevel {
    fn filter(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
            LogLevel::Silent => "off",
        }
    }
}

/// Redirects stderr (fd 2) to `/dev/null` for its lifetime, restoring the
/// original on drop. Used by `--silent` to suppress the ~230 direct
/// `eprintln!` calls scattered across command implementations without
/// rewriting them all. The guard must be dropped *before* `main` returns
/// so that any `miette` error report bubbled up through `?` is printed to
/// the real stderr. Stdout is left alone — `aube --silent config get foo`
/// should still emit data to a pipe.
struct SilentStderrGuard {
    saved: libc::c_int,
}

impl SilentStderrGuard {
    fn install() -> Option<Self> {
        unsafe {
            let saved = libc::dup(2);
            if saved < 0 {
                return None;
            }
            let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
            if devnull < 0 {
                libc::close(saved);
                return None;
            }
            if libc::dup2(devnull, 2) < 0 {
                libc::close(devnull);
                libc::close(saved);
                return None;
            }
            libc::close(devnull);
            Some(Self { saved })
        }
    }
}

impl Drop for SilentStderrGuard {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, 2);
            libc::close(self.saved);
        }
    }
}

// Commands are listed in alphabetical order; validated by
// `cli_ordering_tests::test_cli_ordering`. Per-command arg fields are
// similarly sorted: positional first, then short flags by short option,
// then long-only flags alphabetically. The `External` catch-all is last
// because clap's external_subcommand must come after named variants; it
// has no fixed name so the sort check skips it.
#[derive(Subcommand)]
enum Commands {
    /// Add a dependency
    Add(commands::add::AddArgs),
    /// Approve ignored dependency build scripts and record them in `pnpm-workspace.yaml`'s `onlyBuiltDependencies`
    ApproveBuilds(commands::approve_builds::ApproveBuildsArgs),
    /// Check installed packages against the registry advisory DB
    #[command(after_long_help = commands::audit::AFTER_LONG_HELP)]
    Audit(commands::audit::AuditArgs),
    /// Print the path to `node_modules/.bin`
    #[command(after_long_help = commands::bin::AFTER_LONG_HELP)]
    Bin(commands::bin::BinArgs),
    /// Inspect and manage the packument metadata cache
    Cache(commands::cache::CacheArgs),
    /// Print a file from the global store by integrity or hex hash
    CatFile(commands::cat_file::CatFileArgs),
    /// Print the cached package index JSON for `<name>@<version>`
    CatIndex(commands::cat_index::CatIndexArgs),
    /// Clean install: delete node_modules, then install with frozen lockfile.
    ///
    /// Use in CI to guarantee a reproducible install from the committed lockfile.
    #[command(visible_alias = "clean-install", aliases = ["ic", "install-clean"])]
    Ci(commands::ci::CiArgs),
    /// Remove `node_modules` across every workspace project.
    ///
    /// `--lockfile` / `-l` also deletes lockfiles. A `clean` script in
    /// the root `package.json` overrides the built-in.
    Clean(commands::clean::CleanArgs),
    /// Generate shell completions (bash, zsh, fish)
    Completion(commands::completion::CompletionArgs),
    /// Read and write settings in `.npmrc`
    #[command(alias = "c")]
    Config(commands::config::ConfigArgs),
    /// Scaffold a project from a `create-*` starter kit (via dlx)
    Create(commands::create::CreateArgs),
    /// Re-resolve the lockfile to collapse duplicate versions
    Dedupe(commands::dedupe::DedupeArgs),
    /// Deploy a workspace package into a target directory with deps inlined
    Deploy(commands::deploy::DeployArgs),
    /// Mark published versions of a package as deprecated on the registry
    Deprecate(commands::deprecate::DeprecateArgs),
    /// Manage package distribution tags on the registry
    #[command(visible_alias = "dist-tags")]
    DistTag(commands::dist_tag::DistTagArgs),
    /// Fetch a package into a throwaway environment and run its binary
    Dlx(commands::dlx::DlxArgs),
    /// Execute a locally installed binary
    Exec(commands::exec::ExecArgs),
    /// Download lockfile dependencies into the store without linking node_modules
    Fetch(commands::fetch::FetchArgs),
    /// List packages whose cached index references a given file hash
    #[command(after_long_help = commands::find_hash::AFTER_LONG_HELP)]
    FindHash(commands::find_hash::FindHashArgs),
    /// Alias for `config get` (hidden; prefer `config get`)
    #[command(hide = true)]
    Get(commands::config::GetArgs),
    /// Print packages whose install scripts were skipped by `pnpm.allowBuilds`
    #[command(after_long_help = commands::ignored_builds::AFTER_LONG_HELP)]
    IgnoredBuilds(commands::ignored_builds::IgnoredBuildsArgs),
    /// Convert a supported lockfile into aube-lock.yaml
    Import(commands::import::ImportArgs),
    /// Create a `package.json` in the current directory
    Init(commands::init::InitArgs),
    /// Install all dependencies
    #[command(alias = "i")]
    Install(commands::install::InstallArgs),
    /// Install dependencies, then run the `test` script (pnpm compat alias).
    ///
    /// Hidden from help because `aube test` already auto-installs.
    #[command(alias = "it", hide = true)]
    InstallTest(commands::run::ScriptArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    La(commands::list::ListArgs),
    /// Report the licenses of installed dependencies
    #[command(after_long_help = commands::licenses::AFTER_LONG_HELP)]
    Licenses(commands::licenses::LicensesArgs),
    /// Link a local package globally, or into the current project
    #[command(visible_alias = "ln")]
    Link(commands::link::LinkArgs),
    /// Print the installed dependency tree
    #[command(visible_alias = "ls", after_long_help = commands::list::AFTER_LONG_HELP)]
    List(commands::list::ListArgs),
    /// Alias for `list --long` (hidden; prefer `list --long`)
    #[command(hide = true)]
    Ll(commands::list::ListArgs),
    /// Store a registry auth token in the user's ~/.npmrc
    #[command(alias = "adduser")]
    Login(commands::login::LoginArgs),
    /// Remove a registry auth token from the user's ~/.npmrc
    Logout(commands::logout::LogoutArgs),
    /// Report dependencies whose installed version lags behind the registry
    #[command(after_long_help = commands::outdated::AFTER_LONG_HELP)]
    Outdated(commands::outdated::OutdatedArgs),
    /// Manage package owners (not implemented — use `npm owner`)
    #[command(hide = true)]
    Owner(commands::npm_fallback::FallbackArgs),
    /// Create a publishable `.tgz` tarball from the current project
    Pack(commands::pack::PackArgs),
    /// Extract a package into an edit directory so it can be patched
    Patch(commands::patch::PatchArgs),
    /// Generate a `.patch` file from a `aube patch` edit directory
    PatchCommit(commands::patch_commit::PatchCommitArgs),
    /// Remove patch entries from `pnpm.patchedDependencies`
    PatchRemove(commands::patch_remove::PatchRemoveArgs),
    /// Inspect peer-dependency resolution from the lockfile
    Peers(commands::peers::PeersArgs),
    /// Manage package.json entries (not implemented — use `npm pkg`)
    #[command(hide = true)]
    Pkg(commands::npm_fallback::FallbackArgs),
    /// Remove extraneous packages from node_modules
    Prune(commands::prune::PruneArgs),
    /// Publish the current package to the registry
    Publish(commands::publish::PublishArgs),
    /// Alias for `clean` — remove `node_modules` across every workspace project.
    ///
    /// A `purge` script in the root `package.json` overrides the built-in.
    Purge(commands::clean::CleanArgs),
    /// Re-run root lifecycle scripts and allowlisted dependency builds
    #[command(visible_alias = "rb")]
    Rebuild(commands::rebuild::RebuildArgs),
    /// Run a supported command across workspace packages
    #[command(visible_aliases = ["multi", "m"])]
    Recursive(commands::recursive::RecursiveArgs),
    /// Remove a dependency
    #[command(visible_alias = "rm", aliases = ["uninstall", "un", "uni"])]
    Remove(commands::remove::RemoveArgs),
    /// Restart a package (shortcut for `run restart`; falls back to `stop` + `start`)
    Restart(commands::run::ScriptArgs),
    /// Print the path to `node_modules`
    #[command(after_long_help = commands::root::AFTER_LONG_HELP)]
    Root(commands::root::RootArgs),
    /// Run a script defined in package.json
    #[command(alias = "run-script")]
    Run(commands::run::RunArgs),
    /// Generate a Software Bill of Materials (CycloneDX or SPDX)
    Sbom(commands::sbom::SbomArgs),
    /// Search the registry for packages (not implemented — use `npm search`)
    #[command(hide = true)]
    Search(commands::npm_fallback::FallbackArgs),
    /// Alias for `config set` (hidden; prefer `config set`)
    #[command(hide = true)]
    Set(commands::config::SetArgs),
    /// Set a `package.json` script (not implemented — use `npm set-script`)
    #[command(hide = true, name = "set-script")]
    SetScript(commands::npm_fallback::FallbackArgs),
    /// Start a package (shortcut for `run start`)
    Start(commands::run::ScriptArgs),
    /// Stop a package (shortcut for `run stop`)
    Stop(commands::run::ScriptArgs),
    /// Manage the global store
    Store(commands::store::StoreArgs),
    /// Run the `test` script (shortcut for `run test`)
    #[command(visible_alias = "t")]
    Test(commands::run::ScriptArgs),
    /// Manage registry auth tokens (not implemented — use `npm token`)
    #[command(hide = true)]
    Token(commands::npm_fallback::FallbackArgs),
    /// Clear an existing deprecation on the registry
    Undeprecate(commands::undeprecate::UndeprecateArgs),
    /// Unlink a package (remove linked entries from node_modules)
    #[command(alias = "dislink")]
    Unlink(commands::unlink::UnlinkArgs),
    /// Remove a package (or a single version) from the registry
    Unpublish(commands::unpublish::UnpublishArgs),
    /// Update dependencies
    #[command(aliases = ["up", "upgrade"])]
    Update(commands::update::UpdateArgs),
    /// Print a usage.jdx.dev KDL spec for the CLI (internal)
    #[command(hide = true)]
    Usage,
    /// Bump the version in package.json (and optionally create a git commit + tag)
    Version(commands::version::VersionArgs),
    /// Print package metadata from the registry
    #[command(visible_aliases = ["info", "show"], alias = "v", after_long_help = commands::view::AFTER_LONG_HELP)]
    View(commands::view::ViewArgs),
    /// Report the current registry user (not implemented — use `npm whoami`)
    #[command(hide = true)]
    Whoami(commands::npm_fallback::FallbackArgs),
    /// Print reverse dependency chains explaining why a package is installed
    #[command(after_long_help = commands::why::AFTER_LONG_HELP)]
    Why(commands::why::WhyArgs),
    /// Catch-all for implicit script execution (e.g., `aube dev` = `aube run dev`)
    #[command(external_subcommand)]
    External(Vec<String>),
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse_from(rewrite_multicall_argv(std::env::args_os().collect()));

    // `--color` / `--no-color` take effect before anything else touches
    // color state: we translate the flags into the env vars that miette,
    // clx, `supports-color`, and spawned child processes all already
    // consult, so the choice is consistent across every output path and
    // inherits into `run` / `exec` / lifecycle scripts. The explicit flag
    // wins over whatever was in the environment — that's what pnpm does.
    //
    // This has to happen *before* we build the Tokio runtime: the Rust
    // 2024 contract on `std::env::set_var` requires that no other
    // threads exist, and a multi-threaded runtime spawns its worker
    // pool during `build()`. So we keep `main` synchronous, mutate env
    // here, and only then enter the async body.
    let color_mode = resolve_color_mode(&cli);
    if matches!(color_mode, ColorMode::Never) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("NO_COLOR", "1");
            std::env::remove_var("FORCE_COLOR");
            std::env::remove_var("CLICOLOR_FORCE");
        }
    } else if matches!(color_mode, ColorMode::Always) {
        // SAFETY: single-threaded `main` — no other threads exist yet.
        unsafe {
            std::env::set_var("FORCE_COLOR", "1");
            std::env::set_var("CLICOLOR_FORCE", "1");
            std::env::remove_var("NO_COLOR");
        }
    }

    // `--use-stderr` / `.npmrc` `useStderr=true`: redirect stdout to stderr
    // so all output goes through a single fd. Resolved here (single-threaded)
    // before the tokio runtime spawns workers.
    //
    // Skip when `--silent` is active: the SilentStderrGuard later redirects
    // fd 2 to /dev/null, and if we dup2 first, fd 1 would capture the real
    // stderr and escape silencing.
    let is_silent = cli.silent || matches!(cli.reporter, Some(ReporterType::Silent));
    if !is_silent {
        let use_stderr_active = cli.use_stderr
            || startup_cwd(&cli).ok().is_some_and(|cwd| {
                let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
                let ws = std::collections::BTreeMap::new();
                let env_snap = aube_settings::values::capture_env();
                let ctx = aube_settings::ResolveCtx {
                    npmrc: &npmrc,
                    workspace_yaml: &ws,
                    env: &env_snap,
                    cli: &[],
                };
                aube_settings::resolved::use_stderr(&ctx)
            });
        if use_stderr_active {
            // SAFETY: single-threaded `main` — no other threads exist yet.
            // `dup2(stderr, stdout)` makes fd 1 point at the same file as fd 2.
            unsafe {
                libc::dup2(2, 1);
            }
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .into_diagnostic()
        .wrap_err("failed to build tokio runtime")?;
    let exit_code = runtime.block_on(async_main(cli))?;
    drop(runtime);
    if let Some(exit_code) = exit_code {
        std::process::exit(exit_code);
    }
    Ok(())
}

async fn async_main(cli: Cli) -> miette::Result<Option<i32>> {
    // Default log level is `warn` so routine install output doesn't collide
    // with the clx progress display. `-v` / `--verbose` and `--loglevel debug`
    // turn on debug logging, and in that mode we also force clx into Text
    // output so the progress UI never renders over the log lines. `--silent`
    // (and `--loglevel silent`) turn logging off entirely and disable the
    // progress UI.
    // `--reporter=silent` is equivalent to `--silent`; all other reporter
    // values leave the log level alone and only affect output routing.
    if let Some(dir) = &cli.dir {
        std::env::set_current_dir(dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to change directory to {}", dir.display()))?;
    }

    if cli.workspace_root {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let settings = load_startup_settings()?;
    let effective_level = resolve_loglevel(&cli, settings.loglevel.as_deref());
    init_logging(&cli, effective_level);

    // `--silent` suppresses non-error stderr output from every command,
    // including the ~230 direct `eprintln!` calls in command bodies. The
    // guard restores fd 2 on drop (before main returns), so miette still
    // prints error reports to the real stderr. We also register the
    // saved fd with aube-scripts so child processes spawned via
    // `aube_scripts::child_stderr()` (lifecycle scripts, `aube run`,
    // `aube exec`, `aube dlx`) keep writing to the real terminal — only
    // aube's own output is silenced, matching `pnpm --loglevel silent`.
    let _silent_guard = matches!(effective_level, LogLevel::Silent)
        .then(SilentStderrGuard::install)
        .flatten();
    if let Some(ref guard) = _silent_guard {
        aube_scripts::set_saved_stderr_fd(guard.saved);
    }

    if command_needs_package_manager_guard(cli.command.as_ref()) {
        enforce_package_manager_guardrails(&settings)?;
    }

    // `--recursive` / `-r` is sugar for `--filter=*`. When a filter is
    // already set, `-r` is a no-op — the explicit scope wins.
    let effective_filter = compute_effective_filter(&cli);

    // Snapshot the global frozen-lockfile flags so every install entry
    // point (direct `install`, chained `add`/`remove`/`update`, bare
    // `aube`, auto-install via `ensure_installed`) honors them.
    let global_frozen = frozen_flags_from_cli(&cli);
    commands::set_global_frozen_flags(global_frozen);
    commands::set_registry_override(cli.registry.clone());
    commands::set_global_output_flags(commands::GlobalOutputFlags {
        silent: matches!(effective_level, LogLevel::Silent),
    });

    match cli.command {
        Some(Commands::Add(args)) => {
            commands::add::run(args, effective_filter.clone()).await?;
            post_add_update_notify().await;
        }
        Some(Commands::ApproveBuilds(args)) => commands::approve_builds::run(args).await?,
        Some(Commands::Audit(args)) => commands::audit::run(args).await?,
        Some(Commands::Bin(args)) => commands::bin::run(args).await?,
        Some(Commands::Cache(args)) => commands::cache::run(args).await?,
        Some(Commands::CatFile(args)) => commands::cat_file::run(args).await?,
        Some(Commands::CatIndex(args)) => commands::cat_index::run(args).await?,
        Some(Commands::Ci(args)) => commands::ci::run(args).await?,
        Some(Commands::Clean(args)) => commands::clean::run(args).await?,
        Some(Commands::Completion(args)) => commands::completion::run(args).await?,
        Some(Commands::Config(args)) => commands::config::run(args).await?,
        Some(Commands::Create(args)) => commands::create::run(args).await?,
        Some(Commands::Dedupe(args)) => commands::dedupe::run(args).await?,
        Some(Commands::Deploy(args)) => {
            commands::deploy::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Deprecate(args)) => commands::deprecate::run(args).await?,
        Some(Commands::DistTag(args)) => commands::dist_tag::run(args).await?,
        Some(Commands::Dlx(args)) => commands::dlx::run(args).await?,
        Some(Commands::Exec(args)) => commands::exec::run(args, effective_filter.clone()).await?,
        Some(Commands::Fetch(args)) => commands::fetch::run(args).await?,
        Some(Commands::FindHash(args)) => commands::find_hash::run(args).await?,
        Some(Commands::Get(args)) => commands::config::get(args)?,
        Some(Commands::IgnoredBuilds(args)) => commands::ignored_builds::run(args).await?,
        Some(Commands::Import(args)) => commands::import::run(args).await?,
        Some(Commands::Init(args)) => commands::init::run(args).await?,
        Some(Commands::Install(args)) => {
            run_install_command(
                args,
                global_frozen,
                effective_filter.clone(),
                cli.workspace_root,
            )
            .await?;
        }
        Some(Commands::InstallTest(args)) => commands::install_test::run(args).await?,
        Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
            args.long = true;
            commands::list::run(args, effective_filter.clone()).await?;
        }
        Some(Commands::Licenses(args)) => commands::licenses::run(args).await?,
        Some(Commands::Link(args)) => commands::link::run(args).await?,
        Some(Commands::List(args)) => commands::list::run(args, effective_filter.clone()).await?,
        Some(Commands::Login(args)) => commands::login::run(args).await?,
        Some(Commands::Logout(args)) => commands::logout::run(args).await?,
        Some(Commands::Outdated(args)) => {
            commands::outdated::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Owner(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "owner",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Pack(args)) => commands::pack::run(args).await?,
        Some(Commands::Patch(args)) => commands::patch::run(args).await?,
        Some(Commands::PatchCommit(args)) => commands::patch_commit::run(args).await?,
        Some(Commands::PatchRemove(args)) => commands::patch_remove::run(args).await?,
        Some(Commands::Peers(args)) => commands::peers::run(args).await?,
        Some(Commands::Pkg(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "pkg",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Prune(args)) => commands::prune::run(args).await?,
        Some(Commands::Publish(args)) => {
            commands::publish::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Purge(args)) => commands::clean::run_purge(args).await?,
        Some(Commands::Rebuild(args)) => {
            commands::rebuild::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Remove(args)) => {
            commands::remove::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Recursive(args)) => {
            let argv = commands::recursive::argv(
                args,
                commands::recursive::RecursiveGlobals {
                    filters: effective_filter.clone(),
                    color: cli.color,
                    no_color: cli.no_color,
                },
            )?;
            let nested = Cli::try_parse_from(argv).into_diagnostic()?;
            let nested_filter = compute_effective_filter(&nested);
            let nested_frozen = merge_nested_frozen_flags(global_frozen, &nested);
            let _registry_guard = commands::scoped_registry_override(nested.registry.clone());
            match nested.command {
                Some(Commands::Add(args)) => {
                    commands::add::run(args, nested_filter).await?;
                    post_add_update_notify().await;
                }
                Some(Commands::Deploy(args)) => commands::deploy::run(args, nested_filter).await?,
                Some(Commands::Exec(args)) => commands::exec::run(args, nested_filter).await?,
                Some(Commands::Install(args)) => {
                    run_install_command(args, nested_frozen, nested_filter, nested.workspace_root)
                        .await?;
                }
                Some(Commands::List(args)) => commands::list::run(args, nested_filter).await?,
                Some(Commands::La(mut args)) | Some(Commands::Ll(mut args)) => {
                    args.long = true;
                    commands::list::run(args, nested_filter).await?;
                }
                Some(Commands::Outdated(args)) => {
                    commands::outdated::run(args, nested_filter).await?
                }
                Some(Commands::Publish(args)) => {
                    commands::publish::run(args, nested_filter).await?
                }
                Some(Commands::Rebuild(args)) => {
                    commands::rebuild::run(args, nested_filter).await?
                }
                Some(Commands::Remove(args)) => commands::remove::run(args, nested_filter).await?,
                Some(Commands::Restart(args)) => {
                    commands::restart::run(args, nested_filter).await?
                }
                Some(Commands::Run(args)) => commands::run::run(args, nested_filter).await?,
                Some(Commands::Start(args)) => {
                    commands::run::run_script(
                        "start",
                        &args.args,
                        args.no_install,
                        false,
                        &nested_filter,
                    )
                    .await?;
                }
                Some(Commands::Stop(args)) => {
                    commands::run::run_script(
                        "stop",
                        &args.args,
                        args.no_install,
                        false,
                        &nested_filter,
                    )
                    .await?;
                }
                Some(Commands::Test(args)) => {
                    commands::run::run_script(
                        "test",
                        &args.args,
                        args.no_install,
                        false,
                        &nested_filter,
                    )
                    .await?;
                }
                Some(Commands::Update(args)) => {
                    commands::update::run(args, nested_filter).await?;
                    post_add_update_notify().await;
                }
                Some(Commands::Why(args)) => commands::why::run(args, nested_filter).await?,
                Some(Commands::External(args)) => {
                    let script = &args[0];
                    let script_args: Vec<String> = args[1..].to_vec();
                    commands::run::run_script(script, &script_args, false, false, &nested_filter)
                        .await?;
                }
                Some(_) | None => {
                    return Err(miette::miette!(
                        "aube recursive: command does not support recursive execution"
                    ));
                }
            }
        }
        Some(Commands::Restart(args)) => {
            commands::restart::run(args, effective_filter.clone()).await?
        }
        Some(Commands::Root(args)) => commands::root::run(args).await?,
        Some(Commands::Run(args)) => commands::run::run(args, effective_filter.clone()).await?,
        Some(Commands::Sbom(args)) => commands::sbom::run(args).await?,
        Some(Commands::Search(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "search",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Set(args)) => commands::config::set(args)?,
        Some(Commands::SetScript(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "set-script",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Start(args)) => {
            commands::run::run_script(
                "start",
                &args.args,
                args.no_install,
                false,
                &effective_filter,
            )
            .await?;
        }
        Some(Commands::Stop(args)) => {
            commands::run::run_script(
                "stop",
                &args.args,
                args.no_install,
                false,
                &effective_filter,
            )
            .await?;
        }
        Some(Commands::Store(args)) => commands::store::run(args).await?,
        Some(Commands::Test(args)) => {
            commands::run::run_script(
                "test",
                &args.args,
                args.no_install,
                false,
                &effective_filter,
            )
            .await?;
        }
        Some(Commands::Token(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "token",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Undeprecate(args)) => commands::undeprecate::run(args).await?,
        Some(Commands::Unlink(args)) => commands::unlink::run(args).await?,
        Some(Commands::Unpublish(args)) => commands::unpublish::run(args).await?,
        Some(Commands::Update(args)) => {
            commands::update::run(args, effective_filter.clone()).await?;
            post_add_update_notify().await;
        }
        Some(Commands::Version(args)) => commands::version::run(args).await?,
        Some(Commands::View(args)) => commands::view::run(args).await?,
        Some(Commands::Whoami(args)) => {
            return Ok(Some(commands::npm_fallback::run(
                "whoami",
                &args.args,
                cli.registry.as_deref(),
            )?));
        }
        Some(Commands::Why(args)) => commands::why::run(args, effective_filter.clone()).await?,
        Some(Commands::Usage) => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            clap_usage::generate(&mut cmd, "aube", &mut std::io::stdout());
        }
        Some(Commands::External(args)) => {
            // Implicit run: `aube dev` = `aube run dev`.
            //
            // External is clap's catch-all, so a typo like `aube fooefjwol`
            // lands here too. If the name isn't an actual script in the
            // local `package.json` (or there's no `package.json` at all),
            // print `aube --help` and bail instead of routing it into the
            // script runner and surfacing a confusing "script not found"
            // or "failed to read package.json" — the user typed something
            // we don't recognize and help is the most useful reply.
            //
            // The pre-check only fires when *no* workspace filter is
            // active: `-r` / `-F` fan implicit scripts out across
            // sub-packages, and the script may live in one of the
            // matched workspaces while the root `package.json` has no
            // `scripts` entry at all. In that mode we hand off to
            // `run_script` unchanged and let the filtered runner
            // produce its own per-package diagnostics.
            let script = &args[0];
            let script_args: Vec<String> = args[1..].to_vec();
            if effective_filter.is_empty() {
                let initial_cwd = crate::dirs::cwd()?;
                let script_exists = crate::dirs::find_project_root(&initial_cwd)
                    .and_then(|cwd| {
                        aube_manifest::PackageJson::from_path(&cwd.join("package.json")).ok()
                    })
                    .map(|m| m.scripts.contains_key(script))
                    .unwrap_or(false);
                if !script_exists {
                    use clap::CommandFactory;
                    let mut cmd = Cli::command();
                    cmd.print_help().ok();
                    eprintln!();
                    return Err(miette::miette!("unknown command: {script}"));
                }
            }
            commands::run::run_script(script, &script_args, false, false, &effective_filter)
                .await?;
        }
        None => {
            // Bare `aube` prints `--help` and exits 0, matching pnpm.
            // pnpm's bare invocation does not run an install; users who
            // want that behavior should type `aube install` explicitly.
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            cmd.print_help().ok();
            println!();
        }
    }

    Ok(None)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug)]
struct StartupSettings {
    loglevel: Option<String>,
    package_manager_strict: bool,
    package_manager_strict_version: bool,
}

fn resolve_color_mode(cli: &Cli) -> ColorMode {
    if cli.no_color {
        return ColorMode::Never;
    }
    if cli.color {
        return ColorMode::Always;
    }
    let env = aube_settings::values::capture_env();
    if let Some(mode) =
        aube_settings::values::string_from_env("color", &env).and_then(|raw| parse_color_mode(&raw))
    {
        return mode;
    }
    let Ok(cwd) = startup_cwd(cli) else {
        return ColorMode::Auto;
    };
    let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
    aube_settings::values::string_from_npmrc("color", &npmrc)
        .and_then(|raw| parse_color_mode(&raw))
        .unwrap_or(ColorMode::Auto)
}

fn startup_cwd(cli: &Cli) -> miette::Result<PathBuf> {
    let cwd = match &cli.dir {
        Some(dir) if dir.is_absolute() => Ok(dir.clone()),
        Some(dir) => std::env::current_dir()
            .into_diagnostic()
            .map(|cwd| cwd.join(dir)),
        None => std::env::current_dir().into_diagnostic(),
    }?;
    if cli.workspace_root {
        commands::find_workspace_root(&cwd)
    } else {
        Ok(cwd)
    }
}

fn load_startup_settings() -> miette::Result<StartupSettings> {
    let cwd = std::env::current_dir().into_diagnostic()?;
    let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
    let empty_ws = std::collections::BTreeMap::new();
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &empty_ws,
        env: &env,
        cli: &[],
    };
    Ok(StartupSettings {
        loglevel: aube_settings::values::string_from_env("loglevel", &env)
            .or_else(|| aube_settings::values::string_from_npmrc("loglevel", &npmrc)),
        package_manager_strict: aube_settings::resolved::package_manager_strict(&ctx),
        package_manager_strict_version: aube_settings::resolved::package_manager_strict_version(
            &ctx,
        ),
    })
}

fn resolve_loglevel(cli: &Cli, configured: Option<&str>) -> LogLevel {
    let reporter_silent = matches!(cli.reporter, Some(ReporterType::Silent));
    if cli.silent || reporter_silent {
        return LogLevel::Silent;
    }
    if let Some(level) = cli.loglevel {
        return level;
    }
    if env_is_truthy("AUBE_TRACE") {
        return LogLevel::Trace;
    }
    if cli.verbose || env_is_truthy("AUBE_DEBUG") {
        return LogLevel::Debug;
    }
    configured
        .and_then(parse_loglevel)
        .unwrap_or(LogLevel::Warn)
}

fn env_is_truthy(name: &str) -> bool {
    let Ok(raw) = std::env::var(name) else {
        return false;
    };
    matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y"
    )
}

fn parse_loglevel(raw: &str) -> Option<LogLevel> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(LogLevel::Trace),
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "silent" => Some(LogLevel::Silent),
        _ => None,
    }
}

fn parse_color_mode(raw: &str) -> Option<ColorMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "always" | "true" | "1" => Some(ColorMode::Always),
        "never" | "false" | "0" => Some(ColorMode::Never),
        "auto" => Some(ColorMode::Auto),
        _ => None,
    }
}

fn init_logging(cli: &Cli, effective_level: LogLevel) {
    let log_level = effective_level.filter();
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("AUBE_LOG")
        .unwrap_or_else(|_| format!("aube={log_level},aube_cli={log_level}").into());

    // ndjson swaps the fmt layer for the JSON formatter so every tracing
    // event is serialized as a single line of JSON on stderr. The filter
    // itself is the same as every other mode — `--loglevel` / `--verbose`
    // / `AUBE_DEBUG` / `AUBE_LOG` pick the verbosity, ndjson just changes
    // the encoding.
    //
    // Every mode routes writes through `progress::PausingWriter`, which
    // pauses the clx progress display and holds the terminal lock across
    // each event so a `warn!` emitted mid-render doesn't get smeared
    // through the animated bar. In text mode (append-only / ndjson /
    // silent / verbose) the progress display never starts, so the writer
    // degrades to a plain stderr flush.
    //
    // Timestamps are dropped unless the user asked for `debug` verbosity
    // (via `-v` / `--loglevel=debug` / `AUBE_DEBUG=1` / `AUBE_LOG`). A
    // default-verbosity install that emits deprecated-package warnings
    // reads more like pnpm's `WARN: mathjax-full@3.2.2 is deprecated…`
    // than a server log line; keeping the RFC3339 prefix just pushes the
    // package name off the visible width for no gain. ndjson always
    // keeps the timestamp — its whole point is machine-parseable records.
    let drop_timestamp = !matches!(effective_level, LogLevel::Debug | LogLevel::Trace);
    let registry = tracing_subscriber::registry().with(env_filter);
    if matches!(cli.reporter, Some(ReporterType::Ndjson)) {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else if drop_timestamp {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_writer(crate::progress::PausingWriter),
            )
            .init();
    } else {
        registry
            .with(tracing_subscriber::fmt::layer().with_writer(crate::progress::PausingWriter))
            .init();
    }

    // Force clx into plain text mode whenever the progress UI would collide
    // with the reporter: `append-only` and `ndjson` both want line-at-a-time
    // output, and `debug`/`silent` already disabled the UI for their own
    // reasons.
    let force_text = matches!(
        effective_level,
        LogLevel::Trace | LogLevel::Debug | LogLevel::Silent
    ) || matches!(
        cli.reporter,
        Some(ReporterType::AppendOnly) | Some(ReporterType::Ndjson)
    );
    if force_text {
        clx::progress::set_output(clx::progress::ProgressOutput::Text);
    }
}

fn enforce_package_manager_guardrails(settings: &StartupSettings) -> miette::Result<()> {
    if !settings.package_manager_strict {
        return Ok(());
    }

    let cwd = std::env::current_dir().into_diagnostic()?;
    let Some(root) = crate::dirs::find_workspace_root(&cwd)
        .filter(|root| root.join("package.json").is_file())
        .or_else(|| crate::dirs::find_project_root(&cwd))
    else {
        return Ok(());
    };
    let path = root.join("package.json");
    let raw = std::fs::read_to_string(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", path.display()))?;
    let Some(package_manager) = json.get("packageManager").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some((name, version)) = parse_package_manager(package_manager) else {
        return Err(miette!(
            "invalid packageManager field `{package_manager}` in {}",
            path.display()
        ));
    };

    match name {
        "aube" => {
            if settings.package_manager_strict_version && version != env!("CARGO_PKG_VERSION") {
                return Err(miette!(
                    "packageManager requires aube@{version}, but this is aube@{}",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Ok(())
        }
        "pnpm" => {
            if settings.package_manager_strict_version {
                return Err(miette!(
                    "packageManager requires exact pnpm@{version}, but aube cannot download or re-exec a specific pnpm version. Use pnpm directly, set packageManagerStrictVersion=false, or pin packageManager to aube@{}.",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            Ok(())
        }
        other => Err(miette!(
            "packageManager in {} uses unsupported package manager `{other}`. aube's packageManagerStrict guard is on by default and only accepts `aube` and `pnpm`; remove or change the `packageManager` field, or set `packageManagerStrict=false` in .npmrc to skip this guard.",
            path.display()
        )),
    }
}

fn parse_package_manager(raw: &str) -> Option<(&str, &str)> {
    let (name, rest) = raw.rsplit_once('@')?;
    if name.is_empty() || rest.is_empty() {
        return None;
    }
    let version = rest.split_once('+').map_or(rest, |(version, _)| version);
    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

fn command_needs_package_manager_guard(command: Option<&Commands>) -> bool {
    !matches!(
        command,
        None | Some(Commands::Config(_))
            | Some(Commands::Get(_))
            | Some(Commands::Set(_))
            | Some(Commands::Completion(_))
            | Some(Commands::Usage)
    )
}

fn compute_effective_filter(cli: &Cli) -> aube_workspace::selector::EffectiveFilter {
    // `--recursive` / `-r` is sugar for `--filter=*`, so the wildcard
    // only joins the regular filter list — never `--filter-prod`. When
    // the user supplies either flag explicitly, `-r` is a no-op.
    let mut filters = cli.filter.clone();
    if cli.recursive && filters.is_empty() && cli.filter_prod.is_empty() {
        filters.push("*".to_string());
    }
    aube_workspace::selector::EffectiveFilter {
        filters,
        filter_prods: cli.filter_prod.clone(),
    }
}

fn frozen_flags_from_cli(cli: &Cli) -> commands::install::GlobalFrozenFlags {
    commands::install::GlobalFrozenFlags {
        frozen: cli.frozen_lockfile,
        no_frozen: cli.no_frozen_lockfile,
        prefer_frozen: cli.prefer_frozen_lockfile,
    }
}

fn merge_nested_frozen_flags(
    outer: commands::install::GlobalFrozenFlags,
    nested: &Cli,
) -> commands::install::GlobalFrozenFlags {
    if outer.frozen || outer.no_frozen || outer.prefer_frozen {
        outer
    } else {
        frozen_flags_from_cli(nested)
    }
}

async fn run_install_command(
    args: commands::install::InstallArgs,
    global_frozen: commands::install::GlobalFrozenFlags,
    filter: aube_workspace::selector::EffectiveFilter,
    workspace_root_already: bool,
) -> miette::Result<()> {
    // `-w` on install is a short alias for the global
    // `--workspace-root` flag. Handle the chdir here when the global
    // flag wasn't already set.
    if args.workspace_root_short && !workspace_root_already {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = commands::find_workspace_root(&start)?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to change directory to {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }
    let cwd = crate::dirs::project_root()?;
    let npmrc = aube_registry::config::load_npmrc_entries(&cwd);
    let raw_ws = aube_manifest::workspace::load_raw(&cwd)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let env = aube_settings::values::capture_env();
    let cli_flags = args.to_cli_flag_bag(global_frozen);
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc,
        workspace_yaml: &raw_ws,
        env: &env,
        cli: &cli_flags,
    };
    let yaml_prefer_frozen = aube_settings::resolved::prefer_frozen_lockfile(&ctx);
    let offline = args.offline || args.prefer_offline;
    let mut opts = args.into_options(global_frozen, yaml_prefer_frozen, cli_flags, env);
    opts.workspace_filter = filter;
    commands::install::run(opts).await?;
    update_check::check_and_notify(&cwd, offline).await;
    Ok(())
}

/// Fire the update notifier once per top-level `add` / `update`
/// invocation, after the command has fully returned. Kept at the
/// dispatch layer so that the workspace-recursive paths inside
/// `add::run` / `update::run` (which re-enter `run` per matched
/// package via `run_filtered`) don't each emit their own notice.
///
/// Settings resolution needs the project root, not the raw cwd —
/// `load_npmrc_entries` only reads `<dir>/.npmrc` without walking up,
/// so running `aube add` from a subdirectory must still pick up the
/// project-root `.npmrc` (where `updateNotifier=false` would live).
/// Falls back to the cached cwd when no ancestor has a `package.json`,
/// so a command run outside a project doesn't lose the notifier.
async fn post_add_update_notify() {
    if let Ok(cwd) = crate::dirs::project_root_or_cwd() {
        update_check::check_and_notify(&cwd, false).await;
    }
}

#[cfg(test)]
mod cli_spec_tests {
    use super::*;
    use clap::CommandFactory;

    /// Golden snapshot of aube's CLI structure, generated by
    /// `clap_usage::generate`. Regenerate with:
    ///
    /// ```sh
    /// cargo build && ./target/debug/aube usage > aube.usage.kdl
    /// ```
    ///
    /// This test catches any accidental CLI change: renamed flags,
    /// reordered subcommands, added/removed args, etc. If you intended
    /// the change, regenerate the golden file and commit it.
    #[test]
    fn usage_kdl_matches_committed_golden_file() {
        let mut cmd = Cli::command();
        let mut buf: Vec<u8> = Vec::new();
        clap_usage::generate(&mut cmd, "aube", &mut buf);
        let generated = String::from_utf8(buf).expect("clap_usage output not UTF-8");

        let committed = include_str!("../../../aube.usage.kdl");

        if generated != committed {
            // Print a focused diff hint rather than dumping both blobs.
            let gen_lines: Vec<&str> = generated.lines().collect();
            let com_lines: Vec<&str> = committed.lines().collect();
            let mut diff = String::new();
            for (i, (g, c)) in gen_lines.iter().zip(com_lines.iter()).enumerate() {
                if g != c {
                    diff.push_str(&format!("line {}:\n  - {c}\n  + {g}\n", i + 1));
                }
            }
            if gen_lines.len() != com_lines.len() {
                diff.push_str(&format!(
                    "line count differs: committed={} generated={}\n",
                    com_lines.len(),
                    gen_lines.len()
                ));
            }
            panic!(
                "aube.usage.kdl is out of date.\n\n{diff}\n\
                 Regenerate with: cargo build && ./target/debug/aube usage > aube.usage.kdl"
            );
        }
    }

    #[test]
    fn install_accepts_subcommand_registry_flag() {
        let cli = Cli::try_parse_from([
            "aube",
            "install",
            "--registry",
            "https://registry.example.com/",
        ])
        .expect("install --registry should parse");

        assert_eq!(
            cli.registry.as_deref(),
            Some("https://registry.example.com/")
        );
        assert!(matches!(cli.command, Some(Commands::Install(_))));
    }
}

#[cfg(test)]
mod multicall_tests {
    use super::*;

    fn os(strs: &[&str]) -> Vec<OsString> {
        strs.iter().map(OsString::from).collect()
    }

    #[test]
    fn aube_passes_through_unchanged() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aube", "install"])),
            os(&["aube", "install"])
        );
    }

    #[test]
    fn aubr_rewrites_to_run() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubr", "build"])),
            os(&["aube", "run", "build"])
        );
    }

    #[test]
    fn aubx_rewrites_to_dlx() {
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx", "cowsay", "hi"])),
            os(&["aube", "dlx", "cowsay", "hi"])
        );
    }

    #[test]
    fn absolute_path_and_exe_suffix_are_handled() {
        // argv[0] can be an absolute path (exec-style invocation) or carry
        // a `.exe` suffix on Windows. `Path::file_stem` takes care of both
        // so dispatch stays purely basename-driven.
        assert_eq!(
            rewrite_multicall_argv(os(&["/usr/local/bin/aubr", "test"])),
            os(&["aube", "run", "test"])
        );
        assert_eq!(
            rewrite_multicall_argv(os(&["aubx.exe", "pkg"])),
            os(&["aube", "dlx", "pkg"])
        );
    }

    #[test]
    fn bare_shim_invocation_passes_through_to_subcommand() {
        // `aubr` with no further args becomes `aube run`, which clap
        // parses as the `run` subcommand with no positional — same as
        // the user typing `aube run` directly.
        assert_eq!(rewrite_multicall_argv(os(&["aubr"])), os(&["aube", "run"]));
    }
}

#[cfg(test)]
mod cli_ordering_tests {
    use super::*;
    use clap::CommandFactory;

    /// Validate that aube's CLI commands and arguments are ordered via
    /// the `clap-sort` crate:
    /// - Subcommands alphabetical by name
    /// - Short flags alphabetical by short option
    /// - Long-only flags alphabetical by long name
    /// - Positional args keep their source order
    ///
    /// If this fails, reorder the enum variants and `#[arg(...)]` fields
    /// to match the expected order the panic prints.
    #[test]
    fn test_cli_ordering() {
        clap_sort::assert_sorted(&Cli::command());
    }
}
