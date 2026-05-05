//! `aube deploy` — copy a workspace package into a standalone target
//! directory and install its dependencies there.
//!
//! Mirrors `pnpm --filter=<name> deploy <target>`: we pick one workspace
//! package by name, copy the files it would publish (same selection as
//! `aube pack`), bundle any `workspace:` / `file:` / `link:` deps the
//! deployed package reaches into a staging dir under the target, rewrite
//! the deployed `package.json` to point at the bundled copies, then run a
//! fresh `aube install` rooted at the target dir so the result is a
//! self-contained project — siblings included, no registry round-trip.
//!
//! Implements the common monorepo-CI path:
//!
//!   * required `-F/--filter` (one or more pnpm-style selectors, shared
//!     with the global `-F` flag — exact names, `@scope/*` globs, path
//!     selectors, including dependency-graph selectors)
//!   * `--prod` (default), `--dev`, `--no-prod` (deploy every dep kind),
//!     `--no-optional` forwarded to install and to the manifest rewrite
//!   * single-match fanout drops straight into `<target>`
//!   * multi-match fanout stages each match into
//!     `<target>/<source-dir-basename>/` and requires `<target>` itself
//!     to be empty/missing
//!
//! Workspace siblings + `file:`/`link:` dep targets reachable from the
//! deployed package land at `<target>/.aube-deploy-injected/<id>/`. The
//! deployed manifest (and any nested bundled manifest) gets its
//! `workspace:` / `file:` / `link:` specs rewritten to relative `file:`
//! pointers at those staged copies, so install resolves them as plain
//! local-directory deps. Recursion handles siblings whose own deps are
//! workspace siblings.
//!
//! When the source workspace has a lockfile and no bundling was needed,
//! deploy prunes that lockfile to the deployed package's transitive
//! closure and drops the subset into the target before install runs — a
//! `FrozenMode::Prefer` install then reproduces the workspace's exact
//! resolved versions without re-fetching packuments. When bundling
//! happened, when there is no source lockfile, or the deployed package
//! has workspace-sibling / `link:` / `file:` roots whose rewritten form
//! diverges from the source lockfile, subsetting is skipped and a fresh
//! install runs.
//!
//! Deferred: `--legacy`.

use crate::commands::install::{self, FrozenMode, InstallOptions};
use crate::commands::pack::{build_archive, collect_package_files};
use aube_manifest::PackageJson;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, VecDeque};
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
    #[arg(short = 'D', long, conflicts_with_all = ["prod", "no_prod"])]
    pub dev: bool,
    /// Skip `optionalDependencies`
    #[arg(long)]
    pub no_optional: bool,
    /// Install only production dependencies (default).
    ///
    /// Accepted for pnpm compatibility.
    // Intentionally unread by the deploy code: production is the deploy
    // default, so the `!args.dev && !args.no_prod` axis already captures
    // it. Reach for that, not `args.prod`, when extending the filter.
    #[arg(short = 'P', long, visible_alias = "production")]
    pub prod: bool,
    /// Deploy every dependency kind (production + dev + optional).
    ///
    /// Opts out of the implicit `--prod` deploy default. Useful when a
    /// deployed package needs its devDependencies at runtime (test
    /// harnesses, build-step deploys). Combine with `--no-optional` to
    /// drop optionals while keeping prod + dev. Mutually exclusive with
    /// `--prod` and `--dev`.
    #[arg(long, conflicts_with_all = ["prod", "dev"])]
    pub no_prod: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

