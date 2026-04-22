//! `aube deploy` — copy a workspace package into a standalone target
//! directory and install its production dependencies there.
//!
//! Mirrors `pnpm --filter=<name> deploy <target>`: we pick one workspace
//! package by name, copy the files it would publish (same selection as
//! `aube pack`), rewrite any `workspace:` protocol deps in its
//! `package.json` to the concrete versions of the matched workspace
//! siblings, then run a fresh `aube install` rooted at the target dir so
//! the result is a self-contained project.
//!
//! Implements the common monorepo-CI path:
//!
//!   * required `-F/--filter` (one or more pnpm-style selectors, shared
//!     with the global `-F` flag — exact names, `@scope/*` globs, path
//!     selectors, including dependency-graph selectors)
//!   * `--prod` (default), `--dev`, `--no-optional` forwarded to install
//!   * single-match fanout drops straight into `<target>`
//!   * multi-match fanout stages each match into
//!     `<target>/<source-dir-basename>/` and requires `<target>` itself
//!     to be empty/missing
//!
//! When the source workspace has a lockfile, deploy prunes it to the
//! deployed package's transitive closure and drops the subset into the
//! target before install runs — a `FrozenMode::Prefer` install then
//! reproduces the workspace's exact resolved versions without
//! re-fetching packuments. When there is no source lockfile, or the
//! deployed package has workspace-sibling deps (`link:` / `file:`
//! roots that can't resolve standalone), subsetting is skipped and
//! the original fresh-install path runs.
//!
//! Deferred: `--legacy`.

use crate::commands::install::{self, FrozenMode, InstallOptions};
use crate::commands::pack::build_archive;
use aube_manifest::PackageJson;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct DeployArgs {
    /// Target directory to deploy into.
    ///
    /// Must be empty or not yet exist.
    pub target: PathBuf,
    /// Install only `devDependencies`.
    ///
    /// Implemented by stripping `dependencies` and
    /// `optionalDependencies` from the deployed `package.json` before
    /// install runs.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,
    /// Skip `optionalDependencies`
    #[arg(long)]
    pub no_optional: bool,
    /// Install only production dependencies (default).
    ///
    /// Accepted for pnpm compatibility.
    // Intentionally unread by the deploy code: production is the deploy
    // default, so the `!args.dev` axis already captures it. Reach for
    // `!args.dev`, not `args.prod`, when extending the filter.
    #[arg(short = 'P', long, visible_alias = "production")]
    pub prod: bool,
}

