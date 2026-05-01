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
    /// Update past the manifest range: rewrite `package.json`
    /// specifiers to match the newly resolved versions (the registry's
    /// `latest` dist-tag, clamped by `minimumReleaseAge` /
    /// `resolution-mode` as usual).
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
    /// Parsed for pnpm compatibility.
    #[arg(long)]
    pub depth: Option<String>,
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
}

pub async fn run(
    args: UpdateArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    let _ = args.ignore_scripts; // parity no-op: dep scripts already gated by allowBuilds
    let _ = (
        args.global,
        args.workspace,
        args.interactive,
        args.depth.as_ref(),
    );
    if !filter.is_empty() {
        return run_filtered(args, &filter).await;
    }
    let packages = &args.packages[..];
    let latest = args.latest;
    let no_save = args.no_save;
    let cwd = crate::dirs::project_root()?;
    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    let mut manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .map_err(miette::Report::new)
        .wrap_err("failed to read package.json")?;
    let ignored_updates = resolve_update_ignore_dependencies(&cwd, &manifest)?;

    let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();

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

    // Determine which packages to update
    let update_all = packages.is_empty();
    let manifest_keys_to_update: Vec<String> = if update_all {
        all_specifiers
            .keys()
            .filter(|name| !ignored_updates.contains(name.as_str()))
            .cloned()
            .collect()
    } else {
        for name in packages {
            if !all_specifiers.contains_key(name.as_str()) {
                return Err(miette!("package '{name}' is not a dependency"));
            }
            if ignored_updates.contains(name.as_str()) {
                return Err(miette!(
                    "package '{name}' is ignored by updateConfig.ignoreDependencies"
                ));
            }
        }
        packages
            .iter()
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
        eprintln!("Updating: {}", packages.join(", "));
    }

    // `--latest`: rewrite each targeted direct-dep specifier to
    // `latest` (preserving any `npm:` alias prefix) on a *clone* of
    // the manifest that we hand to the resolver. Mutating the real
    // in-memory manifest would corrupt `package.json` if we then
    // bailed out — and if any package fails to resolve, the literal
    // string `"latest"` would stick. `workspace:` specifiers are
    // skipped: they refer to local workspace packages, not registry
    // versions, so rewriting them to `latest` would send the
    // resolver hunting on the registry for what is actually a
    // sibling package.
    let resolver_manifest = if latest {
        let mut m = manifest.clone();
        for key in &manifest_keys_to_update {
            let real_name = resolve_real_name(key);
            let original = all_specifiers.get(key).map(String::as_str).unwrap_or("");
            if aube_util::pkg::is_workspace_spec(original) {
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
    // locked one. Aliased direct deps (`"alias": "npm:real@x"`) live in
    // the lockfile graph with `pkg.name == "alias"` (the manifest key),
    // not the real name — without the manifest_keys check below the
    // resolver would keep the locked alias version under `--latest`.
    let filtered_existing = existing.as_ref().map(|graph| {
        let mut filtered = graph.clone();
        let manifest_keys: std::collections::HashSet<&str> =
            manifest_keys_to_update.iter().map(String::as_str).collect();
        filtered.packages.retain(|_, pkg| {
            !real_names_to_update.contains(&pkg.name) && !manifest_keys.contains(pkg.name.as_str())
        });
        filtered
    });

    // Re-resolve the full dependency tree. Wire the pnpmfile in so
    // `readPackage` mutations apply during update (not just first
    // install) and `afterAllResolved` gets a chance to rewrite the
    // graph before we hand it to the lockfile writer; without this,
    // `aube install` runs in frozen-prefer mode below and never
    // re-evaluates the hook.
    let pnpmfile_path = (!args.ignore_pnpmfile)
        .then(|| {
            let (ws, _) = aube_manifest::workspace::load_both(&cwd).unwrap_or_default();
            crate::pnpmfile::detect(&cwd, ws.pnpmfile_path.as_deref())
        })
        .flatten();
    if let Some(p) = pnpmfile_path.as_deref() {
        super::run_pnpmfile_pre_resolution(p, &cwd, existing.as_ref()).await?;
    }
    let read_package_host = match pnpmfile_path.as_deref() {
        Some(p) => crate::pnpmfile::ReadPackageHost::spawn(p)
            .await
            .wrap_err("failed to start pnpmfile readPackage host")?,
        None => None,
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
    if let Some(p) = pnpmfile_path.as_deref() {
        crate::pnpmfile::run_after_all_resolved(p, &mut graph)
            .await
            .wrap_err("pnpmfile afterAllResolved hook failed")?;
    }

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

    // `--latest`: rewrite each targeted direct dep in the real
    // `package.json` to pin the resolved version, preserving the
    // user's existing prefix (`^`/`~`/exact) and any `npm:` alias.
    // Skip `workspace:` specifiers (sibling packages) and skip deps
    // that resolved to the same spec they already had, so an idempotent
    // `update --latest` doesn't rewrite the manifest for no reason.
    //
    // `--no-save` short-circuits the manifest rewrite: the resolver
    // already pulled in the new versions for the lockfile above, so we
    // just skip persisting any range bumps to `package.json`.
    if latest {
        if no_save {
            eprintln!("Skipping package.json update (--no-save)");
        } else {
            let mut wrote_any = false;
            for key in &manifest_keys_to_update {
                let real_name = resolve_real_name(key);
                let original = all_specifiers.get(key).cloned().unwrap_or_default();
                if aube_util::pkg::is_workspace_spec(&original) {
                    continue;
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
    }

    super::write_and_log_lockfile(&cwd, &graph, &manifest)?;

    // Propagate `--ignore-pnpmfile` into the chained install. Frozen-prefer
    // normally short-circuits to a no-op fetch/link, but if the lockfile
    // we just wrote falls out of sync (drift, manual edits, future
    // chained calls) the install would re-resolve and re-attach the
    // pnpmfile hook — silently overriding the flag the user passed to
    // `aube update`.
    let mut chained =
        install::InstallOptions::with_mode(super::chained_frozen_mode(install::FrozenMode::Prefer));
    chained.ignore_pnpmfile = args.ignore_pnpmfile;
    install::run(chained).await?;

    Ok(())
}

fn resolve_update_ignore_dependencies(
    cwd: &std::path::Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<BTreeSet<String>> {
    let npmrc_entries = aube_registry::config::load_npmrc_entries(cwd);
    let (_workspace_config, raw_workspace) = aube_manifest::workspace::load_both(cwd)
        .into_diagnostic()
        .wrap_err("failed to read workspace config")?;
    let env = aube_settings::values::capture_env();
    let ctx = aube_settings::ResolveCtx {
        npmrc: &npmrc_entries,
        workspace_yaml: &raw_workspace,
        env: &env,
        cli: &[],
    };

    let mut ignored: BTreeSet<String> = manifest.update_ignore_dependencies().into_iter().collect();
    if let Some(from_settings) = aube_settings::resolved::update_config_ignore_dependencies(&ctx) {
        ignored.extend(from_settings);
    }
    Ok(ignored)
}

async fn run_filtered(
    args: UpdateArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
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
                per_pkg.packages = args
                    .packages
                    .iter()
                    .filter(|name| declared.contains(name.as_str()))
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