pub async fn run(
    args: DeployArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
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
    let env = aube_settings::values::process_env();
    let settings_ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env,
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
        // fell back to a fresh install (no source lockfile, the
        // importer had local roots we couldn't seed, or staging
        // bundled local refs in a way that diverges from the source
        // lockfile).
        let seeded = if s.bundled_local_refs {
            tracing::debug!(
                "deploy: bundled local refs into {}; skipping lockfile subset",
                s.target.display()
            );
            false
        } else {
            seed_target_lockfile(&source_root, source_pkg_dir, &s.target, &args)?
        };

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
            dep_selection: dep_selection_for_args(&args),
            ignore_pnpmfile: false,
            pnpmfile: None,
            global_pnpmfile: None,
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
            skip_root_lifecycle: false,
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
    /// Whether staging bundled any local refs (workspace siblings,
    /// `file:` / `link:` targets) into `<target>/.aube-deploy-injected/`.
    /// When set, the source lockfile subset must be skipped — the
    /// rewritten manifest's `file:` pointers don't appear in the source
    /// lockfile, so a frozen install would immediately read as drifted.
    bundled_local_refs: bool,
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

    let Some(mut subset) = graph.subset_to_importer(&importer_path, keep_dep_for_args(args)) else {
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
/// whole source tree, depending on `deploy_all_files`), bundle any
/// local-ref deps the deployed package reaches into
/// `<target>/.aube-deploy-injected/<id>/`, and rewrite each manifest's
/// `workspace:` / `file:` / `link:` specs so install resolves them to
/// the bundled copies. Returns enough state for the caller to drive
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

    // Plan + materialize bundled local refs, then rewrite each manifest
    // (top-level + every bundled sibling) to point at the staged
    // copies. We strip excluded dep fields *before* install runs:
    // install's resolver walks every dep type in the manifest up front
    // and only the linker applies `--prod` / `--no-optional` filtering,
    // so leaving e.g. a devDependency with an unpublished `workspace:`
    // ref in the manifest would make `--prod` deploys fail resolution
    // on a package that would never have been installed.
    let plan = plan_injections(source_pkg_dir, target, ws_index, args)?;
    materialize_injections(&plan, ws_index, deploy_all_files)?;
    let deployed_canonical = canonicalize(source_pkg_dir);
    let root = DeployRoot {
        deployed_canonical: &deployed_canonical,
        target_root: target,
    };
    rewrite_local_refs(
        &target.join("package.json"),
        source_pkg_dir,
        target,
        ws_index,
        &plan,
        StripFields::for_args(args),
        root,
    )?;
    let bundled_strip = StripFields::for_bundled_sibling(args);
    for inj in plan.values() {
        // Tarballs ship as opaque archives — there's no extracted
        // `package.json` under their `target_dir` to rewrite, and no
        // way to recurse into one anyway since the sibling pipeline
        // doesn't unpack archives.
        if inj.is_tarball {
            continue;
        }
        rewrite_local_refs(
            &inj.target_dir.join("package.json"),
            &inj.source_dir,
            &inj.target_dir,
            ws_index,
            &plan,
            bundled_strip,
            root,
        )?;
    }

    Ok(StagedDeploy {
        name,
        version,
        target: target.to_path_buf(),
        bundled_local_refs: !plan.is_empty(),
    })
}

/// Where a bundled local ref ends up under
/// `<target>/.aube-deploy-injected/`. Distinct sources with distinct
/// canonical paths each get their own entry — siblings shared between
/// multiple parents bundle once.
#[derive(Debug, Clone)]
struct Injection {
    /// Source directory (workspace sibling root or `file:` directory)
    /// or tarball path on disk. Reads come from here.
    source_dir: PathBuf,
    /// Set when `source_dir` is actually a tarball file (`*.tgz` /
    /// `*.tar.gz`) rather than a directory. Materialization copies the
    /// tarball verbatim and the rewriter emits `file:` pointers at the
    /// staged tarball.
    is_tarball: bool,
    /// Absolute path inside the deploy target where the bundled copy
    /// lives. For directory sources this is the staged package root;
    /// for tarball sources this is the directory holding the tarball.
    target_dir: PathBuf,
    /// For tarball sources: filename under `target_dir`. Empty for
    /// directory sources.
    tarball_filename: String,
}

/// Map keyed by the canonical absolute source path — that gives us
/// stable identity across multiple rewriters that find the same local
/// ref via different relative specs (e.g. `file:../foo` from two
/// different consumer manifests).
type InjectionPlan = BTreeMap<PathBuf, Injection>;

/// BFS the deployed package's manifest plus every bundled sibling's
/// manifest, recording one [`Injection`] per distinct local-ref target.
/// The returned map preserves insertion order via canonical path keys —
/// callers iterate it to materialize copies and rewrite manifests in any
/// order they like.
fn plan_injections(
    deployed_pkg_dir: &Path,
    target_root: &Path,
    ws_index: &BTreeMap<String, (PathBuf, String)>,
    args: &DeployArgs,
) -> miette::Result<InjectionPlan> {
    let injected_root = target_root.join(".aube-deploy-injected");
    let mut plan: InjectionPlan = BTreeMap::new();
    // Track id collisions so a second sibling with the same encoded
    // name gets a `_2`, `_3`, ... suffix. Keyed by the encoded id.
    let mut used_ids: BTreeMap<String, u32> = BTreeMap::new();
    // Don't bundle the deployed package itself: a sibling B with a
    // back-dep `"@deployed-pkg": "workspace:*"` would otherwise duplicate
    // the deploy root under `.aube-deploy-injected/` and break runtime
    // singleton assumptions (two distinct module instances). The
    // rewriter handles back-refs separately.
    let deployed_canonical = canonicalize(deployed_pkg_dir);

    // BFS frontier: each entry is `(source_dir, strip)`. The first
    // entry is the deployed package; everything queued after is a
    // bundled sibling, which uses the bundled-sibling strip policy.
    let mut queue: VecDeque<(PathBuf, StripFields)> = VecDeque::new();
    queue.push_back((deployed_pkg_dir.to_path_buf(), StripFields::for_args(args)));

    while let Some((pkg_dir, strip)) = queue.pop_front() {
        let manifest_path = pkg_dir.join("package.json");
        let manifest = super::load_manifest(&manifest_path)?;

        for (dep_name, dep_spec) in iter_strippable_deps(&manifest, strip) {
            // Workspace sibling refs win over file:/link: parsing —
            // a workspace sibling can also be referenced via `link:`
            // pointing at its dir, but the workspace index is the
            // authoritative match.
            if aube_util::pkg::is_workspace_spec(&dep_spec) {
                let Some((sibling_dir, _)) = ws_index.get(&dep_name) else {
                    return Err(miette!(
                        "aube deploy: {} declares `{dep_name}: {dep_spec}` but no workspace package named {dep_name:?} was found",
                        manifest_path.display()
                    ));
                };
                let canonical = canonicalize(sibling_dir);
                if canonical == deployed_canonical {
                    continue;
                }
                if !plan.contains_key(&canonical) {
                    let id = unique_id(&dep_name, &mut used_ids);
                    plan.insert(
                        canonical.clone(),
                        Injection {
                            source_dir: canonical.clone(),
                            is_tarball: false,
                            target_dir: injected_root.join(&id),
                            tarball_filename: String::new(),
                        },
                    );
                    queue.push_back((canonical, StripFields::for_bundled_sibling(args)));
                }
            } else if let Some(local) = aube_lockfile::LocalSource::parse(&dep_spec, &pkg_dir) {
                match local {
                    aube_lockfile::LocalSource::Directory(rel)
                    | aube_lockfile::LocalSource::Link(rel) => {
                        let abs = pkg_dir.join(&rel);
                        let canonical = canonicalize(&abs);
                        // Same back-ref guard as the `workspace:` branch:
                        // a bundled sibling reaching the deployed package
                        // via `file:../deployed-pkg` must not duplicate it.
                        if canonical == deployed_canonical {
                            continue;
                        }
                        if !plan.contains_key(&canonical) {
                            let id_seed = canonical
                                .file_name()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&dep_name);
                            let id = unique_id(id_seed, &mut used_ids);
                            plan.insert(
                                canonical.clone(),
                                Injection {
                                    source_dir: canonical.clone(),
                                    is_tarball: false,
                                    target_dir: injected_root.join(&id),
                                    tarball_filename: String::new(),
                                },
                            );
                            // Recurse: a bundled `file:` directory may
                            // itself reach further siblings or `file:`
                            // targets. Tarballs don't recurse — they
                            // ship as opaque archives.
                            queue.push_back((
                                canonical.clone(),
                                StripFields::for_bundled_sibling(args),
                            ));
                        }
                    }
                    aube_lockfile::LocalSource::Tarball(rel) => {
                        let abs = pkg_dir.join(&rel);
                        let canonical = canonicalize(&abs);
                        if !plan.contains_key(&canonical) {
                            let stem = canonical
                                .file_stem()
                                .and_then(|s| s.to_str())
                                .unwrap_or(&dep_name);
                            let id = unique_id(stem, &mut used_ids);
                            let filename = canonical
                                .file_name()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| format!("{stem}.tgz"));
                            plan.insert(
                                canonical.clone(),
                                Injection {
                                    source_dir: canonical.clone(),
                                    is_tarball: true,
                                    target_dir: injected_root.join(&id),
                                    tarball_filename: filename,
                                },
                            );
                        }
                    }
                    // Git / RemoteTarball: install fetches these
                    // standalone from their source — the deploy target
                    // doesn't need a bundled copy.
                    aube_lockfile::LocalSource::Git(_)
                    | aube_lockfile::LocalSource::RemoteTarball(_) => {}
                }
            }
        }
    }

    Ok(plan)
}