pub async fn run(
    args: DeployArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if filter.is_empty() {
        return Err(miette!(
            "aube deploy: --filter/-F is required to pick a workspace package"
        ));
    }
    let source_root = crate::dirs::cwd().wrap_err("failed to read current directory")?;

    // Resolve `deployAllFiles` from the source workspace root, before
    // we chdir into any per-match target. `.npmrc` and
    // `pnpm-workspace.yaml` in the source tree are the source of
    // truth — the freshly-created target has neither yet.
    //
    // Use `load_raw` rather than `load_both`: settings resolution only
    // needs the raw YAML map, and `load_both` fails the whole call
    // (including the raw map) when any unrelated typed field
    // mismatches (e.g. `shamefullyHoist: "maybe"`). That would
    // silently drop `deployAllFiles: true`.
    let npmrc_entries = aube_registry::config::load_npmrc_entries(&source_root);
    let raw_workspace = aube_manifest::workspace::load_raw(&source_root).unwrap_or_default();
    let env = aube_settings::values::capture_env();
    let settings_ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env: &env,
        cli: &[],
    };
    let deploy_all_files = aube_settings::resolved::deploy_all_files(&settings_ctx);

    let workspace_pkgs = aube_workspace::find_workspace_packages(&source_root)
        .map_err(|e| miette!("failed to discover workspace packages: {e}"))?;
    if workspace_pkgs.is_empty() {
        return Err(miette!(
            "aube deploy: no workspace packages found. \
             `deploy` requires a workspace root (aube-workspace.yaml, pnpm-workspace.yaml, or package.json with a `workspaces` field) at {}",
            source_root.display()
        ));
    }

    // Build (name -> (path, version)) for every workspace package.
    let mut ws_index: BTreeMap<String, (PathBuf, String)> = BTreeMap::new();
    for dir in &workspace_pkgs {
        let Ok(m) = PackageJson::from_path(&dir.join("package.json")) else {
            continue;
        };
        if let (Some(n), Some(v)) = (m.name, m.version) {
            ws_index.insert(n, (dir.clone(), v));
        }
    }

    let selected =
        aube_workspace::selector::select_workspace_packages(&source_root, &workspace_pkgs, &filter)
            .map_err(|e| miette!("invalid --filter selector: {e}"))?;
    let mut matches: Vec<(String, PathBuf)> = selected
        .into_iter()
        .filter_map(|pkg| pkg.name.map(|name| (name, pkg.dir)))
        .filter(|(name, _)| ws_index.contains_key(name))
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0));

    if matches.is_empty() {
        let names: Vec<&str> = ws_index.keys().map(String::as_str).collect();
        return Err(miette!(
            "aube deploy: --filter {:?} did not match any workspace package. Known: {}",
            filter,
            names.join(", ")
        ));
    }

    // Resolve target root (relative to the source root — the in-process
    // single-match path chdir's into the target before install runs, so
    // any relative path resolved after that would be wrong).
    let target_root = if args.target.is_absolute() {
        args.target.clone()
    } else {
        source_root.join(&args.target)
    };

    // Work out the real target directory per match. Single match keeps
    // the pre-fanout layout: drop straight into `target_root`. Multi-
    // match requires `target_root` itself to be empty/missing and
    // writes one subdir per package named after the source workspace
    // folder (e.g. `packages/lib` → `<target>/lib`). Using the source
    // basename rather than the package name keeps scoped names
    // (`@test/lib`) out of the deploy path so we don't have to URL-
    // encode or collapse slashes.
    let plan: Vec<(String, PathBuf, PathBuf)> = if matches.len() == 1 {
        let (name, src) = matches.into_iter().next().unwrap();
        vec![(name, src, target_root.clone())]
    } else {
        ensure_target_writable(&target_root)?;
        let mut used: BTreeMap<String, String> = BTreeMap::new();
        let mut v = Vec::with_capacity(matches.len());
        for (name, src) in matches {
            let base = src
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    miette!(
                        "aube deploy: workspace package {} has no directory name",
                        src.display()
                    )
                })?;
            if let Some(prev) = used.insert(base.clone(), name.clone()) {
                return Err(miette!(
                    "aube deploy: workspace packages {prev:?} and {name:?} both live in a directory named {base:?}; \
                     multi-package deploy uses the source basename as the target subdir, so these would collide"
                ));
            }
            v.push((name, src, target_root.join(&base)));
        }
        v
    };

    // Stage every target (copy + manifest rewrite) up front. Running
    // staging for all matches before any install means a multi-package
    // fanout can't half-install one package and then fail on a copy
    // error in the next.
    let mut staged: Vec<StagedDeploy> = Vec::with_capacity(plan.len());
    for (_name, source_pkg_dir, target) in &plan {
        staged.push(stage_one(
            source_pkg_dir,
            target,
            &ws_index,
            &args,
            deploy_all_files,
        )?);
    }

    for (s, source_pkg_dir) in staged.iter().zip(plan.iter().map(|(_, src, _)| src)) {
        // Seed the target with a pruned copy of the source workspace
        // lockfile before chdir'ing into the target. Both the source
        // read and the target write use absolute paths, so ordering
        // with `retarget_cwd` doesn't matter for correctness — doing
        // it before keeps the side-effect timeline "stage → seed →
        // install" readable top-to-bottom. Returns `false` when we
        // fell back to a fresh install (no source lockfile, or the
        // importer had workspace-sibling deps we can't represent
        // standalone).
        let seeded = seed_target_lockfile(&source_root, source_pkg_dir, &s.target, &args)?;

        super::retarget_cwd(&s.target)?;

        // `no_optional` here is only the user flag — don't fold `--dev` in.
        // The `StripFields` in `stage_one` already dropped top-level
        // `optionalDependencies` from the manifest for `--dev`, which is
        // what pnpm does. Setting `InstallOptions.no_optional` on top of
        // that would also filter out *transitive* optional deps of
        // devDependencies (e.g. an optional sub-dep of `jest`), breaking
        // dev tooling at runtime.
        //
        // `mode`: when we seeded a subset lockfile, `Prefer` lets the
        // install reproduce the source workspace's pinned versions
        // without re-resolving against the registry. When we didn't,
        // fall back to `No` so install resolves from scratch — same as
        // the pre-subsetting behavior.
        let mode = if seeded {
            FrozenMode::Prefer
        } else {
            FrozenMode::No
        };
        let opts = InstallOptions {
            project_dir: Some(s.target.clone()),
            mode,
            dep_selection: install::DepSelection::from_flags(!args.dev, args.dev, args.no_optional),
            ignore_pnpmfile: false,
            ignore_scripts: false,
            lockfile_only: false,
            merge_git_branch_lockfiles: false,
            dangerously_allow_all_builds: false,
            network_mode: aube_registry::NetworkMode::Online,
            minimum_release_age_override: None,
            strict_no_lockfile: false,
            force: false,
            cli_flags: Vec::new(),
            env_snapshot: aube_settings::values::capture_env(),
            git_prepare_depth: 0,
            workspace_filter: aube_workspace::selector::EffectiveFilter::default(),
        };
        install::run(opts).await?;

        println!(
            "deployed {}@{} to {}",
            s.name,
            s.version,
            s.target.display()
        );
    }

    Ok(())
}

