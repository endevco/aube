use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// Package(s) to update (all if empty)
    pub packages: Vec<String>,
    /// Update only devDependencies.
    #[arg(short = 'D', long, conflicts_with = "prod")]
    pub dev: bool,
    /// Pin manifest specifiers to the resolved version with no range
    /// prefix.
    ///
    /// Pair with `--latest`: when the rewritten specifier replaces the
    /// caret/tilde original, drop the prefix so the manifest carries an
    /// exact pin (`"1.2.3"`) instead of `"^1.2.3"`. Mirrors
    /// `pnpm update --save-exact`.
    #[arg(short = 'E', long, visible_alias = "save-exact")]
    pub exact: bool,
    /// Update globally installed packages.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Interactive update picker.
    ///
    /// Parsed for pnpm compatibility.
    #[arg(short = 'i', long)]
    pub interactive: bool,
    /// Update past the manifest range.
    ///
    /// Rewrites `package.json` specifiers to match the newly resolved
    /// versions (the registry's `latest` dist-tag, clamped by
    /// `minimumReleaseAge` / `resolution-mode` as usual).
    #[arg(short = 'L', long)]
    pub latest: bool,
    /// Update only production dependencies.
    #[arg(
        short = 'P',
        long,
        conflicts_with = "dev",
        visible_alias = "production"
    )]
    pub prod: bool,
    /// Update dependencies in the current workspace package.
    #[arg(short = 'w', long)]
    pub workspace: bool,
    /// Dependency traversal depth.
    ///
    /// Parsed for pnpm compatibility but ignored: aube only ever
    /// refreshes direct deps (pnpm's `--depth 0` default). Setting
    /// the flag emits a one-line warning pointing at
    /// `rm aube-lock.yaml && aube install` for the
    /// `--depth Infinity` case.
    #[arg(long)]
    pub depth: Option<String>,
    /// Add a global pnpmfile that runs before the local one.
    ///
    /// Mirrors pnpm's `--global-pnpmfile <path>`. The global hook runs
    /// first and the local hook (if any) runs second.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub global_pnpmfile: Option<std::path::PathBuf>,
    /// Skip running `.pnpmfile.mjs` / `.pnpmfile.cjs` hooks for this update.
    #[arg(long)]
    pub ignore_pnpmfile: bool,
    /// Skip lifecycle scripts.
    ///
    /// Accepted for pnpm parity — dep scripts are already gated by
    /// `allowBuilds`, so the flag is currently a no-op, but scripts
    /// that wrap `pnpm update --ignore-scripts` keep working without
    /// complaint.
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Skip optionalDependencies.
    #[arg(long)]
    pub no_optional: bool,
    /// Refresh the lockfile without rewriting `package.json` ranges.
    ///
    /// Pair with `--latest` to pull a newer resolved version into the
    /// lockfile while leaving the manifest's caret/tilde ranges
    /// untouched. Without `--latest` this flag is a no-op (plain
    /// `update` already doesn't touch the manifest). Mirrors
    /// `pnpm update --no-save`.
    #[arg(long)]
    pub no_save: bool,
    /// Override the local pnpmfile location.
    ///
    /// Mirrors pnpm's `--pnpmfile <path>`. Relative paths resolve
    /// against the project root; absolute paths are used as-is. Wins
    /// over `pnpmfilePath` from `pnpm-workspace.yaml`.
    #[arg(long, value_name = "PATH", conflicts_with = "ignore_pnpmfile")]
    pub pnpmfile: Option<std::path::PathBuf>,
}