/// Iterate the three bundleable dep fields, skipping any field the
/// strip policy will drop. Yields `(name, spec)` pairs the rewriter
/// will keep — the bundling planner only needs to see deps that
/// survive the strip, otherwise it would copy a sibling that the
/// deployed manifest is about to discard. `peerDependencies` is
/// intentionally omitted: peers are satisfied by the consumer's
/// installed tree, not bundled into the deploy.
fn iter_strippable_deps(manifest: &PackageJson, strip: StripFields) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if !strip.dependencies {
        for (k, v) in &manifest.dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    if !strip.dev_dependencies {
        for (k, v) in &manifest.dev_dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    if !strip.optional_dependencies {
        for (k, v) in &manifest.optional_dependencies {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// Pick a filesystem-safe id under `.aube-deploy-injected/`. Starts
/// from `seed` (with `/` and any other unsafe characters sanitized) and
/// disambiguates collisions with `_2`, `_3`, ... — collisions are rare
/// and the suffix keeps the staged path readable when debugging.
fn unique_id(seed: &str, used: &mut BTreeMap<String, u32>) -> String {
    let cleaned: String = seed
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | ' ' | '\t') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let base = if cleaned.is_empty() {
        "pkg".to_string()
    } else {
        cleaned
    };
    let count = used.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{base}_{count}")
    }
}

fn canonicalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Translate a `workspace:` peer spec into a concrete semver range using
/// the sibling's pinned version. `workspace:*` / `workspace:` collapse to
/// the exact version; `^`/`~` keep their operator; any other suffix is
/// already a valid range and used verbatim. Used only for peer-dep
/// rewrites — regular deps go through bundling and become `file:` refs.
fn resolve_workspace_spec(spec: &str, concrete_version: &str) -> String {
    let suffix = spec.strip_prefix("workspace:").unwrap_or(spec);
    match suffix {
        "" | "*" => concrete_version.to_string(),
        "^" => format!("^{concrete_version}"),
        "~" => format!("~{concrete_version}"),
        other => other.to_string(),
    }
}

/// Copy each planned source into its `target_dir`. Directory sources
/// honor pack's selection (or the `deployAllFiles` carve-out when the
/// caller opted in); tarball sources copy the archive bytes verbatim.
fn materialize_injections(
    plan: &InjectionPlan,
    ws_index: &BTreeMap<String, (PathBuf, String)>,
    deploy_all_files: bool,
) -> miette::Result<()> {
    for inj in plan.values() {
        std::fs::create_dir_all(&inj.target_dir)
            .into_diagnostic()
            .wrap_err_with(|| format!("failed to create {}", inj.target_dir.display()))?;

        if inj.is_tarball {
            let dst = inj.target_dir.join(&inj.tarball_filename);
            std::fs::copy(&inj.source_dir, &dst)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!(
                        "failed to copy {} -> {}",
                        inj.source_dir.display(),
                        dst.display()
                    )
                })?;
            continue;
        }

        // Directory source. Reuse pack's file selection by default so a
        // sibling with `files: [...]` ships the same payload it would
        // publish; honor `deployAllFiles=true` for parity with the
        // top-level deployed-package selection.
        let source_is_workspace_sibling = ws_index
            .values()
            .any(|(p, _)| canonicalize(p) == inj.source_dir);
        let files: Vec<(PathBuf, String)> = if deploy_all_files && source_is_workspace_sibling {
            collect_all_files(&inj.source_dir, &inj.target_dir)?
        } else {
            let manifest = super::load_manifest(&inj.source_dir.join("package.json"))?;
            collect_package_files(&inj.source_dir, &manifest)?
        };
        for (src, rel) in &files {
            let dst = inj.target_dir.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("failed to create {}", parent.display()))?;
            }
            std::fs::copy(src, &dst)
                .into_diagnostic()
                .wrap_err_with(|| {
                    format!("failed to copy {} -> {}", src.display(), dst.display())
                })?;
        }
    }
    Ok(())
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

