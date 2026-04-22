//! `aube ignored-builds` — print packages whose lifecycle scripts were
//! skipped by the `pnpm.allowBuilds` allowlist.
//!
//! Walks the lockfile, reads each dep's stored `package.json` from the
//! global store, and reports any package that declares a
//! `preinstall` / `install` / `postinstall` script but isn't explicitly
//! allowed by the current `BuildPolicy`. Shared with `approve-builds`,
//! which re-uses [`collect_ignored`] to drive its interactive picker.
//!
//! Pure read — no network, no writes, no project lock.

use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeSet;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube ignored-builds
  The following builds were ignored during install:
    esbuild@0.20.2
    puppeteer@22.8.0

  # When nothing was skipped
  $ aube ignored-builds
  No ignored builds.

  # Approve them for this project
  $ aube approve-builds
";

#[derive(Debug, Args)]
pub struct IgnoredBuildsArgs {
    /// Operate on globally-installed packages instead of the current project.
    #[arg(short = 'g', long)]
    pub global: bool,
}

pub async fn run(args: IgnoredBuildsArgs) -> miette::Result<()> {
    if args.global {
        return Err(miette!(
            "`--global` is not yet implemented for `ignored-builds`"
        ));
    }

    let cwd = crate::dirs::project_root()?;
    let ignored = collect_ignored(&cwd)?;

    if ignored.is_empty() {
        println!("No ignored builds.");
        return Ok(());
    }

    println!("The following builds were ignored during install:");
    for entry in &ignored {
        println!("  {}@{}", entry.name, entry.version);
    }
    Ok(())
}

/// One package whose lifecycle scripts were skipped because it was not
/// allowed by the current `BuildPolicy`. `name` is the pnpm package name,
/// `version` is the resolved version from the lockfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct IgnoredEntry {
    pub name: String,
    pub version: String,
}

impl std::cmp::Ord for IgnoredEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.version.cmp(&other.version))
    }
}

impl std::cmp::PartialOrd for IgnoredEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Load the lockfile and build policy for `project_dir`, then return the
/// sorted, deduplicated list of `(name, version)` pairs that declare a
/// dep-lifecycle hook and are not allowed by the policy.
///
/// Returns an empty list (not an error) if there is no lockfile yet —
/// callers print their own "nothing to do" message.
pub(super) fn collect_ignored(project_dir: &std::path::Path) -> miette::Result<Vec<IgnoredEntry>> {
    let manifest = super::load_manifest(&project_dir.join("package.json"))?;

    let graph = match aube_lockfile::parse_lockfile(project_dir, &manifest) {
        Ok(g) => g,
        Err(aube_lockfile::Error::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(miette::Report::new(e)).wrap_err("failed to parse lockfile"),
    };

    let workspace = aube_manifest::WorkspaceConfig::load(project_dir)
        .into_diagnostic()
        .wrap_err("failed to load workspace config")?;
    let (policy, _warnings) =
        super::install::build_policy_from_sources(&manifest, &workspace, false);

    let store = super::open_store(project_dir)?;

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut out: Vec<IgnoredEntry> = Vec::new();

    for pkg in graph.packages.values() {
        if !seen.insert((pkg.name.clone(), pkg.version.clone())) {
            continue;
        }
        // Match on registry_name, not pkg.name. Allowlist pins the
        // real pkg name. npm: alias would sneak past otherwise. Same
        // fix as every other policy.decide callsite.
        if matches!(
            policy.decide(pkg.registry_name(), &pkg.version),
            aube_scripts::AllowDecision::Allow
        ) {
            continue;
        }
        if !has_lifecycle_scripts(&store, &pkg.name, &pkg.version, pkg.integrity.as_deref()) {
            continue;
        }
        out.push(IgnoredEntry {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        });
    }

    out.sort();
    Ok(out)
}

/// Read `<name>@<version>`'s stored `package.json` from the global store
/// index and return true when any dep-lifecycle script (preinstall /
/// install / postinstall) is declared — or when the package ships a
/// top-level `binding.gyp` with no install/preinstall script, which
/// means the install pipeline would have fallen back to the implicit
/// `node-gyp rebuild` default.
///
/// Missing / unreadable manifests conservatively return `false` — the
/// package might have scripts we can't see, but reporting them as
/// "ignored" would be noise since the install pipeline also skipped
/// them for the same reason.
fn has_lifecycle_scripts(
    store: &aube_store::Store,
    name: &str,
    version: &str,
    integrity: Option<&str>,
) -> bool {
    // Cache lookup is integrity-keyed to prevent same-(name, version)
    // entries from different sources colliding. Missing integrity
    // means we have no cache key, so conservatively report false —
    // matches the behavior for "we couldn't read the manifest".
    let Some(index) = integrity.and_then(|i| store.load_index(name, version, i)) else {
        return false;
    };
    let Some(stored) = index.get("package.json") else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&stored.store_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<aube_manifest::PackageJson>(&content) else {
        return false;
    };
    if aube_scripts::DEP_LIFECYCLE_HOOKS
        .iter()
        .any(|h| manifest.scripts.contains_key(h.script_name()))
    {
        return true;
    }
    // Delegate the implicit-rebuild gate to `aube-scripts` so this
    // stays in lockstep with what the install pipeline actually runs.
    // Presence comes from the store index here (the package isn't
    // materialized yet at this point in the command), but the
    // condition itself lives in exactly one place.
    aube_scripts::implicit_install_script(&manifest, index.contains_key("binding.gyp")).is_some()
}