pub async fn run(
    args: UpdateArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let _ = args.ignore_scripts; // parity no-op: dep scripts already gated by allowBuilds
    let _ = (args.global, args.workspace, args.interactive);
    if let Some(depth) = args.depth.as_deref() {
        // pnpm's `--depth Infinity` is the only useful value; the
        // intermediate ones (`--depth 1`, `--depth 2`) have semantics
        // that even pnpm users get tripped up on. aube only refreshes
        // direct deps, so the flag is a no-op — warn once with the
        // workaround for the genuine refresh-transitives case.
        eprintln!(
            "warn: --depth {depth} is ignored; aube only refreshes direct deps. \
             For a full refresh, run `rm aube-lock.yaml && aube install`."
        );
    }
    if !filter.is_empty() {
        return run_filtered(args, &filter).await;
    }
    reject_unsupported_pkg_specs(&args.packages)?;
    // Parse `<pkg>@<spec>` arg syntax. Today only `@latest` is honored —
    // it's the syntactic equivalent of `--latest` scoped to that one
    // entry, which is how pnpm phrases the manifest-rewrite-past-range
    // case (`pnpm update foo@latest`). Non-`latest` specs are rejected
    // by `reject_unsupported_pkg_specs` above so they don't silently
    // get swallowed.
    let mut explicit_latest_keys: BTreeSet<String> = BTreeSet::new();
    let parsed_packages: Vec<String> = args
        .packages
        .iter()
        .map(|raw| {
            let (name, spec) = split_pkg_arg(raw);
            if spec == Some("latest") {
                explicit_latest_keys.insert(name.to_string());
            }
            name.to_string()
        })
        .collect();
    let packages = &parsed_packages[..];
    let latest = args.latest;
    let no_save = args.no_save;
    // `--latest` flag triggers manifest rewrites for every direct dep;
    // `<pkg>@latest` triggers it only for that one entry. Combine them
    // into a per-key predicate so the same code path serves both.
    let effective_latest = latest || !explicit_latest_keys.is_empty();
    let should_rewrite_key = |key: &str| -> bool { latest || explicit_latest_keys.contains(key) };
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let UpdateSettings {
        ignored: ignored_updates,
        rewrites_specifier: rewrites_specifier_setting,
    } = resolve_update_settings(&cwd, &manifest)?;
    // Cosmetic floor-bump: outside `--latest` and `--no-save`, with
    // `updateRewritesSpecifier=true` (default), `aube update <pkg>` also
    // tracks the resolved in-range version in `package.json`. Limited to
    // `^X.Y.Z` / `~X.Y.Z` specs at the rewrite site below; other shapes
    // (`>=`, `1.x`, exact, dist-tags, git, workspace:) are preserved.
    let cosmetic_rewrite_eligible = !effective_latest && rewrites_specifier_setting && !no_save;

    // Read the lockfile from the project, or fall back to the shared
    // workspace-root one when the project doesn't have its own (the
    // common shape after a fresh `aube install` from the workspace
    // root). This is what unblocks `aube update -r <indirect>@latest`
    // — the indirect's snapshot lives in the shared lockfile, not in
    // each project's directory, and without the fallback the
    // indirect-arg validation below would reject it.
    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest)
        .ok()
        .or_else(|| {
            super::find_workspace_root(&cwd)
                .ok()
                .filter(|ws| ws.as_path() != cwd.as_path())
                .and_then(|ws| aube_lockfile::parse_lockfile(&ws, &manifest).ok())
        });

    // Snapshot of every direct dep as (manifest key, specifier). Owned
    // strings so we can hold this across mutations of `manifest`.
    let include_prod = !args.dev;
    let include_dev = !args.prod;
    let include_optional = !args.no_optional && !args.dev;
    let all_specifiers: BTreeMap<String, String> = manifest
        .dependencies
        .iter()
        .filter(|_| include_prod)
        .chain(manifest.dev_dependencies.iter().filter(|_| include_dev))
        .chain(
            manifest
                .optional_dependencies
                .iter()
                .filter(|_| include_optional),
        )
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Set of every direct manifest key, ignoring `--prod`/`--dev`/
    // `--no-optional` filters. Used below to distinguish "flag-excluded
    // direct dep" from "truly indirect dep" — without this, a devDep
    // named under `--prod` would miss `all_specifiers`, fall through to
    // the indirect-dep branch, pass `in_graph` (the lockfile carries
    // every dep regardless of bucket), and get silently re-resolved.
    let all_direct_keys: BTreeSet<&str> = manifest
        .dependencies
        .keys()
        .chain(manifest.dev_dependencies.keys())
        .chain(manifest.optional_dependencies.keys())
        .map(String::as_str)
        .collect();

    let resolve_real_name = |manifest_key: &str| -> String {
        if let Some(specifier) = all_specifiers.get(manifest_key)
            && let Some(rest) = specifier.strip_prefix("npm:")
        {
            // "npm:real-pkg@^2.0.0" -> "real-pkg"
            if let Some(at_idx) = rest.rfind('@') {
                return rest[..at_idx].to_string();
            }
            return rest.to_string();
        }
        manifest_key.to_string()
    };

    // Determine which packages to update.
    //
    // Args split into direct (in package.json) vs indirect (only in the
    // lockfile). pnpm allows `pnpm update <indirect-pkg>` to refresh a
    // transitive dep without touching package.json — match that. Direct
    // args land in `manifest_keys_to_update` and drive the rewrite path;
    // indirect args go into `indirect_arg_names` and are filtered from
    // the locked snapshot below so the resolver picks them fresh.
    let update_all = packages.is_empty();
    let mut indirect_arg_names: BTreeSet<String> = BTreeSet::new();
    if !update_all {
        for name in packages {
            if all_specifiers.contains_key(name.as_str()) {
                if ignored_updates.contains(name.as_str()) {
                    return Err(miette!(
                        "package '{name}' is ignored by updateConfig.ignoreDependencies"
                    ));
                }
                continue;
            }
            // Direct dep excluded by `--prod`/`--dev`/`--no-optional`.
            // Match the pre-indirect-support behavior so the flag
            // mismatch is visible — otherwise the dep would slip into
            // the indirect path below and silently re-resolve.
            if all_direct_keys.contains(name.as_str()) {
                return Err(miette!("package '{name}' is not a dependency"));
            }
            // Indirect dep: must exist in the lockfile graph (either as
            // its own name or as the real-name of an aliased entry).
            let in_graph = existing.as_ref().is_some_and(|g| {
                g.packages
                    .values()
                    .any(|p| p.name == *name || p.alias_of.as_deref() == Some(name.as_str()))
            });
            if !in_graph {
                return Err(miette!("package '{name}' is not a dependency"));
            }
            if ignored_updates.contains(name.as_str()) {
                return Err(miette!(
                    "package '{name}' is ignored by updateConfig.ignoreDependencies"
                ));
            }
            indirect_arg_names.insert(name.clone());
        }
    }
    let manifest_keys_to_update: Vec<String> = if update_all {
        all_specifiers
            .keys()
            .filter(|name| !ignored_updates.contains(name.as_str()))
            .cloned()
            .collect()
    } else {
        packages
            .iter()
            .filter(|p| all_specifiers.contains_key(p.as_str()))
            .filter(|p| {
                if ignored_updates.contains(p.as_str()) {
                    tracing::info!("skipping {p} (updateConfig.ignoreDependencies)");
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect()
    };

    let real_names_to_update: std::collections::HashSet<String> = manifest_keys_to_update
        .iter()
        .map(|k| resolve_real_name(k))
        .collect();

    if update_all {
        eprintln!("Updating all dependencies...");
    } else {
        eprintln!("Updating: {}", parsed_packages.join(", "));
    }

    // `--latest`: pre-fetch packuments for every direct dep we're
    // about to rewrite so we can detect manifest pins that are NEWER
    // than the registry's `latest` dist-tag and skip them. Mirrors
    // pnpm's regression guard from #7436 — a user-pinned prerelease
    // (e.g. `"3.0.0-rc.0"` while latest=`2.0.0`) shouldn't be silently
    // downgraded by `update --latest`. Packuments come from the same
    // cache the resolver uses moments later, so the only cost when
    // nothing's stale is a cheap revalidation.
    //
    // Only applied to bulk updates (no positional args). When the user
    // explicitly names a package — `aube update --latest <pkg>` or
    // `aube update <pkg>@latest` — they're opting in to whatever the
    // registry's `latest` says, even when that downgrades a prerelease
    // (matching pnpm's behavior for `pnpm update <pkg>@latest`).
    let preserve_pin: BTreeSet<String> = if latest && update_all {
        let client = std::sync::Arc::new(super::make_client(&cwd));
        let mut handles = Vec::new();
        for key in &manifest_keys_to_update {
            let original = all_specifiers.get(key).map(String::as_str).unwrap_or("");
            if aube_util::pkg::is_workspace_spec(original) {
                continue;
            }
            let Some(pinned) = exact_pin_version(original) else {
                continue;
            };
            let Ok(parsed_pin) = node_semver::Version::parse(pinned) else {
                continue;
            };
            let real_name = resolve_real_name(key);
            let key_owned = key.clone();
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                // A fetch failure here would silently fall through to the
                // rewrite path and downgrade the prerelease pin — exactly
                // what this guard is supposed to prevent. Surface the
                // underlying error via tracing so the user can spot a
                // transient registry failure that broke the guard, then
                // continue with the resolver path (which has its own
                // retry/cache semantics and may still succeed).
                let packument = match client.fetch_packument(&real_name).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            code = aube_codes::warnings::WARN_AUBE_PRERELEASE_CHECK_SKIPPED,
                            "skipping prerelease-preservation check for {real_name}: {e}"
                        );
                        return None;
                    }
                };
                let latest_v = packument.dist_tags.get("latest")?;
                let Ok(parsed_latest) = node_semver::Version::parse(latest_v) else {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_PRERELEASE_CHECK_SKIPPED,
                        "skipping prerelease-preservation check for {real_name}: \
                         registry returned non-semver latest dist-tag {latest_v:?}"
                    );
                    return None;
                };
                (parsed_pin > parsed_latest).then_some(key_owned)
            }));
        }
        let mut set = BTreeSet::new();
        for h in handles {
            if let Ok(Some(key)) = h.await {
                set.insert(key);
            }
        }
        set
    } else {
        BTreeSet::new()
    };

    // Rewrite each targeted direct-dep specifier on a *clone* of the
    // manifest handed to the resolver. Mutating the real in-memory
    // manifest would corrupt `package.json` if any package fails to
    // resolve — the literal `"latest"` string would stick.
    // `workspace:` specs are skipped: they refer to local workspace
    // packages, not registry versions. `preserve_pin` entries (a
    // manifest pin newer than the dist-tag latest) are also left
    // untouched so the resolver naturally re-resolves the original
    // version into the graph and the manifest rewrite path leaves the
    // pin alone. Both `--latest` (every direct dep) and `<pkg>@latest`
    // (only the named entries — see `should_rewrite_key`) flow
    // through this loop.
    let resolver_manifest = if effective_latest {
        let mut m = manifest.clone();
        for key in &manifest_keys_to_update {
            if !should_rewrite_key(key) {
                continue;
            }
            let real_name = resolve_real_name(key);
            let original = all_specifiers.get(key).map(String::as_str).unwrap_or("");
            if aube_util::pkg::is_workspace_spec(original) || preserve_pin.contains(key) {
                continue;
            }
            if aube_lockfile::parse_git_spec(original).is_some() {
                // Git specs carry their own committish; rewriting to
                // `latest` would route the resolver at the registry
                // instead of the git source. Symmetric with the guard in
                // the package.json rewrite loop below.
                continue;
            }
            let new_spec = if original.starts_with("npm:") {
                format!("npm:{real_name}@latest")
            } else {
                "latest".to_string()
            };
            if m.dependencies.contains_key(key) {
                m.dependencies.insert(key.clone(), new_spec);
            } else if m.dev_dependencies.contains_key(key) {
                m.dev_dependencies.insert(key.clone(), new_spec);
            } else if m.optional_dependencies.contains_key(key) {
                m.optional_dependencies.insert(key.clone(), new_spec);
            }
        }
        m
    } else {
        manifest.clone()
    };

    // Build a filtered lockfile that excludes packages being updated
    // so the resolver picks the latest matching version instead of the
    // locked one. Three sets need to come out:
    //   - `real_names_to_update` — direct deps by their real name.
    //   - `manifest_keys_to_update` — direct alias entries
    //     (`"alias": "npm:real@x"`) live in the graph with
    //     `pkg.name == "alias"`, not the real name; without this check
    //     the resolver would keep the locked alias version under
    //     `--latest`.
    //   - `indirect_arg_names` — transitive deps the user named on the
    //     command line. We filter both `pkg.name` matches AND
    //     `pkg.alias_of` matches so an indirect aliased entry
    //     (`other-alias: npm:dep-of-pkg-with-1-dep@x`) is also dropped.
    let filtered_existing = existing.as_ref().map(|graph| {
        let mut filtered = graph.clone();
        let manifest_keys: std::collections::HashSet<&str> =
            manifest_keys_to_update.iter().map(String::as_str).collect();
        let indirect_set: std::collections::HashSet<&str> =
            indirect_arg_names.iter().map(String::as_str).collect();
        filtered.packages.retain(|_, pkg| {
            !real_names_to_update.contains(&pkg.name)
                && !manifest_keys.contains(pkg.name.as_str())
                && !indirect_set.contains(pkg.name.as_str())
                && !pkg
                    .alias_of
                    .as_deref()
                    .is_some_and(|a| indirect_set.contains(a))
        });
        // Indirect-arg dist-tag forwarding. When the user passes
        // `<indirect>@latest`, dropping the indirect's own snapshot
        // entry isn't enough on its own — the resolver's lockfile-reuse
        // path (aube_resolver::resolve.rs:1164) iterates each parent's
        // locked `dependencies` map and enqueues transitive tasks using
        // the *locked version* as the range. So a parent locked at
        // `pkg-with-1-dep@100.0.0` with `dependencies: { dep-of: 100.0.0 }`
        // still re-resolves dep-of at exactly 100.0.0, even after we
        // dropped dep-of from `packages`. Rewriting the edge to `latest`
        // rebroadcasts it as a dist-tag spec, so the transitive task
        // resolves through the registry packument and picks the new
        // latest. Only applied for entries the user named with
        // `@latest`; bare `update <indirect>` keeps the locked edge so
        // we don't silently bump something the user didn't ask to bump.
        if !explicit_latest_keys.is_empty() {
            for parent_pkg in filtered.packages.values_mut() {
                for indirect_name in &indirect_arg_names {
                    if !explicit_latest_keys.contains(indirect_name) {
                        continue;
                    }
                    if parent_pkg.dependencies.contains_key(indirect_name.as_str()) {
                        parent_pkg
                            .dependencies
                            .insert(indirect_name.clone(), "latest".to_string());
                    }
                }
            }
        }
        filtered
    });

    // Re-resolve the full dependency tree. Wire the pnpmfile in so
    // `readPackage` mutations apply during update (not just first
    // install) and `afterAllResolved` gets a chance to rewrite the
    // graph before we hand it to the lockfile writer; without this,
    // `aube install` runs in frozen-prefer mode below and never
    // re-evaluates the hook.
    let pnpmfile_paths = if args.ignore_pnpmfile {
        Vec::new()
    } else {
        let (ws, _) = aube_manifest::workspace::load_both(&cwd).unwrap_or_default();
        crate::pnpmfile::ordered_paths(
            crate::pnpmfile::detect_global(&cwd, args.global_pnpmfile.as_deref()).as_deref(),
            crate::pnpmfile::detect(&cwd, args.pnpmfile.as_deref(), ws.pnpmfile_path.as_deref())
                .as_deref(),
        )
    };
    super::run_pnpmfile_pre_resolution(&pnpmfile_paths, &cwd, existing.as_ref()).await?;
    let (read_package_host, read_package_forwarders) =
        match crate::pnpmfile::ReadPackageHostChain::spawn(&pnpmfile_paths, &cwd)
            .await
            .wrap_err("failed to start pnpmfile readPackage host")?
        {
            Some((h, f)) => (Some(h), f),
            None => (None, Vec::new()),
        };
    let workspace_catalogs = super::load_workspace_catalogs(&cwd)?;
    let mut resolver = super::build_resolver(&cwd, &manifest, workspace_catalogs);
    if let Some(host) = read_package_host {
        resolver = resolver
            .with_read_package_hook(Box::new(host) as Box<dyn aube_resolver::ReadPackageHook>);
    }
    let mut graph = resolver
        .resolve(&resolver_manifest, filtered_existing.as_ref())
        .await
        .map_err(miette::Report::new)
        .wrap_err("failed to resolve dependencies")?;
    drop(resolver);
    // Drain the readPackage stderr forwarders so resolve-time `ctx.log`
    // records flush to stdout before afterAllResolved emits its own.
    crate::pnpmfile::ReadPackageHostChain::drain_forwarders(read_package_forwarders).await;
    crate::pnpmfile::run_after_all_resolved_chain(&pnpmfile_paths, &cwd, &mut graph).await?;

    // Report what changed. Aliased direct deps (`"alias": "npm:real@x"`)
    // land in the lockfile graph with `pkg.name == "alias"` and
    // `pkg.alias_of == Some("real")`, so the version-lookup match has to
    // accept either the manifest key (the alias) or the real name —
    // matching only on `real_name` would miss aliased entries.
    fn lookup_pkg<'a>(
        g: &'a aube_lockfile::LockfileGraph,
        manifest_key: &str,
        real_name: &str,
    ) -> Option<&'a aube_lockfile::LockedPackage> {
        g.packages
            .values()
            .find(|p| p.name == real_name || p.name == manifest_key)
    }
    for manifest_key in &manifest_keys_to_update {
        let real_name = resolve_real_name(manifest_key);

        let old_ver = existing
            .as_ref()
            .and_then(|g| lookup_pkg(g, manifest_key, &real_name))
            .map(|p| p.version.as_str());
        let new_ver = lookup_pkg(&graph, manifest_key, &real_name).map(|p| p.version.as_str());

        match (old_ver, new_ver) {
            (Some(old), Some(new)) if old != new => {
                eprintln!("  {manifest_key}: {old} -> {new}");
            }
            (Some(ver), Some(_)) => {
                eprintln!("  {manifest_key}: {ver} (already latest)");
            }
            (None, Some(new)) => {
                eprintln!("  {manifest_key}: (new) {new}");
            }
            (Some(old), None) => {
                eprintln!("  {manifest_key}: {old} -> (removed from graph)");
            }
            (None, None) => {}
        }
    }

    eprintln!("Resolved {} packages", graph.packages.len());

    // Rewrite each targeted direct dep in the real `package.json` to
    // pin the resolved version, preserving the user's existing prefix
    // (`^`/`~`/exact) and any `npm:` alias. `--latest` covers every
    // direct dep; `<pkg>@latest` covers only the named entry — both
    // flow through `should_rewrite_key`. Skip `workspace:` specs
    // (sibling packages) and skip deps that resolved to the same spec
    // they already had, so an idempotent rewrite doesn't churn the
    // manifest for no reason.
    //
    // `--no-save` short-circuits the manifest rewrite: the resolver
    // already pulled in the new versions for the lockfile above, so we
    // just skip persisting any range bumps to `package.json`.
    if no_save && (effective_latest || rewrites_specifier_setting) {
        eprintln!("Skipping package.json update (--no-save)");
    } else if effective_latest || cosmetic_rewrite_eligible {
        let mut wrote_any = false;
        for key in &manifest_keys_to_update {
            if effective_latest && !should_rewrite_key(key) {
                continue;
            }
            let real_name = resolve_real_name(key);
            let original = all_specifiers.get(key).cloned().unwrap_or_default();
            if aube_util::pkg::is_workspace_spec(&original) {
                continue;
            }
            if aube_lockfile::parse_git_spec(&original).is_some() {
                // Git specs (`github:user/repo`, `git+https://…`, bare
                // `user/repo` shorthand, …) carry their own committish
                // and have no semver range to bump. Rewriting one to
                // `^<resolved>` would silently swap the dep for a
                // registry pin and break install.
                continue;
            }
            // Cosmetic floor-bump (no `--latest`) only rewrites specs
            // that literally begin with `^` or `~` — dist-tags
            // (`"latest"`, `"next"`, …), exact pins, raw ranges
            // (`>=1.0`), and anything else are preserved as-is.
            // `range_prefix` defaults to `"^"` for unknown shapes so it
            // can't be the discriminator here. Caret/tilde under an
            // `npm:` alias lives on the post-`@` portion.
            if !effective_latest {
                let range_slice = original
                    .strip_prefix("npm:")
                    .and_then(|rest| rest.rsplit_once('@').map(|(_, r)| r))
                    .unwrap_or(original.as_str());
                if !range_slice.starts_with('^') && !range_slice.starts_with('~') {
                    continue;
                }
            }
            let Some(resolved) = lookup_pkg(&graph, key, &real_name).map(|p| p.version.clone())
            else {
                continue;
            };
            let new_spec = rewrite_specifier(&original, &real_name, &resolved, args.exact);
            if new_spec == original {
                continue;
            }
            if manifest.dependencies.contains_key(key) {
                manifest.dependencies.insert(key.clone(), new_spec);
            } else if manifest.dev_dependencies.contains_key(key) {
                manifest.dev_dependencies.insert(key.clone(), new_spec);
            } else if manifest.optional_dependencies.contains_key(key) {
                manifest.optional_dependencies.insert(key.clone(), new_spec);
            } else {
                continue;
            }
            wrote_any = true;
        }
        if wrote_any {
            super::write_manifest_dep_sections(&manifest_path, &manifest)?;
            eprintln!("Updated package.json");
        }
    }

    super::write_and_log_lockfile(&cwd, &graph, &manifest)?;

    // Propagate `--ignore-pnpmfile` / `--pnpmfile` / `--global-pnpmfile`
    // into the chained install. Frozen-prefer normally short-circuits to
    // a no-op fetch/link, but if the lockfile we just wrote falls out of
    // sync (drift, manual edits, future chained calls) the install would
    // re-resolve and re-attach the pnpmfile hook — silently overriding
    // the flags the user passed to `aube update`.
    let mut chained =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer));
    chained.ignore_pnpmfile = args.ignore_pnpmfile;
    chained.pnpmfile = args.pnpmfile.clone();
    chained.global_pnpmfile = args.global_pnpmfile.clone();
    install::run(chained).await?;

    Ok(())
}