/// Which dep fields `rewrite_local_refs` should physically remove from a
/// `package.json` before install runs.
#[derive(Debug, Clone, Copy, Default)]
struct StripFields {
    dependencies: bool,
    dev_dependencies: bool,
    optional_dependencies: bool,
}

impl StripFields {
    /// Stripping policy for the top-level deployed manifest. Honors the
    /// CLI flags: `--prod` (default), `--dev`, `--no-prod`,
    /// `--no-optional`.
    fn for_args(args: &DeployArgs) -> Self {
        let DepAxis {
            prod,
            dev,
            optional,
        } = DepAxis::for_args(args);
        Self {
            dependencies: !prod,
            dev_dependencies: !dev,
            optional_dependencies: !optional,
        }
    }

    /// Stripping policy for bundled siblings. Bundled siblings exist
    /// only to satisfy the deployed package's runtime tree, so their
    /// devDependencies are always dropped — the deploy isn't a dev
    /// environment for siblings. Optional sibling deps mirror the
    /// top-level `--no-optional` choice so a sibling's optional sub-dep
    /// doesn't sneak past the user's filter.
    fn for_bundled_sibling(args: &DeployArgs) -> Self {
        Self {
            dependencies: false,
            dev_dependencies: true,
            optional_dependencies: args.no_optional,
        }
    }
}

/// Single source of truth for which dep types a deploy keeps, given the
/// CLI flag combination. Every consumer (manifest strip, install
/// `DepSelection`, lockfile-subset keep predicate) derives from this,
/// so the three paths can't silently drift onto different formulas.
/// Booleans are "keep this dep type", not "the flag is set".
#[derive(Debug, Clone, Copy)]
struct DepAxis {
    prod: bool,
    dev: bool,
    optional: bool,
}

impl DepAxis {
    fn for_args(args: &DeployArgs) -> Self {
        // clap enforces `--prod`, `--dev`, `--no-prod` mutually
        // exclusive on the deploy surface, so the cases collapse:
        //   default / --prod  -> prod + optional
        //   --dev             -> dev only
        //   --no-prod         -> prod + dev + optional
        // `--no-optional` is independent and only suppresses optionals.
        Self {
            prod: !args.dev,
            dev: args.dev || args.no_prod,
            optional: !args.dev && !args.no_optional,
        }
    }
}

/// Install-side dep selection. Intentionally NOT derived from `DepAxis`:
/// the manifest strip and lockfile subset operate on *direct* dep fields
/// (under `--dev` they drop the top-level `optionalDependencies` field,
/// matching pnpm), but install's `DepSelection` filters the *resolved*
/// dependency graph — folding `--dev` into `no_optional` here would also
/// strip transitive optional sub-deps of devDependencies (e.g. an
/// optional sub-dep of `jest`), breaking dev tooling at runtime. Only
/// the explicit `--no-optional` flag gates the install-side optional
/// axis; the direct `optionalDependencies` field is already gone from
/// the deployed manifest before install runs.
fn dep_selection_for_args(args: &DeployArgs) -> install::DepSelection {
    let prod = !args.dev && !args.no_prod;
    let dev = args.dev;
    install::DepSelection::from_flags(prod, dev, args.no_optional)
}

/// `subset_to_importer` keep predicate: shares `DepAxis::for_args` with
/// `StripFields::for_args` so the source lockfile subset and the
/// rewritten target manifest agree on which dep types survive.
fn keep_dep_for_args(args: &DeployArgs) -> impl Fn(&aube_lockfile::DirectDep) -> bool + use<> {
    let DepAxis {
        prod,
        dev,
        optional,
    } = DepAxis::for_args(args);
    move |d: &aube_lockfile::DirectDep| match d.dep_type {
        aube_lockfile::DepType::Production => prod,
        aube_lockfile::DepType::Dev => dev,
        aube_lockfile::DepType::Optional => optional,
    }
}

