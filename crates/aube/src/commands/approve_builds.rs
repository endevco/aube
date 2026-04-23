//! `aube approve-builds` — write packages into
//! `pnpm-workspace.yaml`'s `onlyBuiltDependencies` so their install
//! scripts run on the next `aube install`.
//!
//! Walks the lockfile via `ignored_builds::collect_ignored`, presents an
//! interactive multi-select picker (or approves everything under
//! `--all`), then merges the selections into the workspace yaml's
//! `onlyBuiltDependencies` sequence. Matches pnpm v10+, which moved
//! build approvals out of `package.json`'s `pnpm.allowBuilds` and
//! into `pnpm-workspace.yaml`. Entries are added as bare package names
//! so a future resolution of the same dep under a different version
//! keeps working without re-prompting.
//!
//! Existing projects with `pnpm.allowBuilds` in `package.json` still
//! have those entries honored at install time — the install-time
//! build policy reads both sources — so this change is a write-target
//! swap, not a read-side break.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::io::IsTerminal;

#[derive(Debug, Args)]
pub struct ApproveBuildsArgs {
    /// Approve every pending ignored build without prompting.
    #[arg(long)]
    pub all: bool,

    /// Operate on globally-installed packages instead of the current project.
    #[arg(short = 'g', long)]
    pub global: bool,

    /// Packages to approve directly, skipping the picker. Each name
    /// must match a currently-ignored build. Unknown names are rejected
    /// so a typo cannot silently no-op.
    #[arg(value_name = "PKG")]
    pub packages: Vec<String>,
}

pub async fn run(args: ApproveBuildsArgs) -> miette::Result<()> {
    if args.global {
        return Err(miette!(
            "`--global` is not yet implemented for `approve-builds`"
        ));
    }

    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;

    let ignored = super::ignored_builds::collect_ignored(&cwd)?;
    if ignored.is_empty() {
        println!("No ignored builds to approve.");
        return Ok(());
    }

    let selected: Vec<String> = if args.all {
        if !args.packages.is_empty() {
            return Err(miette!(
                "`--all` and positional package names are mutually exclusive"
            ));
        }
        ignored.iter().map(|e| e.name.clone()).collect()
    } else if !args.packages.is_empty() {
        let known: std::collections::HashSet<&str> =
            ignored.iter().map(|e| e.name.as_str()).collect();
        let unknown: Vec<&str> = args
            .packages
            .iter()
            .filter(|p| !known.contains(p.as_str()))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(miette!(
                "not in the ignored-builds set: {}. Run `aube ignored-builds` to see candidates.",
                unknown.join(", ")
            ));
        }
        args.packages
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(miette!(
                "approve-builds needs a TTY for the interactive picker; pass `--all` or name packages positionally to approve non-interactively"
            ));
        }
        pick_interactively(&ignored)?
    };

    if selected.is_empty() {
        println!("No packages selected.");
        return Ok(());
    }

    let written = aube_manifest::workspace::add_to_only_built_dependencies(&cwd, &selected)
        .into_diagnostic()
        .wrap_err("failed to update workspace yaml")?;

    let rel = written
        .strip_prefix(&cwd)
        .unwrap_or(written.as_path())
        .display();
    println!("Approved {} package(s) in {rel}:", selected.len());
    for name in &selected {
        println!("  {name}");
    }
    println!("Run `aube install` (or `aube rebuild`) to execute their scripts.");
    Ok(())
}

/// Show a `demand::MultiSelect` picker seeded with every ignored package
/// and return the names the user accepted. Using bare names (not
/// `name@version`) keeps the written allowBuilds entry broad, so the
/// next resolution with a patch-level bump doesn't silently drop back
/// into the ignored set.
fn pick_interactively(
    ignored: &[super::ignored_builds::IgnoredEntry],
) -> miette::Result<Vec<String>> {
    let mut picker = demand::MultiSelect::new("Choose which packages to allow building")
        .description("Space to toggle, Enter to confirm");
    for entry in ignored {
        let label = format!("{}@{}", entry.name, entry.version);
        picker = picker.option(demand::DemandOption::new(entry.name.clone()).label(&label));
    }
    picker
        .run()
        .into_diagnostic()
        .wrap_err("failed to read approve-builds selection")
}