struct UpdateSettings {
    ignored: BTreeSet<String>,
    rewrites_specifier: bool,
}

fn resolve_update_settings(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<UpdateSettings> {
    let npmrc_entries = aube_registry::config::load_npmrc_entries(cwd);
    let (_workspace_config, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to read workspace config")?;
    let env = aube_settings::values::process_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env,
        cli: &[],
    };

    let mut ignored: BTreeSet<String> = manifest.update_ignore_dependencies().into_iter().collect();
    if let Some(from_settings) = aube_settings::resolved::update_config_ignore_dependencies(&ctx) {
        ignored.extend(from_settings);
    }
    let rewrites_specifier = aube_settings::resolved::update_rewrites_specifier(&ctx);
    Ok(UpdateSettings {
        ignored,
        rewrites_specifier,
    })
}

async fn run_filtered(
    args: UpdateArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    reject_unsupported_pkg_specs(&args.packages)?;
    let cwd = crate::dirs::cwd()?;
    let (_root, matched) = super::select_workspace_packages(&cwd, filter, "update")?;
    let result = async {
        for pkg in matched {
            super::retarget_cwd(&pkg.dir)?;
            // pnpm's recursive update silently skips packages that aren't
            // declared in a given project's manifest — only updates the
            // ones that match. Without this the fanout hard-errors on the
            // first project that's missing one of the named deps. Compute
            // the per-project arg list by filtering against the project's
            // direct deps, then skip the project entirely if nothing
            // matched (no work to do, no noise).
            let mut per_pkg = args.clone();
            // The top-level `run` already emitted any `--depth` warning;
            // clear it on the per-pkg clone so the recursive call doesn't
            // re-warn once per matched workspace package.
            per_pkg.depth = None;
            if !args.packages.is_empty() {
                let manifest_path = pkg.dir.join("package.json");
                let project_manifest = aube_manifest::PackageJson::from_path(&manifest_path)
                    .map_err(miette::Report::new)
                    .wrap_err_with(|| format!("failed to read {}", manifest_path.display()))?;
                // Mirror the bucket filter from `run` so the declared set
                // ignores entries the inner update would skip — without
                // this an arg that's only a devDep under `--prod` survives
                // the filter here and then hard-errors inside `run` with
                // 'package X is not a dependency'.
                let include_prod = !args.dev;
                let include_dev = !args.prod;
                let include_optional = !args.no_optional && !args.dev;
                let declared: BTreeSet<String> = project_manifest
                    .dependencies
                    .keys()
                    .filter(|_| include_prod)
                    .chain(
                        project_manifest
                            .dev_dependencies
                            .keys()
                            .filter(|_| include_dev),
                    )
                    .chain(
                        project_manifest
                            .optional_dependencies
                            .keys()
                            .filter(|_| include_optional),
                    )
                    .cloned()
                    .collect();
                // Same set ignoring flag filters. Used to distinguish
                // "flag-excluded direct dep" from "truly indirect dep"
                // before the lockfile fallback below — a devDep named
                // under `--prod` is in the lockfile too, so without
                // this guard it would get rescued by
                // `project_lockfile_names` and pushed into the inner
                // `run` as if it were transitive.
                let all_declared: BTreeSet<String> = project_manifest
                    .dependencies
                    .keys()
                    .chain(project_manifest.dev_dependencies.keys())
                    .chain(project_manifest.optional_dependencies.keys())
                    .cloned()
                    .collect();
                // Pull in indirect-dep names from the project's lockfile
                // too — without this, `aube update -r <indirect>@latest`
                // is silently dropped from every project (the indirect
                // isn't in `declared`, the per-project filter empties
                // `per_pkg.packages`, and the inner `run` is never
                // invoked). Prefer the project's own per-project
                // lockfile; fall back to the shared workspace-root
                // lockfile (the one `aube install` writes) when there
                // isn't one yet.
                let project_lockfile_names: BTreeSet<String> =
                    aube_lockfile::parse_lockfile(&pkg.dir, &project_manifest)
                        .ok()
                        .or_else(|| {
                            super::find_workspace_root(&pkg.dir).ok().and_then(|ws| {
                                aube_lockfile::parse_lockfile(&ws, &project_manifest).ok()
                            })
                        })
                        .map(|g| g.packages.values().map(|p| p.name.clone()).collect())
                        .unwrap_or_default();
                // Compare each arg's bare name (stripping any
                // `@<spec>` suffix) against the project's declared deps
                // and lockfile names, but pass the original raw arg into
                // the inner `run` call so it re-parses `<pkg>@latest`
                // consistently.
                per_pkg.packages = args
                    .packages
                    .iter()
                    .filter(|raw| {
                        let name = split_pkg_arg(raw).0;
                        if declared.contains(name) {
                            return true;
                        }
                        // Flag-excluded direct dep — drop instead of
                        // letting `project_lockfile_names` rescue it as
                        // a fake indirect. Mirrors pnpm's silent skip
                        // for `-r --prod <devdep>`.
                        if all_declared.contains(name) {
                            return false;
                        }
                        project_lockfile_names.contains(name)
                    })
                    .cloned()
                    .collect();
                if per_pkg.packages.is_empty() {
                    continue;
                }
            }
            Box::pin(run(
                per_pkg,
                aube_workspace::selector::EffectiveFilter::default(),
            ))
            .await?;
        }
        Ok(())
    }
    .await;
    super::finish_filtered_workspace(&cwd, result)
}

/// Split a `<pkg>@<spec>` arg into a bare name and an optional spec.
/// Scope-aware: `@scope/foo@latest` → (`@scope/foo`, `Some("latest")`).
/// Bare names (`foo`, `@scope/foo`) yield `None`.
fn split_pkg_arg(arg: &str) -> (&str, Option<&str>) {
    let search_start = if arg.starts_with('@') { 1 } else { 0 };
    match arg[search_start..].find('@') {
        Some(rel) => {
            let at = search_start + rel;
            (&arg[..at], Some(&arg[at + 1..]))
        }
        None => (arg, None),
    }
}

/// Reject any `<pkg>@<spec>` arg whose spec isn't `latest`. Other forms
/// (`foo@^2.0.0`, `foo@1.2.3`) are *parsed* by `split_pkg_arg` but the
/// rest of the update path only acts on `@latest` — silently swallowing
/// them would leave the user wondering why their spec didn't take. Hard
/// error early with the supported alternatives so it's discoverable;
/// future work can lift the restriction by threading the spec into the
/// resolver_manifest rewrite + manifest write paths.
fn reject_unsupported_pkg_specs(packages: &[String]) -> miette::Result<()> {
    for raw in packages {
        let (name, spec) = split_pkg_arg(raw);
        if let Some(s) = spec
            && s != "latest"
        {
            return Err(miette!(
                "package spec '{name}@{s}' is not supported by `update` — use `--latest` (or `<pkg>@latest`) to bump past the manifest range, or omit the spec to refresh in-range",
            ));
        }
    }
    Ok(())
}

/// Rewrite a direct-dep specifier to pin `resolved_version`, preserving:
///   - `npm:<alias>@…` aliases round-trip through the `npm:` prefix.
///   - The leading range operator (`^`, `~`, `>=`, `<`, `=`), or `^`
///     when the original was a bare version / dist-tag / missing.
///
/// `exact == true` forces an exact pin regardless of the original
/// prefix (the `--save-exact` / `-E` knob).
fn rewrite_specifier(
    original: &str,
    real_name: &str,
    resolved_version: &str,
    exact: bool,
) -> String {
    let (prefix, is_alias) = if let Some(rest) = original.strip_prefix("npm:") {
        let range = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or("");
        (if exact { "" } else { range_prefix(range) }, true)
    } else {
        (if exact { "" } else { range_prefix(original) }, false)
    };
    let versioned = format!("{prefix}{resolved_version}");
    if is_alias {
        format!("npm:{real_name}@{versioned}")
    } else {
        versioned
    }
}

/// Extract the leading range operator so `rewrite_specifier` can glue
/// it back onto the resolved version. Returns an empty string for an
/// exact pin (`1.2.3`) so `update --latest` doesn't silently flip it
/// into a caret. Dist-tags and unknown shapes default to `^` — there
/// is no operator to preserve and a bare resolved version would
/// accidentally pin what was previously a floating range.
fn range_prefix(spec: &str) -> &'static str {
    let trimmed = spec.trim_start();
    if trimmed.starts_with("^") {
        "^"
    } else if trimmed.starts_with("~") {
        "~"
    } else if trimmed.starts_with(">=") {
        ">="
    } else if trimmed.starts_with("<=") {
        "<="
    } else if trimmed.starts_with('>') {
        ">"
    } else if trimmed.starts_with('<') {
        "<"
    } else if trimmed.starts_with('=') {
        "="
    } else if looks_like_exact_version(trimmed) {
        ""
    } else {
        "^"
    }
}