/// Rewrite the `dependencies` / `devDependencies` / `optionalDependencies`
/// fields of `manifest_path` so every `workspace:` / `file:` / `link:`
/// specifier becomes a relative `file:` pointer at the bundled copy
/// staged under `<target>/.aube-deploy-injected/<id>/`. `strip` names
/// any dep fields the caller wants physically removed before install
/// runs — load-bearing for `--prod` / `--dev` / `--no-optional`, since
/// install's resolver walks the full manifest before the linker applies
/// filtering, so an unstripped sibling devDep would still be fetched.
///
/// `source_pkg_dir` resolves relative `file:` / `link:` paths the same
/// way they resolve in the source workspace; `manifest_dir` is where
/// the rewritten manifest lives, used to compute the relative
/// `file:./...` path back to the staged sibling. For the deployed
/// package these are the source pkg and the target root; for a bundled
/// sibling they are the sibling's own source dir and its
/// `<target>/.aube-deploy-injected/<id>/` staging dir.
///
/// Unknown `workspace:` refs are a hard error (bundling would have
/// already inserted them into `plan` if they were valid).
/// `peerDependencies` is left untouched — peers are satisfied by the
/// consumer's installed tree, not bundled.
/// Where the deployed package lives in the source workspace (canonical
/// path) and the deploy target root. Used by the rewriter to recognize
/// back-refs to the deployed package and emit a `file:` pointer back at
/// the target instead of bundling a duplicate copy.
#[derive(Debug, Clone, Copy)]
struct DeployRoot<'a> {
    deployed_canonical: &'a Path,
    target_root: &'a Path,
}

fn rewrite_local_refs(
    manifest_path: &Path,
    source_pkg_dir: &Path,
    manifest_dir: &Path,
    ws_index: &BTreeMap<String, (PathBuf, String)>,
    plan: &InjectionPlan,
    strip: StripFields,
    root: DeployRoot<'_>,
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
            if aube_util::pkg::is_workspace_spec(spec) {
                let Some((sibling_dir, sibling_version)) = ws_index.get(name) else {
                    return Err(miette!(
                        "aube deploy: {} declares `{name}: {spec}` but no workspace package named {name:?} was found",
                        manifest_path.display()
                    ));
                };
                let canonical = canonicalize(sibling_dir);
                // Back-ref to the deployed package itself: a sibling B
                // depending on `@deployed-pkg` via `workspace:*` must
                // resolve to the deploy root, not a bundled copy
                // (singletons would otherwise break). Emit a `file:`
                // pointer back at `target_root` from `manifest_dir`.
                if canonical == root.deployed_canonical {
                    *spec_val =
                        serde_json::Value::String(file_spec_to_dir(manifest_dir, root.target_root));
                    continue;
                }
                let Some(inj) = plan.get(&canonical) else {
                    // Reachable when `peerDependencies` references a
                    // workspace sibling — peers aren't bundled (bundling
                    // walks dependencies/devDependencies/optionalDependencies
                    // only). Resolve the `workspace:` spec to a concrete
                    // semver range so the install layer can actually parse
                    // it; leaving raw `workspace:*` would hard-fail when
                    // the deploy target has no workspace context.
                    if *field == "peerDependencies" {
                        *spec_val = serde_json::Value::String(resolve_workspace_spec(
                            spec,
                            sibling_version,
                        ));
                        continue;
                    }
                    return Err(miette!(
                        "aube deploy: bundling plan missing entry for workspace sibling {name:?} declared in {}",
                        manifest_path.display()
                    ));
                };
                *spec_val = serde_json::Value::String(file_spec_for_injection(manifest_dir, inj));
            } else if let Some(local) = aube_lockfile::LocalSource::parse(spec, source_pkg_dir) {
                let abs = match &local {
                    aube_lockfile::LocalSource::Directory(rel)
                    | aube_lockfile::LocalSource::Link(rel)
                    | aube_lockfile::LocalSource::Tarball(rel) => source_pkg_dir.join(rel),
                    aube_lockfile::LocalSource::Git(_)
                    | aube_lockfile::LocalSource::RemoteTarball(_) => continue,
                };
                let canonical = canonicalize(&abs);
                // Same back-ref guard as the `workspace:` branch: a sibling
                // reaching the deployed package via `file:../deployed-pkg`
                // must rewrite to a `file:` pointer at the deploy root,
                // not to a duplicate copy.
                if canonical == root.deployed_canonical {
                    *spec_val =
                        serde_json::Value::String(file_spec_to_dir(manifest_dir, root.target_root));
                    continue;
                }
                let Some(inj) = plan.get(&canonical) else {
                    // `file:`/`link:` peers are not bundled (peerDependencies
                    // is excluded from `iter_strippable_deps`), so a peer
                    // pointing at a relative local path can't be left as-is:
                    // the relative path means something else under the
                    // deploy target. Fail loudly rather than ship a manifest
                    // whose paths resolve nowhere at runtime.
                    if *field == "peerDependencies" {
                        return Err(miette!(
                            "aube deploy: peerDependencies cannot reference a local `file:`/`link:` target ({name:?} -> {spec:?}) — peers aren't bundled into the deploy and the relative path won't resolve under the target. Promote the peer to a regular dependency or drop the local path."
                        ));
                    }
                    return Err(miette!(
                        "aube deploy: bundling plan missing entry for `{name}: {spec}` declared in {}",
                        manifest_path.display()
                    ));
                };
                *spec_val = serde_json::Value::String(file_spec_for_injection(manifest_dir, inj));
            }
        }
    }

    let rewritten = serde_json::to_string_pretty(&doc)
        .into_diagnostic()
        .wrap_err("failed to serialize rewritten package.json")?;
    aube_util::fs_atomic::atomic_write(manifest_path, rewritten.as_bytes())
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to write {}", manifest_path.display()))?;
    Ok(())
}