/// Staged per-package state: copy and manifest rewrite are complete,
/// but `aube install` hasn't run yet in `target`.
struct StagedDeploy {
    name: String,
    version: String,
    target: PathBuf,
}

/// Attempt to seed `target` with a subset of the source workspace's
/// lockfile, pruned to the deployed package's transitive closure.
/// Returns `true` iff a lockfile was written; `false` means we fell
/// back to the fresh-install path.
///
/// Fall-back (return `Ok(false)`) happens when:
///   * the source workspace has no lockfile (nothing to subset),
///   * the source lockfile can't be parsed,
///   * the deployed importer isn't in the source lockfile (stale
///     or never-installed workspace),
///   * any retained direct dep is backed by a local source (`link:`,
///     `file:` directory, or `file:` tarball). Workspace siblings and
///     local file deps can't resolve in a standalone target: the
///     sibling isn't published, and the local path would point
///     outside the deploy tree. Writing a subset lockfile that
///     references them would be strictly worse than letting the
///     fresh install surface the same resolution error at the right
///     layer.
///
/// The subset honors `--prod` / `--dev` / `--no-optional` the same
/// way `stage_one` rewrites the target manifest, so the two agree on
/// which dep fields survive — drift detection would otherwise fire.
/// Graph-wide metadata (`overrides`, `catalogs`,
/// `ignoredOptionalDependencies`) is cleared: the source resolver
/// already baked its effects into `packages:`, and keeping them in
/// the header would only trip drift against the target's minimal
/// package.json.
fn seed_target_lockfile(
    source_root: &Path,
    source_pkg_dir: &Path,
    target: &Path,
    args: &DeployArgs,
) -> miette::Result<bool> {
    // Source workspace root manifest is required by
    // `parse_lockfile_with_kind` (yarn.lock in particular needs the
    // manifest to classify direct vs transitive deps). A workspace
    // without a root `package.json` is unusual but not invalid, so
    // fall back rather than erroring.
    let Ok(source_manifest) = PackageJson::from_path(&source_root.join("package.json")) else {
        tracing::debug!("deploy: workspace root package.json unreadable, skipping lockfile subset");
        return Ok(false);
    };
    let (graph, kind) = match aube_lockfile::parse_lockfile_with_kind(source_root, &source_manifest)
    {
        Ok(pair) => pair,
        Err(e) => {
            tracing::debug!("deploy: no usable source lockfile ({e}); fresh install instead");
            return Ok(false);
        }
    };

    // Workspace-relative importer path ("." for root, "packages/lib"
    // for a sibling) — same shape pnpm writes into `importers:`
    // keys, which is what `subset_to_importer` indexes by.
    let importer_path = super::workspace_importer_path(source_root, source_pkg_dir)?;

    // Match `stage_one`'s `StripFields` semantics for which dep
    // types survive the manifest rewrite. `--dev` also strips
    // optional deps on the manifest side; mirror that here so the
    // lockfile and manifest agree.
    let prod = !args.dev;
    let dev = args.dev;
    let keep_optional = !(args.no_optional || args.dev);
    let keep = move |d: &aube_lockfile::DirectDep| match d.dep_type {
        aube_lockfile::DepType::Production => prod,
        aube_lockfile::DepType::Dev => dev,
        aube_lockfile::DepType::Optional => keep_optional,
    };
    let Some(mut subset) = graph.subset_to_importer(&importer_path, keep) else {
        tracing::debug!(
            "deploy: importer {importer_path:?} not in source lockfile; fresh install instead"
        );
        return Ok(false);
    };

    // Any retained direct dep backed by a local source is a dead
    // end for a standalone target. See the function doc for the
    // reasoning — short version: the sibling isn't published and
    // the `link:` / `file:` path points outside the deploy tree.
    let has_local_root = subset.root_deps().iter().any(|d| {
        subset
            .get_package(&d.dep_path)
            .and_then(|p| p.local_source.as_ref())
            .is_some_and(|src| {
                matches!(
                    src,
                    aube_lockfile::LocalSource::Link(_)
                        | aube_lockfile::LocalSource::Directory(_)
                        | aube_lockfile::LocalSource::Tarball(_)
                )
            })
    });
    if has_local_root {
        tracing::debug!("deploy: source importer has link:/file: roots; fresh install instead");
        return Ok(false);
    }

    // Drop workspace-scope metadata the target can't honor. Their
    // effects already live in `packages:` (the resolver baked them
    // in), so keeping them here would only trip drift detection
    // against the target's minimal package.json — which has no
    // `pnpm.overrides`, no `catalog:` refs, no
    // `pnpm.ignoredOptionalDependencies`.
    subset.overrides.clear();
    subset.ignored_optional_dependencies.clear();
    subset.catalogs.clear();

    // Prune `times` to match the subset's `packages`. `times` isn't
    // part of drift detection, so keeping the source workspace's
    // full `time:` map doesn't break `FrozenMode::Prefer`, but it
    // bloats the target lockfile with timestamps for every package
    // the source workspace ever resolved — including the ones we
    // just pruned from the closure.
    //
    // `times` is keyed by the canonical `name@version` (no peer
    // suffix) while `subset.packages` is keyed by the full dep_path
    // (which can carry a `(peer@ver)` suffix), so a direct
    // `contains_key` check against `packages` would silently drop
    // timestamps for any package resolved with a peer context.
    // Build the canonical key set from `LockedPackage.name` /
    // `.version` and filter against that.
    let canonical_keys: std::collections::HashSet<String> =
        subset.packages.values().map(|pkg| pkg.spec_key()).collect();
    subset.times.retain(|key, _| canonical_keys.contains(key));

    // Re-read the rewritten target manifest. The writer uses `name`
    // / `version` / direct-dep specifiers to stamp the lockfile
    // header correctly; using the source workspace root manifest
    // would fill in the wrong name for the deployed package.
    let target_manifest = PackageJson::from_path(&target.join("package.json"))
        .map_err(miette::Report::new)
        .wrap_err("deploy: failed to re-read rewritten target package.json")?;

    aube_lockfile::write_lockfile_as(target, &subset, &target_manifest, kind)
        .into_diagnostic()
        .wrap_err("deploy: failed to write subset lockfile into target")?;
    Ok(true)
}