/// A rough "is this a concrete semver?" check: first char must be a
/// digit and every remaining char must be a member of the semver
/// grammar (digits, `.`, `-`, `+`, ASCII letters for prerelease/build
/// ids). Deliberately permissive — the goal is to tell `1.2.3` apart
/// from `latest`, not to fully validate semver.
fn looks_like_exact_version(spec: &str) -> bool {
    let mut chars = spec.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// If `spec` is an exact-version pin (no caret, tilde, range
/// operator, or non-semver tag), return the version substring.
/// Handles bare `1.2.3` and `=1.2.3` plus the `npm:<real>@<version>`
/// alias form. Returns `None` for ranges, tags, or anything else
/// `looks_like_exact_version` rejects.
///
/// The returned slice is the bare version (already stripped of
/// `npm:`, the `<name>@` alias prefix, the optional `=` operator, and
/// any surrounding whitespace) — suitable for `Version::parse`. It is
/// NOT a valid round-trip back to the original specifier.
fn exact_pin_version(spec: &str) -> Option<&str> {
    let stripped = spec.strip_prefix("npm:").unwrap_or(spec);
    // Drop the optional `<name>@` prefix on alias forms.
    let after_name = stripped
        .rsplit_once('@')
        .map(|(_, v)| v)
        .unwrap_or(stripped);
    let trimmed = after_name.trim_start_matches('=').trim();
    looks_like_exact_version(trimmed).then_some(trimmed)
}