/// Build the `file:./...` spec the rewriter writes for an injected ref.
/// For directory sources the path points at the staged package root;
/// for tarball sources it points at the staged tarball file. Always
/// emits POSIX separators so a deploy artifact built on macOS/Linux
/// installs unchanged on Windows.
fn file_spec_for_injection(manifest_dir: &Path, inj: &Injection) -> String {
    let target_path = if inj.is_tarball {
        inj.target_dir.join(&inj.tarball_filename)
    } else {
        inj.target_dir.clone()
    };
    file_spec_to_dir(manifest_dir, &target_path)
}

/// `file:` spec from `manifest_dir` to `target` as a forward-slashed
/// relative path. Used both for bundled-injection refs and for
/// back-refs from a bundled sibling to the deploy root.
fn file_spec_to_dir(manifest_dir: &Path, target: &Path) -> String {
    let rel = pathdiff::diff_paths(target, manifest_dir).unwrap_or_else(|| target.to_path_buf());
    let mut s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        s = ".".to_string();
    }
    // npm/pnpm canonicalize plain `file:` refs; `file:./x` is more
    // visually obviously a relative path than `file:x`, so prefix `./`
    // when the result doesn't already start with a path-traversal or
    // absolute marker.
    if !s.starts_with("./") && !s.starts_with("../") && !s.starts_with('/') {
        s = format!("./{s}");
    }
    format!("file:{s}")
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

    fn deploy_args() -> DeployArgs {
        DeployArgs {
            target: PathBuf::from("/tmp/unused"),
            dev: false,
            no_optional: false,
            prod: false,
            no_prod: false,
            lockfile: crate::cli_args::LockfileArgs::default(),
            network: crate::cli_args::NetworkArgs::default(),
            virtual_store: crate::cli_args::VirtualStoreArgs::default(),
        }
    }

    #[test]
    fn dep_selection_default_is_prod() {
        let a = deploy_args();
        assert_eq!(dep_selection_for_args(&a), install::DepSelection::Prod);
    }

    #[test]
    fn dep_selection_no_prod_is_all() {
        let a = DeployArgs {
            no_prod: true,
            ..deploy_args()
        };
        assert_eq!(dep_selection_for_args(&a), install::DepSelection::All);
    }

    #[test]
    fn dep_selection_no_prod_and_no_optional_is_no_optional() {
        let a = DeployArgs {
            no_prod: true,
            no_optional: true,
            ..deploy_args()
        };
        assert_eq!(
            dep_selection_for_args(&a),
            install::DepSelection::NoOptional
        );
    }

    #[test]
    fn dep_selection_covers_every_flag_combo() {
        // Lock the (dev, no_prod, no_optional) -> DepSelection table so
        // a future tweak to dep_selection_for_args can't silently drift.
        // Note `--dev` alone maps to `Dev`, not `DevNoOptional`: the
        // direct `optionalDependencies` field is stripped by
        // `StripFields`, but install's optional axis must stay open so
        // transitive optional sub-deps of devDependencies still resolve
        // (see comment on `dep_selection_for_args`).
        let cases: &[(bool, bool, bool, install::DepSelection)] = &[
            (false, false, false, install::DepSelection::Prod),
            (false, false, true, install::DepSelection::ProdNoOptional),
            (false, true, false, install::DepSelection::All),
            (false, true, true, install::DepSelection::NoOptional),
            (true, false, false, install::DepSelection::Dev),
            (true, false, true, install::DepSelection::DevNoOptional),
        ];
        for &(dev, no_prod, no_optional, want) in cases {
            let a = DeployArgs {
                dev,
                no_prod,
                no_optional,
                ..deploy_args()
            };
            assert_eq!(
                dep_selection_for_args(&a),
                want,
                "dev={dev} no_prod={no_prod} no_optional={no_optional}"
            );
        }
    }

    #[test]
    fn strip_default_drops_dev_keeps_prod_and_optional() {
        let s = StripFields::for_args(&deploy_args());
        assert!(!s.dependencies);
        assert!(s.dev_dependencies);
        assert!(!s.optional_dependencies);
    }

    #[test]
    fn strip_no_prod_keeps_everything() {
        let a = DeployArgs {
            no_prod: true,
            ..deploy_args()
        };
        let s = StripFields::for_args(&a);
        assert!(!s.dependencies);
        assert!(!s.dev_dependencies);
        assert!(!s.optional_dependencies);
    }

    #[test]
    fn strip_dev_only_drops_prod_and_optional() {
        let a = DeployArgs {
            dev: true,
            ..deploy_args()
        };
        let s = StripFields::for_args(&a);
        assert!(s.dependencies);
        assert!(!s.dev_dependencies);
        assert!(s.optional_dependencies);
    }

    #[test]
    fn strip_for_bundled_sibling_always_drops_dev() {
        // Bundled siblings exist only to satisfy the runtime tree. The
        // top-level flag set must not flip dev back on for siblings.
        let s = StripFields::for_bundled_sibling(&DeployArgs {
            no_prod: true,
            ..deploy_args()
        });
        assert!(s.dev_dependencies);
        assert!(!s.dependencies);
        assert!(!s.optional_dependencies);
    }

    #[test]
    fn rewrite_local_refs_drops_workspace_dep_when_field_stripped() {
        // `--prod` default: a workspace: devDep must be physically
        // removed from the deployed manifest before install runs (the
        // resolver walks every dep field before filtering).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"lodash":"^4"},"devDependencies":{"@test/internal":"workspace:*"}}"#,
        )
        .unwrap();

        let idx = ws_index(&[]); // empty: dev is stripped, sibling never looked up
        let plan = InjectionPlan::new();
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &idx,
            &plan,
            StripFields {
                dependencies: false,
                dev_dependencies: true,
                optional_dependencies: false,
            },
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(out.get("devDependencies").is_none());
        assert_eq!(out["dependencies"]["lodash"], "^4");
    }

    #[test]
    fn rewrite_local_refs_writes_relative_file_spec_for_workspace_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest_dir = tmp.path();
        let sibling_dir = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        let injected_dir = manifest_dir.join(".aube-deploy-injected").join("lib");
        std::fs::create_dir_all(&injected_dir).unwrap();

        let path = manifest_dir.join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/lib":"workspace:*"}}"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        idx.insert(
            "@test/lib".to_string(),
            (sibling_dir.clone(), "1.2.3".to_string()),
        );
        let mut plan = InjectionPlan::new();
        plan.insert(
            canonicalize(&sibling_dir),
            Injection {
                source_dir: sibling_dir.clone(),
                is_tarball: false,
                target_dir: injected_dir.clone(),
                tarball_filename: String::new(),
            },
        );
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            manifest_dir,
            manifest_dir,
            &idx,
            &plan,
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: manifest_dir,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            out["dependencies"]["@test/lib"],
            "file:./.aube-deploy-injected/lib"
        );
    }

    #[test]
    fn rewrite_local_refs_resolves_workspace_peer_to_concrete_range() {
        // peerDependencies aren't bundled (bundling walks deps/devDeps/
        // optionalDeps only), so they hit the resolve_workspace_spec
        // path. Each spec form should land on a parseable semver range.
        let tmp = tempfile::tempdir().unwrap();
        let manifest_dir = tmp.path();
        let sibling_dir = tmp.path().join("packages/lib");
        std::fs::create_dir_all(&sibling_dir).unwrap();

        let path = manifest_dir.join("package.json");
        std::fs::write(
            &path,
            r#"{
                "name":"x",
                "version":"1.0.0",
                "peerDependencies":{
                    "@test/lib":"workspace:*",
                    "@test/lib-caret":"workspace:^",
                    "@test/lib-tilde":"workspace:~",
                    "@test/lib-literal":"workspace:^2.0.0"
                }
            }"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        for n in [
            "@test/lib",
            "@test/lib-caret",
            "@test/lib-tilde",
            "@test/lib-literal",
        ] {
            idx.insert(n.to_string(), (sibling_dir.clone(), "1.2.3".to_string()));
        }
        let stub = PathBuf::from("/nonexistent-deployed");
        rewrite_local_refs(
            &path,
            manifest_dir,
            manifest_dir,
            &idx,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: manifest_dir,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let peers = &out["peerDependencies"];
        assert_eq!(peers["@test/lib"], "1.2.3");
        assert_eq!(peers["@test/lib-caret"], "^1.2.3");
        assert_eq!(peers["@test/lib-tilde"], "~1.2.3");
        assert_eq!(peers["@test/lib-literal"], "^2.0.0");
    }

    #[test]
    fn rewrite_local_refs_writes_back_ref_to_target_root_for_deployed_pkg() {
        // Sibling B (staged at <target>/.aube-deploy-injected/B/)
        // declares a `workspace:*` back-dep on the deployed package.
        // We must not bundle the deployed package as a separate
        // injection (singleton would break); instead, rewrite the spec
        // to a `file:` pointer back at the deploy root.
        let tmp = tempfile::tempdir().unwrap();
        let target_root = tmp.path();
        let deployed_dir = target_root.join("source/deployed-pkg");
        std::fs::create_dir_all(&deployed_dir).unwrap();
        let deployed_canonical = canonicalize(&deployed_dir);
        let sibling_target = target_root.join(".aube-deploy-injected").join("b");
        std::fs::create_dir_all(&sibling_target).unwrap();

        let sibling_manifest = sibling_target.join("package.json");
        std::fs::write(
            &sibling_manifest,
            r#"{"name":"@test/b","version":"1.0.0","dependencies":{"@deployed/pkg":"workspace:*"}}"#,
        )
        .unwrap();

        let mut idx = BTreeMap::new();
        idx.insert(
            "@deployed/pkg".to_string(),
            (deployed_canonical.clone(), "9.9.9".to_string()),
        );
        rewrite_local_refs(
            &sibling_manifest,
            &deployed_canonical,
            &sibling_target,
            &idx,
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &deployed_canonical,
                target_root,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sibling_manifest).unwrap()).unwrap();
        // From <target>/.aube-deploy-injected/b/ back to <target>/ is
        // `../..`.
        assert_eq!(out["dependencies"]["@deployed/pkg"], "file:../..");
    }

    #[test]
    fn rewrite_local_refs_writes_back_ref_for_file_link_to_deployed_pkg() {
        // Same back-ref scenario as the workspace test, but the sibling
        // references the deployed package via `file:` instead of
        // `workspace:*`. The result must still be a relative file:
        // pointer at the deploy root, not a bundled duplicate.
        let tmp = tempfile::tempdir().unwrap();
        let target_root = tmp.path();
        let deployed_dir = target_root.join("source/deployed-pkg");
        std::fs::create_dir_all(&deployed_dir).unwrap();
        let deployed_canonical = canonicalize(&deployed_dir);
        let sibling_target = target_root.join(".aube-deploy-injected").join("b");
        std::fs::create_dir_all(&sibling_target).unwrap();

        let sibling_manifest = sibling_target.join("package.json");
        std::fs::write(
            &sibling_manifest,
            r#"{"name":"@test/b","version":"1.0.0","dependencies":{"@deployed/pkg":"file:../../source/deployed-pkg"}}"#,
        )
        .unwrap();

        rewrite_local_refs(
            &sibling_manifest,
            &deployed_canonical,
            &sibling_target,
            &BTreeMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &deployed_canonical,
                target_root,
            },
        )
        .unwrap();

        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sibling_manifest).unwrap()).unwrap();
        assert_eq!(out["dependencies"]["@deployed/pkg"], "file:../..");
    }

    #[test]
    fn rewrite_local_refs_errors_on_file_peer_dep() {
        // `file:`/`link:` peer specs aren't bundled and the relative
        // path doesn't survive the deploy. Hard-fail rather than ship a
        // manifest whose peer paths resolve nowhere.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","peerDependencies":{"vendor":"file:../local-vendor"}}"#,
        )
        .unwrap();
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &BTreeMap::new(),
            &InjectionPlan::new(),
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("peerDependencies"), "msg was: {msg}");
        assert!(msg.contains("vendor"), "msg was: {msg}");
    }

    #[test]
    fn rewrite_local_refs_errors_on_unknown_workspace_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("package.json");
        std::fs::write(
            &path,
            r#"{"name":"x","version":"1.0.0","dependencies":{"@test/missing":"workspace:*"}}"#,
        )
        .unwrap();
        let idx = ws_index(&[]);
        let plan = InjectionPlan::new();
        let stub = PathBuf::from("/nonexistent-deployed");
        let err = rewrite_local_refs(
            &path,
            tmp.path(),
            tmp.path(),
            &idx,
            &plan,
            StripFields::default(),
            DeployRoot {
                deployed_canonical: &stub,
                target_root: tmp.path(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("@test/missing"));
    }

    #[test]
    fn file_spec_for_injection_emits_relative_directory_path() {
        let manifest_dir = PathBuf::from("/tmp/deploy/out");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/lib");
        let inj = Injection {
            source_dir: PathBuf::from("/src/lib"),
            is_tarball: false,
            target_dir,
            tarball_filename: String::new(),
        };
        assert_eq!(
            file_spec_for_injection(&manifest_dir, &inj),
            "file:./.aube-deploy-injected/lib"
        );
    }

    #[test]
    fn file_spec_for_injection_emits_relative_tarball_path() {
        let manifest_dir = PathBuf::from("/tmp/deploy/out");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/foo");
        let inj = Injection {
            source_dir: PathBuf::from("/src/foo.tgz"),
            is_tarball: true,
            target_dir,
            tarball_filename: "foo.tgz".to_string(),
        };
        assert_eq!(
            file_spec_for_injection(&manifest_dir, &inj),
            "file:./.aube-deploy-injected/foo/foo.tgz"
        );
    }

    #[test]
    fn file_spec_for_injection_emits_dotdot_for_sibling_in_injected_dir() {
        // A bundled sibling whose own manifest references another
        // bundled sibling: rewrite emits `../<id>` relative to the
        // sibling's own staging dir, not the deploy root.
        let manifest_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/lib");
        let target_dir = PathBuf::from("/tmp/deploy/out/.aube-deploy-injected/core");
        let inj = Injection {
            source_dir: PathBuf::from("/src/core"),
            is_tarball: false,
            target_dir,
            tarball_filename: String::new(),
        };
        assert_eq!(file_spec_for_injection(&manifest_dir, &inj), "file:../core");
    }

    #[test]
    fn unique_id_disambiguates_collisions() {
        let mut used = BTreeMap::new();
        assert_eq!(unique_id("lib", &mut used), "lib");
        assert_eq!(unique_id("lib", &mut used), "lib_2");
    }

    #[test]
    fn unique_id_sanitizes_unsafe_chars() {
        let mut used = BTreeMap::new();
        assert_eq!(unique_id("@scope/name", &mut used), "@scope_name");
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