/// Copy files into `target` (either pack's publish-selection or the
/// whole source tree, depending on `deploy_all_files`) and rewrite the
/// deployed `package.json` (strip excluded dep fields, inline
/// `workspace:` deps). Returns enough state for the caller to drive
/// install.
fn stage_one(
    source_pkg_dir: &Path,
    target: &Path,
    ws_index: &BTreeMap<String, (PathBuf, String)>,
    args: &DeployArgs,
    deploy_all_files: bool,
) -> miette::Result<StagedDeploy> {
    ensure_target_writable(target)?;
    std::fs::create_dir_all(target)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to create {}", target.display()))?;

    let (name, version, files) = if deploy_all_files {
        // `deployAllFiles=true`: ignore pack's selection entirely and
        // copy every file in the source tree (minus `node_modules/`,
        // `.git/`, and the target itself when nested). Read the
        // manifest directly to reuse `name`/`version` for the final
        // println without building a throwaway tarball.
        let manifest = super::load_manifest(&source_pkg_dir.join("package.json"))?;
        let name = manifest
            .name
            .ok_or_else(|| miette!("deploy: package.json has no `name` field"))?;
        let version = manifest
            .version
            .ok_or_else(|| miette!("deploy: package.json has no `version` field"))?;
        let files = collect_all_files(source_pkg_dir, target)?;
        (name, version, files)
    } else {
        // Default: reuse pack's file selection so deploy ships exactly
        // what publish would. Throws away the tarball bytes — only
        // `files` is load-bearing — but building it in memory is cheap
        // and keeps the logic single-source.
        let archive = build_archive(source_pkg_dir)?;
        let files = archive
            .files
            .into_iter()
            .map(|rel| (source_pkg_dir.join(&rel), rel))
            .collect();
        (archive.name, archive.version, files)
    };

    for (src, rel) in &files {
        let dst = target.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::copy(src, &dst)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to copy {} -> {}", src.display(), dst.display()))?;
    }

    // Rewrite package.json: strip `workspace:` prefixes, resolving them
    // to the matched sibling's concrete version while preserving any
    // range operator (`^`, `~`, literal). Unknown workspace: refs are a
    // hard error — they'd fail the subsequent install anyway, and
    // erroring here produces a clearer message.
    //
    // We also physically strip the dep fields that this deploy excludes
    // *before* running install. That's load-bearing, not a convenience:
    // install's resolver walks every dep type in the manifest up front
    // and only the linker applies `--prod` / `--no-optional` filtering,
    // so leaving e.g. a devDependency with an unpublished `workspace:`
    // ref in the manifest would make `--prod` deploys fail resolution
    // on a package that would never have been installed.
    let strip = StripFields {
        dependencies: args.dev,
        dev_dependencies: !args.dev,
        optional_dependencies: args.no_optional || args.dev,
    };
    rewrite_workspace_deps(&target.join("package.json"), ws_index, strip)?;

    Ok(StagedDeploy {
        name,
        version,
        target: target.to_path_buf(),
    })
}

/// Walk `source` recursively and collect every file path. Skips only
/// the filesystem cruft that could never be part of a package payload
/// (`node_modules/`, `.git/`) and the `target` directory itself when
/// it sits inside `source`. Unlike pack's selection, this path keeps
/// dot-files, test fixtures, and anything the `files` field /
/// `.npmignore` would have filtered — which is the whole point of
/// `deployAllFiles=true`.
fn collect_all_files(source: &Path, target: &Path) -> miette::Result<Vec<(PathBuf, String)>> {
    // Canonicalize both sides so the "is this entry the target dir?"
    // check survives `./foo` vs absolute-path spellings. `target`
    // always exists here (ensure_target_writable + create_dir_all
    // already ran), so canonicalize is not expected to fail; fall
    // back to the raw path rather than aborting the deploy.
    let target_canon = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    let mut out = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let iter = std::fs::read_dir(&dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("deploy: read_dir({}) failed", dir.display()))?;
        for entry in iter {
            let entry = entry
                .into_diagnostic()
                .wrap_err_with(|| format!("deploy: failed to read entry in {}", dir.display()))?;
            let name = entry.file_name();
            if matches!(name.to_string_lossy().as_ref(), "node_modules" | ".git") {
                continue;
            }
            let path = entry.path();
            let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            if canon == target_canon {
                continue;
            }
            // `file_type()` is `lstat`, so a symlink-to-file answers
            // `false` to both `is_file()` and `is_dir()`. Follow one
            // level via `metadata()` (which is `stat`) so symlinked
            // files are copied verbatim — packages that ship linked
            // executables or assets would otherwise lose content
            // under `deployAllFiles=true`. Directory symlinks stay
            // excluded: recursing through them risks cycles
            // (e.g. `src/self -> src/`) and pulls in trees outside
            // the package, which is strictly worse than the pack
            // default. `std::fs::copy` follows links, so the
            // destination gets the target's bytes, not another
            // symlink — matches what a user typing `cp -L` expects.
            let ft = entry
                .file_type()
                .into_diagnostic()
                .wrap_err_with(|| format!("deploy: failed to stat {}", path.display()))?;
            let (is_dir, is_file) = if ft.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(md) => (md.is_dir(), md.is_file()),
                    // Broken link (dangling target). Skip rather
                    // than error — the source package owns it and a
                    // broken link is almost certainly not part of
                    // the intended payload.
                    Err(_) => (false, false),
                }
            } else {
                (ft.is_dir(), ft.is_file())
            };
            if is_dir && !ft.is_symlink() {
                stack.push(path);
            } else if is_file && let Ok(rel) = path.strip_prefix(source) {
                out.push((path.clone(), rel.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    Ok(out)
}

/// Error if the target already holds files. An empty existing directory
/// is fine — useful when CI pre-creates the mount point.
fn ensure_target_writable(target: &Path) -> miette::Result<()> {
    match std::fs::read_dir(target) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(miette!(
                    "aube deploy: target directory {} is not empty",
                    target.display()
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(miette!(
            "aube deploy: failed to inspect {}: {e}",
            target.display()
        )),
    }
}

/// Which dep fields `rewrite_workspace_deps` should physically remove
/// from the deployed `package.json` before install runs.
#[derive(Debug, Clone, Copy, Default)]
struct StripFields {
    dependencies: bool,
    dev_dependencies: bool,
    optional_dependencies: bool,
}

/// Walk `dependencies` / `devDependencies` / `optionalDependencies` /
/// `peerDependencies` in the target package.json and resolve every
/// `workspace:` specifier against the workspace index, preserving the
/// range operator per pnpm semantics:
///
///   * `workspace:*`        → `<version>` (exact pin)
///   * `workspace:^`        → `^<version>`
///   * `workspace:~`        → `~<version>`
///   * `workspace:<range>`  → `<range>` (literal suffix wins; `<range>`
///     already carries its own operator, e.g. `^1.2.3`, `>=2`, `1.2.3`)
///
/// Anything that isn't `workspace:` is left untouched. `strip` names
/// any dep fields the caller wants physically removed before install
/// runs — load-bearing for `--prod` / `--dev` / `--no-optional`, since
/// install's resolver walks the full manifest before the linker
/// applies filtering, so an unstripped workspace: dep in an excluded
/// field would still be fetched.
fn rewrite_workspace_deps(
    manifest_path: &Path,
    ws_index: &BTreeMap<String, (PathBuf, String)>,
    strip: StripFields,
) -> miette::Result<()> {
    let raw = std::fs::read_to_string(manifest_path)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", manifest_path.display()))?;

    const DEP_FIELDS: &[&str] = &[
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ];
    let Some(obj) = doc.as_object_mut() else {
        return Err(miette!(
            "{} did not parse to a JSON object",
            manifest_path.display()
        ));
    };
    if strip.dependencies {
        obj.remove("dependencies");
    }
    if strip.dev_dependencies {
        obj.remove("devDependencies");
    }
    if strip.optional_dependencies {
        obj.remove("optionalDependencies");
    }
    for field in DEP_FIELDS {
        let Some(deps) = obj.get_mut(*field).and_then(|v| v.as_object_mut()) else {
            continue;
        };
        for (name, spec_val) in deps.iter_mut() {
            let Some(spec) = spec_val.as_str() else {
                continue;
            };
            if !spec.starts_with("workspace:") {
                continue;
            }
            let (_, concrete_version) = ws_index.get(name).ok_or_else(|| {
                miette!(
                    "aube deploy: {} declares `{name}: {spec}` but no workspace package named {name:?} was found",
                    manifest_path.display()
                )
            })?;
            *spec_val = serde_json::Value::String(resolve_workspace_spec(spec, concrete_version));
        }
    }

    let rewritten = serde_json::to_string_pretty(&doc)
        .into_diagnostic()
        .wrap_err("failed to serialize rewritten package.json")?;
    std::fs::write(manifest_path, rewritten)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

/// Resolve a `workspace:...` specifier against the sibling's concrete
/// version, preserving the range operator. See `rewrite_workspace_deps`
/// for the full mapping table.
fn resolve_workspace_spec(spec: &str, concrete_version: &str) -> String {
    let suffix = spec.strip_prefix("workspace:").unwrap_or(spec);
    match suffix {
        "" | "*" => concrete_version.to_string(),
        "^" => format!("^{concrete_version}"),
        "~" => format!("~{concrete_version}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_index(entries: &[(&str, &str)]) -> BTreeMap<String, (PathBuf, String)> {
        entries
            .iter()
            .map(|(n, v)| ((*n).to_string(), (PathBuf::from("/tmp"), (*v).to_string())))
            .collect()
    }

    #[test]
    fn resolve_workspace_spec_star_pins_exact() {
        assert_eq!(resolve_workspace_spec("workspace:*", "1.2.3"), "1.2.3");
        assert_eq!(resolve_workspace_spec("workspace:", "1.2.3"), "1.2.3");
    }

    #[test]
    fn resolve_workspace_spec_caret_and_tilde_preserve_operator() {
        assert_eq!(resolve_workspace_spec("workspace:^", "1.2.3"), "^1.2.3");
        assert_eq!(resolve_workspace_spec("workspace:~", "1.2.3"), "~1.2.3");
    }

    #[test]
    fn resolve_workspace_spec_literal_suffix_wins() {
        // Explicit range after `workspace:` is used verbatim — it already
        // carries its own operator.
        assert_eq!(
            resolve_workspace_spec("workspace:^2.0.0", "1.2.3"),
            "^2.0.0"
        );
        assert_eq!(resolve_workspace_spec("workspace:1.2.3", "9.9.9"), "1.2.3");
        assert_eq!(resolve_workspace_spec("workspace:>=2", "1.2.3"), ">=2");
    }

    #[test]
    fn rewrite_replaces_workspace_star_with_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/lib":"workspace:*","lodash":"^4"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[("@test/lib", "1.2.3")]);
        rewrite_workspace_deps(&path, &idx, StripFields::default()).unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["@test/lib"], "1.2.3");
        assert_eq!(out["dependencies"]["lodash"], "^4");
    }

    #[test]
    fn rewrite_preserves_caret_and_tilde_range_operators() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@a/lib":"workspace:^","@b/lib":"workspace:~"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[("@a/lib", "1.2.3"), ("@b/lib", "4.5.6")]);
        rewrite_workspace_deps(&path, &idx, StripFields::default()).unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["@a/lib"], "^1.2.3");
        assert_eq!(out["dependencies"]["@b/lib"], "~4.5.6");
    }

    #[test]
    fn rewrite_dev_only_drops_non_dev_dep_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"lodash":"^4"},"optionalDependencies":{"fsevents":"^2"},"devDependencies":{"jest":"^29"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[]);
        rewrite_workspace_deps(
            &path,
            &idx,
            StripFields {
                dependencies: true,
                dev_dependencies: false,
                optional_dependencies: true,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(out.get("dependencies").is_none());
        assert!(out.get("optionalDependencies").is_none());
        assert_eq!(out["devDependencies"]["jest"], "^29");
    }

    #[test]
    fn rewrite_prod_mode_drops_dev_dependencies() {
        // --prod default: devDependencies must be physically removed
        // from the manifest, not just filtered at link time. Install's
        // resolver walks every dep type before filtering, so an
        // unpublished `workspace:` devDep would otherwise fail the
        // whole deploy.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"lodash":"^4"},"devDependencies":{"@test/internal":"workspace:*"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[]); // unpublished devDep — deliberately absent
        rewrite_workspace_deps(
            &path,
            &idx,
            StripFields {
                dependencies: false,
                dev_dependencies: true,
                optional_dependencies: false,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(out.get("devDependencies").is_none());
        assert_eq!(out["dependencies"]["lodash"], "^4");
    }

    #[test]
    fn rewrite_errors_on_unknown_workspace_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/missing":"workspace:*"}}"#,
        )
        .unwrap();
        let idx = ws_index(&[]);
        let err = rewrite_workspace_deps(&path, &idx, StripFields::default()).unwrap_err();
        assert!(err.to_string().contains("@test/missing"));
    }

    #[test]
    fn ensure_target_writable_empty_dir_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_target_writable(tmp.path()).unwrap();
    }

    #[test]
    fn ensure_target_writable_missing_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_target_writable(&tmp.path().join("nope")).unwrap();
    }

    #[test]
    fn ensure_target_writable_nonempty_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stuff"), "hi").unwrap();
        assert!(ensure_target_writable(tmp.path()).is_err());
    }
}
