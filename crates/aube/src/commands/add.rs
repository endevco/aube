use super::catalogs::{CatalogRewrite, decide_add_rewrite};
use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Package(s) to add
    pub packages: Vec<String>,
    /// Add as dev dependency
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Pin the exact resolved version (no `^` prefix)
    #[arg(short = 'E', long)]
    pub save_exact: bool,
    /// Install the package globally (into the aube/pnpm global directory)
    /// and link its binaries into the global bin directory.
    ///
    /// Mirrors `pnpm add -g`.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Add as optional dependency
    #[arg(short = 'O', long)]
    pub save_optional: bool,
    /// Skip lifecycle scripts (no-op; aube already skips by default)
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Snapshot `package.json` and the lockfile, link the named
    /// packages into `node_modules`, and then restore both files —
    /// so the dependency is usable for the current process but the
    /// project's committed state is untouched.
    ///
    /// Handy for one-off experiments and for scripts that install a
    /// tool transiently. Mirrors `pnpm add --no-save`. Conflicts with
    /// `-g`/`--global`, which has to persist the install to its global
    /// manifest.
    #[arg(long, conflicts_with = "global")]
    pub no_save: bool,
    /// Add as a peer dependency (written to `peerDependencies` in
    /// package.json).
    ///
    /// By convention you usually pair this with `--save-dev` so the
    /// peer is also installed for local development; that's what pnpm
    /// does.
    #[arg(long, conflicts_with = "save_optional")]
    pub save_peer: bool,
    /// Add the dependency to the workspace root's `package.json`,
    /// regardless of the current working directory.
    ///
    /// Walks up from cwd looking for `aube-workspace.yaml`,
    /// `pnpm-workspace.yaml`, or a `package.json` with a `workspaces`
    /// field and runs the add against that directory.
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
    /// Allow `add` to run in a workspace root.
    ///
    /// By default aube refuses to add dependencies to the root
    /// `package.json` of a workspace (a directory containing
    /// `aube-workspace.yaml`, `pnpm-workspace.yaml`, or a `package.json`
    /// with a `workspaces` field) because deps added there end up
    /// shared by every package and usually reflect a mistake. Pass
    /// this flag to opt in. Mirrors `pnpm add -W`.
    #[arg(short = 'W', long)]
    pub ignore_workspace_root_check: bool,
}

/// Parsed result of a package spec like "lodash@^4" or "my-alias@npm:real-pkg@^2".
#[cfg_attr(test, derive(Debug))]
struct ParsedPkgSpec {
    /// The name to use in package.json (alias if provided, otherwise the real name)
    alias: Option<String>,
    /// The real package name on the registry
    name: String,
    /// For `jsr:` specs, the JSR-style name (e.g. `@std/collections`).
    /// `name` has already been translated to the npm-compat form
    /// (`@jsr/std__collections`) so the registry fetch hits
    /// <https://npm.jsr.io>; we keep the original around so the
    /// manifest-write path can round-trip `jsr:…` back into
    /// `package.json`. `None` for non-jsr specs.
    jsr_name: Option<String>,
    /// The version range
    range: String,
    /// `true` when the user wrote an explicit `@<range>` (e.g. `lodash@latest`,
    /// `lodash@^4`). `false` when no version was given and the range was
    /// defaulted to `"latest"` by the parser. Used to decide whether the
    /// configured `tag` setting should override the range.
    has_explicit_range: bool,
}

/// Parse a package spec into its components.
///
/// Supported forms:
/// - `lodash` → name=lodash, range=latest
/// - `lodash@^4` → name=lodash, range=^4
/// - `@scope/pkg@latest` → name=@scope/pkg, range=latest
/// - `npm:real-pkg@^4` → name=real-pkg, range=^4 (no alias)
/// - `my-alias@npm:real-pkg@^4` → alias=my-alias, name=real-pkg, range=^4
/// - `jsr:@std/collections@^1` → alias=@std/collections,
///   name=@jsr/std__collections, range=^1 (jsr translation)
/// - `my-alias@jsr:@std/collections@^1` → alias=my-alias,
///   name=@jsr/std__collections, range=^1
fn parse_pkg_spec(spec: &str) -> miette::Result<ParsedPkgSpec> {
    // Handle full alias form: alias@jsr:@scope/name[@range]
    if let Some(jsr_idx) = spec.find("@jsr:") {
        let before = &spec[..jsr_idx];
        let after_jsr = &spec[jsr_idx + 5..]; // after "jsr:"
        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };
        return parse_jsr_name_range(after_jsr, alias);
    }
    // Handle bare jsr: prefix: jsr:@scope/name[@range]
    if let Some(rest) = spec.strip_prefix("jsr:") {
        return parse_jsr_name_range(rest, None);
    }
    // Handle full alias form: alias@npm:real-pkg@range
    if let Some(npm_idx) = spec.find("@npm:") {
        // Everything before @npm: could be empty (bare npm:pkg@range) or an alias name
        let before = &spec[..npm_idx];
        let after_npm = &spec[npm_idx + 5..]; // after "npm:"

        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };

        // after_npm is "real-pkg@range" or "@scope/pkg@range" or just "real-pkg"
        return Ok(parse_name_range(after_npm, alias));
    }

    // Handle bare npm: prefix: npm:pkg@range
    if let Some(rest) = spec.strip_prefix("npm:") {
        return Ok(parse_name_range(rest, None));
    }

    // Normal spec: name[@range]
    Ok(parse_name_range(spec, None))
}

fn parse_name_range(s: &str, alias: Option<String>) -> ParsedPkgSpec {
    // Handle scoped packages: @scope/name@range
    if s.starts_with('@') {
        if let Some(slash_idx) = s.find('/') {
            let after_slash = &s[slash_idx + 1..];
            if let Some(at_idx) = after_slash.find('@') {
                return ParsedPkgSpec {
                    alias,
                    name: s[..slash_idx + 1 + at_idx].to_string(),
                    jsr_name: None,
                    range: after_slash[at_idx + 1..].to_string(),
                    has_explicit_range: true,
                };
            }
        }
        return ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
        };
    }

    // Unscoped: name@range
    if let Some(at_idx) = s.find('@') {
        ParsedPkgSpec {
            alias,
            name: s[..at_idx].to_string(),
            jsr_name: None,
            range: s[at_idx + 1..].to_string(),
            has_explicit_range: true,
        }
    } else {
        ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
        }
    }
}

/// Parse the `@scope/name[@range]` tail of a `jsr:` spec and translate
/// the JSR-style scoped name into the npm-compat form served at
/// <https://npm.jsr.io>. JSR packages always use scoped names — we
/// reject anything that doesn't start with `@scope/` so the user gets a
/// real error instead of a `latest` lookup against a garbled package
/// name.
///
/// If `alias` is `None`, we default the manifest key to the JSR name
/// itself so `aube add jsr:@std/collections` lands as
/// `"@std/collections": "jsr:…"` — matching pnpm's behavior.
fn parse_jsr_name_range(s: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let inner = parse_name_range(s, None);
    let jsr_name = inner.name.clone();
    let npm_name = aube_registry::jsr::jsr_to_npm_name(&jsr_name).ok_or_else(|| {
        miette!(
            "invalid jsr: spec — expected `jsr:@scope/name[@range]`, got `jsr:{s}` \
             (JSR packages must be scoped, e.g. `jsr:@std/collections`)"
        )
    })?;
    let final_alias = alias.or_else(|| Some(jsr_name.clone()));
    Ok(ParsedPkgSpec {
        alias: final_alias,
        name: npm_name,
        jsr_name: Some(jsr_name),
        range: inner.range,
        has_explicit_range: inner.has_explicit_range,
    })
}

pub async fn run(
    args: AddArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    let AddArgs {
        packages,
        global,
        save_dev,
        save_optional,
        save_exact,
        save_peer,
        workspace,
        ignore_scripts: _,
        no_save,
        ignore_workspace_root_check,
    } = args;
    let packages = &packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }

    if global {
        return run_global(packages).await;
    }

    // `--workspace` / `-w`: redirect the add at the workspace root
    // (directory containing `aube-workspace.yaml` / `pnpm-workspace.yaml`)
    // before anything reads `dirs::cwd()`. We chdir into it so the
    // downstream install pipeline treats the root as the project.
    if workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    let cwd = crate::dirs::project_root()?;

    // Refuse to add into a workspace root unless the caller opts out.
    // Matches pnpm: deps added here are shared by every workspace
    // package and usually reflect a mistake. `-W` /
    // `--ignore-workspace-root-check` bypasses the check, and `-w` /
    // `--workspace` implies the bypass since the user explicitly
    // targeted the root. We trip on a *declared* package-pattern list,
    // not on the materialized glob — an empty `packages/*` directory
    // is still a workspace root the user should opt into. Bare
    // catalog-only yaml is not a workspace root, and a `package.json`
    // without a `workspaces` field isn't either.
    if !ignore_workspace_root_check && !workspace {
        // `WorkspaceConfig::load` already returns an empty `packages`
        // list when no yaml exists, so propagating errors here only
        // surfaces genuine yaml problems (permission denied, malformed
        // YAML) instead of silently letting `add` proceed against what
        // might actually be a workspace root.
        let ws = aube_manifest::WorkspaceConfig::load(&cwd)
            .into_diagnostic()
            .wrap_err("failed to read workspace config")?;
        let yaml_has_packages = !ws.packages.is_empty();
        // `package.json` read errors fall through intentionally: the
        // install pipeline below re-reads and parses the same file and
        // surfaces a richer miette diagnostic pointing at the offending
        // byte. Duplicating that error here would double-report.
        let pkg_json_has_workspaces =
            aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
                .ok()
                .and_then(|m| m.workspaces)
                .is_some_and(|w| !w.patterns().is_empty());
        if yaml_has_packages || pkg_json_has_workspaces {
            return Err(miette!(
                "refusing to add dependencies to the workspace root. \
                 If this is intentional, pass --ignore-workspace-root-check (-W)."
            ));
        }
    }

    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    // 1. Read existing package.json. Snapshot the raw bytes when
    // `--no-save` is in effect so we can restore both the manifest
    // *and* the lockfile after the resolver/install pipeline (both
    // re-read from disk) has done its work — the user gets the new
    // package linked into `node_modules` while their committed
    // project state stays exactly as they wrote it.
    //
    // The lockfile path matches whatever
    // `write_lockfile_preserving_existing` will write to: detect the
    // existing lockfile kind on disk (pnpm, npm, yarn, bun, …) so a
    // project using `pnpm-lock.yaml` doesn't end up with both a
    // restored aube-lock.yaml *and* a leftover modified pnpm-lock.yaml.
    // When no lockfile exists yet the resolver falls back to aube's
    // own format, so we target that path and the restore step deletes
    // it (since `lockfile_bytes` is `None`).
    let lockfile_path = lockfile_path_for_project(&cwd);
    let no_save_snapshot = if no_save {
        let manifest_bytes = std::fs::read(&manifest_path)
            .into_diagnostic()
            .wrap_err("failed to snapshot package.json for --no-save")?;
        let lockfile_bytes = match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        };
        Some(NoSaveSnapshot {
            manifest_bytes,
            lockfile_bytes,
        })
    } else {
        None
    };
    let (manifest, workspace_catalogs) = update_manifest_for_add(
        &cwd,
        packages,
        AddManifestOptions {
            save_dev,
            save_exact,
            save_optional,
            save_peer,
        },
        !no_save,
    )
    .await?;

    // 4 + 5. Resolve, write the lockfile, and run install. We collect
    // the entire pipeline into a single `Result` so the restore step
    // below runs even if the resolver, lockfile writer, or install
    // bails out — without this wrapper a network failure mid-resolve
    // would leave the mutated `package.json` (and any partially
    // written lockfile) on disk, breaking the `--no-save` promise.
    let pipeline_result: miette::Result<()> = async {
        let existing = aube_lockfile::parse_lockfile(&cwd, &manifest).ok();
        let mut resolver = super::build_resolver(&cwd, &manifest, workspace_catalogs);
        let graph = resolver
            .resolve(&manifest, existing.as_ref())
            .await
            .map_err(miette::Report::new)
            .wrap_err("failed to resolve dependencies")?;
        eprintln!("Resolved {} packages", graph.packages.len());

        super::write_and_log_lockfile(&cwd, &graph, &manifest)?;

        install::run(install::InstallOptions::with_mode(
            super::chained_frozen_mode(install::FrozenMode::Prefer),
        ))
        .await
    }
    .await;

    // 6. Under `--no-save`, restore the snapshotted `package.json` and
    // lockfile so neither shows up in `git status`. The user's
    // `node_modules` keeps the freshly linked package — matching
    // pnpm's `--no-save` semantics. We do this regardless of whether
    // the install succeeded so failures still leave the project
    // pristine. If the lockfile didn't exist before, delete the one
    // we just wrote.
    //
    // Both restores are attempted independently — if the manifest
    // write fails, we still try the lockfile restore so the project
    // doesn't get stuck in a half-mutated state. Any errors from this
    // step (and the captured `pipeline_result`) are folded together
    // before returning, so the caller sees the *first* relevant
    // failure rather than silently dropping later ones.
    let restore_errors = if let Some(snapshot) = no_save_snapshot {
        let mut errors: Vec<miette::Report> = Vec::new();
        if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &snapshot.manifest_bytes)
        {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original package.json after --no-save")
                    .unwrap_err(),
            );
        }
        let lockfile_restore = match &snapshot.lockfile_bytes {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!("Restored package.json and lockfile (--no-save)");
        }
        errors
    } else {
        Vec::new()
    };

    // Order matters: surface the pipeline error first when present —
    // it's the root cause and the restore errors are downstream
    // fallout. With no pipeline error, surface the first restore
    // failure (subsequent ones are usually variants of the same
    // filesystem problem).
    pipeline_result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

/// Bytes captured from disk before `aube add --no-save` mutated the
/// manifest and lockfile, used to put both back exactly as the user had
/// them once the install pipeline (which insists on reading from disk)
/// has finished linking `node_modules`.
struct NoSaveSnapshot {
    manifest_bytes: Vec<u8>,
    /// `None` means the lockfile didn't exist before the add — in that
    /// case the restore step deletes whatever the resolver wrote.
    lockfile_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct AddManifestOptions {
    save_dev: bool,
    save_exact: bool,
    save_optional: bool,
    save_peer: bool,
}

impl AddManifestOptions {
    fn from_args(args: &AddArgs) -> Self {
        Self {
            save_dev: args.save_dev,
            save_exact: args.save_exact,
            save_optional: args.save_optional,
            save_peer: args.save_peer,
        }
    }
}

async fn update_manifest_for_add(
    cwd: &Path,
    packages: &[String],
    opts: AddManifestOptions,
    print_updated: bool,
) -> miette::Result<(aube_manifest::PackageJson, super::CatalogMap)> {
    // Resolve settings (savePrefix, tag, catalogMode) from .npmrc /
    // workspace yaml. `catalog_mode` decides whether a newly-added dep
    // that already lives in the default workspace catalog gets rewritten
    // to `catalog:` (see `commands::catalogs::decide_add_rewrite`).
    let (default_tag, default_prefix, catalog_mode) = super::with_settings_ctx(cwd, |ctx| {
        let tag = aube_settings::resolved::tag(ctx);
        let prefix = if opts.save_exact {
            String::new()
        } else {
            let raw = aube_settings::resolved::save_prefix(ctx);
            // Validate: only ^, ~, or empty are valid prefixes.
            match raw.as_str() {
                "^" | "~" | "" => raw,
                _ => {
                    tracing::warn!("ignoring invalid save-prefix={raw:?}, falling back to ^");
                    "^".to_string()
                }
            }
        };
        let catalog_mode = aube_settings::resolved::catalog_mode(ctx);
        (tag, prefix, catalog_mode)
    });
    // Load the workspace catalog map up front — the resolver needs it
    // later, but `catalogMode` consults the default catalog while we
    // build the specifier below. Pass the same map to the resolver to
    // avoid re-reading the workspace file.
    let workspace_catalogs = super::load_workspace_catalogs(cwd)?;
    let default_catalog = workspace_catalogs.get("default");
    let manifest_path = cwd.join("package.json");
    let mut manifest = super::load_manifest(&manifest_path)?;

    // Parse all specs and fetch packuments concurrently.
    let client = std::sync::Arc::new(super::make_client(cwd));
    let parsed: Vec<_> = packages
        .iter()
        .map(|s| {
            let mut spec = parse_pkg_spec(s)?;
            // Replace the implicit default tag with the configured one
            // so that `aube add lodash` respects `tag=next` in .npmrc.
            // Only applies when the user didn't write an explicit version
            // or tag — `aube add lodash@latest` always means `latest`.
            if !spec.has_explicit_range && default_tag != "latest" {
                spec.range = default_tag.clone();
            }
            Ok::<_, miette::Report>(spec)
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let mut handles = Vec::new();
    for spec in &parsed {
        let client = client.clone();
        let name = spec.name.clone();
        let handle = tokio::spawn(async move {
            let packument = client
                .fetch_packument(&name)
                .await
                .map_err(|e| miette!("failed to fetch {name}: {e}"))?;
            Ok::<_, miette::Report>((name, packument))
        });
        handles.push(handle);
    }

    let mut packuments = BTreeMap::new();
    for handle in handles {
        let (name, packument) = handle.await.into_diagnostic()??;
        packuments.insert(name, packument);
    }

    // Resolve each package and update manifest.
    for (spec, orig) in parsed.iter().zip(packages.iter()) {
        let packument = packuments.get(&spec.name).unwrap();
        let pkg_name_for_manifest = spec.alias.as_deref().unwrap_or(&spec.name);

        eprintln!("Resolving {}@{}...", spec.name, spec.range);

        // Resolve "latest" and other dist-tags to a version range.
        let effective_range = if let Some(tagged_version) = packument.dist_tags.get(&spec.range) {
            tagged_version.clone()
        } else {
            spec.range.clone()
        };

        // Find highest matching version. Reused below when a
        // `catalogMode` rewrite redirects resolution to the catalog's
        // range — the display version should match what will actually
        // get installed, not what the user's original range resolved
        // to, so we call this twice when the rewrite fires.
        let highest_satisfying = |range_str: &str| -> Option<String> {
            let range = node_semver::Range::parse(range_str).ok()?;
            let mut versions: Vec<&String> = packument.versions.keys().collect();
            versions.sort_by(|a, b| {
                let va = node_semver::Version::parse(a);
                let vb = node_semver::Version::parse(b);
                match (va, vb) {
                    (Ok(va), Ok(vb)) => vb.cmp(&va),
                    _ => std::cmp::Ordering::Equal,
                }
            });
            versions
                .into_iter()
                .find(|v| node_semver::Version::parse(v).is_ok_and(|p| p.satisfies(&range)))
                .cloned()
        };
        let resolved_version = highest_satisfying(&effective_range)
            .ok_or_else(|| miette!("no version of {} matches {effective_range}", spec.name))?;

        // Build the specifier for package.json.
        // Dist-tags (including "latest") are written as ^version — this matches pnpm's behavior
        // where the lockfile records the resolved version, not the tag name.
        // `--save-exact` drops the `^` so the manifest pins the resolved version.
        //
        // The `npm:` protocol must survive every branch: either the user wrote
        // an alias (`foo@npm:real@range`), which produced `spec.alias`, or they
        // used the bare form (`npm:real@range`), which leaves `alias` empty but
        // keeps the prefix on `orig`. Both cases round-trip back as `npm:...`.
        // `jsr:` is handled separately below, because the manifest form omits
        // the name when the alias equals the JSR name (matching pnpm).
        let is_jsr = spec.jsr_name.is_some();
        let needs_npm_prefix = !is_jsr && (spec.alias.is_some() || orig.starts_with("npm:"));
        let prefix = &default_prefix;
        let pin_to_resolved = spec.range == default_tag
            || packument.dist_tags.contains_key(&spec.range)
            || opts.save_exact;
        // Dist-tags and `--save-exact` both resolve to a concrete version
        // with the configured prefix (empty when `--save-exact`). Non-dist-tag
        // explicit ranges (e.g. `lodash@^4`) are preserved as-is.
        let manual_specifier = if let Some(jsr_name) = spec.jsr_name.as_deref() {
            // jsr:<range> when the manifest key matches the JSR name (the
            // default when the user didn't supply an alias); otherwise we
            // embed the JSR name so the resolver can rebuild the npm-compat
            // name on its next read.
            let effective_range = if pin_to_resolved {
                format!("{prefix}{resolved_version}")
            } else {
                spec.range.clone()
            };
            let alias_matches_jsr_name =
                spec.alias.as_deref() == Some(jsr_name) || spec.alias.is_none();
            if alias_matches_jsr_name {
                format!("jsr:{effective_range}")
            } else {
                format!("jsr:{jsr_name}@{effective_range}")
            }
        } else if pin_to_resolved {
            if needs_npm_prefix {
                format!("npm:{}@{prefix}{resolved_version}", spec.name)
            } else {
                format!("{prefix}{resolved_version}")
            }
        } else if needs_npm_prefix {
            // Preserve npm: protocol for aliases and bare-prefix specs.
            format!("npm:{}@{}", spec.name, spec.range)
        } else {
            spec.range.clone()
        };
        // Apply `catalogMode`. Only the default catalog participates —
        // named catalogs still require the user to write `catalog:<name>`
        // explicitly. `npm:`/alias specs can't be re-expressed as a
        // catalog reference, so they opt out regardless of mode.
        let (specifier, display_version) = match decide_add_rewrite(
            catalog_mode,
            default_catalog,
            &spec.name,
            &spec.range,
            spec.has_explicit_range,
            &resolved_version,
            needs_npm_prefix || is_jsr,
        ) {
            CatalogRewrite::Manual => (manual_specifier, resolved_version.clone()),
            CatalogRewrite::UseDefaultCatalog => {
                // The install will resolve against the catalog's range,
                // not the user's original spec — so the printed version
                // should reflect what actually lands in `node_modules`.
                // `strict` + bare `aube add <pkg>` is the case this
                // matters most for: the user never gave a range, so
                // `resolved_version` comes from `latest` and can easily
                // disagree with what the catalog entry picks. Fall back
                // to `resolved_version` only when the catalog range
                // can't resolve a version from the packument (shouldn't
                // happen in practice, but we'd rather print something
                // than fail the command on a display edge case).
                let cat_range = default_catalog
                    .and_then(|c| c.get(&spec.name))
                    .cloned()
                    .unwrap_or_default();
                let catalog_version = highest_satisfying(&cat_range).unwrap_or_else(|| {
                    tracing::debug!(
                        "catalog range {cat_range:?} for {} did not match any packument version; \
                         falling back to user-resolved version for display",
                        spec.name
                    );
                    resolved_version.clone()
                });
                ("catalog:".to_string(), catalog_version)
            }
            CatalogRewrite::StrictMismatch {
                pkg,
                catalog_range,
                user_range,
            } => {
                return Err(miette!(
                    "catalogMode=strict: {pkg}@{user_range} does not match the \
                     default catalog entry `{catalog_range}`. Update the catalog \
                     or rerun with the catalog range."
                ));
            }
        };

        eprintln!("  + {pkg_name_for_manifest}@{display_version} (specifier: {specifier})");

        // Remove from all dep sections first to avoid duplicates across
        // sections. `--save-peer` intentionally does NOT clear the peer
        // section (see below) — we may end up writing to both peer and
        // dev simultaneously, which is pnpm's `--save-peer` behavior.
        manifest.dependencies.remove(pkg_name_for_manifest);
        manifest.optional_dependencies.remove(pkg_name_for_manifest);
        if !opts.save_peer {
            manifest.peer_dependencies.remove(pkg_name_for_manifest);
        }
        if !(opts.save_peer && opts.save_dev) {
            manifest.dev_dependencies.remove(pkg_name_for_manifest);
        }

        // Add to the appropriate section. When `--save-peer` is paired
        // with `--save-dev`, pnpm writes to BOTH peerDependencies and
        // devDependencies — the peer entry declares what downstream
        // consumers need, and the dev entry makes the local project
        // actually install it for tests and tooling.
        let dep_name = pkg_name_for_manifest.to_string();
        if opts.save_peer {
            manifest
                .peer_dependencies
                .insert(dep_name.clone(), specifier.clone());
            if opts.save_dev {
                manifest.dev_dependencies.insert(dep_name, specifier);
            }
        } else if opts.save_dev {
            manifest.dev_dependencies.insert(dep_name, specifier);
        } else if opts.save_optional {
            manifest.optional_dependencies.insert(dep_name, specifier);
        } else {
            manifest.dependencies.insert(dep_name, specifier);
        }
    }

    // Write the updated package.json. Under `--no-save` callers still
    // write the mutated manifest to disk for the duration of the
    // resolver + install pipeline (both re-read from disk), then
    // restore the original bytes from their snapshot before returning.
    write_manifest_json_for_add(&manifest_path, &manifest)?;
    if print_updated {
        eprintln!("Updated package.json");
    }

    Ok((manifest, workspace_catalogs))
}

fn write_manifest_json_for_add(
    path: &Path,
    manifest: &aube_manifest::PackageJson,
) -> miette::Result<()> {
    let content = std::fs::read_to_string(path)
        .into_diagnostic()
        .wrap_err("failed to read package.json")?;
    let mut json: serde_json::Value = serde_json::from_str(&content)
        .into_diagnostic()
        .wrap_err("failed to parse package.json")?;
    let serde_json::Value::Object(obj) = &mut json else {
        return Err(miette!("package.json must contain a JSON object"));
    };

    sync_dep_section(obj, "dependencies", &manifest.dependencies);
    sync_dep_section(obj, "devDependencies", &manifest.dev_dependencies);
    sync_dep_section(obj, "peerDependencies", &manifest.peer_dependencies);
    sync_dep_section(obj, "optionalDependencies", &manifest.optional_dependencies);

    let json = serde_json::to_string_pretty(&json)
        .into_diagnostic()
        .wrap_err("failed to serialize package.json")?;
    super::write_manifest_atomic(path, format!("{json}\n").as_bytes())
}

fn sync_dep_section(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    deps: &BTreeMap<String, String>,
) {
    if deps.is_empty() {
        obj.remove(key);
        return;
    }

    let section = deps
        .iter()
        .map(|(name, spec)| (name.clone(), serde_json::Value::String(spec.clone())))
        .collect();
    obj.insert(key.to_string(), serde_json::Value::Object(section));
}

/// Resolve the on-disk lockfile path that a normal `add` would write
/// to in `project_dir`. Mirrors the `LockfileKind` -> filename mapping
/// inside `aube_lockfile::write_lockfile_as` so the snapshot/restore
/// path under `--no-save` lines up byte-for-byte with whatever
/// `write_lockfile_preserving_existing` produces, including non-aube
/// lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
/// `bun.lock`, `npm-shrinkwrap.json`). When no lockfile exists yet the
/// resolver falls back to aube's own format.
fn lockfile_path_for_project(project_dir: &std::path::Path) -> std::path::PathBuf {
    use aube_lockfile::LockfileKind;
    let kind =
        aube_lockfile::detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Aube);
    let filename = match kind {
        LockfileKind::Aube => aube_lockfile::aube_lock_filename(project_dir),
        LockfileKind::Pnpm => aube_lockfile::pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    project_dir.join(filename)
}

async fn run_filtered(
    args: AddArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if args.packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    let cwd = crate::dirs::cwd()?;
    // The workspace root — not the child `cwd` — is what owns the
    // lockfile and the project lock in yarn / npm / bun monorepos.
    // Taking the lock or snapshotting the lockfile against `cwd` would
    // target a stale subpackage path, letting `install::run` (which
    // walks up) mutate the real root lockfile and then silently skip
    // the restore under `--no-save`.
    let (root, matched) = super::select_workspace_packages(&cwd, filter, "add")?;
    let _lock = super::take_project_lock(&root)?;

    let mut snapshots = Vec::new();
    let lockfile_path = lockfile_path_for_project(&root);
    let root_lockfile_snapshot = if args.no_save {
        match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        }
    } else {
        None
    };

    let result: miette::Result<()> = async {
        for pkg in &matched {
            let manifest_path = pkg.dir.join("package.json");
            if args.no_save {
                let manifest_bytes = std::fs::read(&manifest_path)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot package.json for --no-save")?;
                snapshots.push((manifest_path.clone(), manifest_bytes));
            }
            update_manifest_for_add(
                &pkg.dir,
                &args.packages,
                AddManifestOptions::from_args(&args),
                !args.no_save,
            )
            .await?;
        }

        let mut install_opts = install::InstallOptions::with_mode(super::chained_frozen_mode(
            install::FrozenMode::Prefer,
        ));
        install_opts.workspace_filter = filter.clone();
        install::run(install_opts).await?;
        Ok(())
    }
    .await;

    let restore_errors = if args.no_save {
        let mut errors: Vec<miette::Report> = Vec::new();
        let restored = snapshots.len();
        for (manifest_path, manifest_bytes) in snapshots {
            if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &manifest_bytes) {
                errors.push(
                    Result::<(), _>::Err(e)
                        .into_diagnostic()
                        .wrap_err_with(|| {
                            format!(
                                "failed to restore original package.json after --no-save at {}",
                                manifest_path.display()
                            )
                        })
                        .unwrap_err(),
                );
            }
        }
        let lockfile_restore = match &root_lockfile_snapshot {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!(
                "Restored {} and lockfile (--no-save)",
                pluralizer::pluralize("package.json file", restored as isize, true)
            );
        }
        errors
    } else {
        Vec::new()
    };

    result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

/// `aube add -g <pkg>...` — install into an isolated global install dir
/// and symlink the resulting binaries into the global bin dir.
///
/// The project-local `run` path assumes a `package.json` in the cwd. The
/// global path deliberately does *not* — it creates a fresh install dir
/// under `<pkg_dir>/<pid>-<ts>`, writes a minimal `package.json` so the
/// normal install pipeline has something to resolve against, chdirs into
/// it, and then re-enters `run` with the local flow. After the install
/// lands we scan the install dir's `node_modules/.bin/` and symlink each
/// bin into `<bin_dir>`.
///
/// The freshly-created install dir is cleaned up if *any* step after
/// creation fails — inner install, manifest re-read, hash pointer, or
/// bin linking. Without this guard every failed `add -g` would leak a
/// subdir that `scan_packages` ignores (no hash symlink) but disk space
/// keeps.
async fn run_global(packages: &[String]) -> miette::Result<()> {
    use super::global;

    let mut layout = global::GlobalLayout::resolve()?;
    let install_dir_raw = global::create_install_dir(&layout.pkg_dir)?;

    // Canonicalize both the install dir and the layout's pkg dir so the
    // comparisons downstream (`find_package`, `remove_package`) see the
    // same form regardless of filesystem-level symlinks. On macOS the
    // default temp dir `/var/folders/...` is itself a symlink to
    // `/private/var/folders/...`, and `scan_packages` always canonicalizes
    // the hash-symlink targets — so without normalizing our side the
    // `!=` / `starts_with` checks all come out wrong and we either leak
    // orphan install dirs or leave duplicate hash pointers behind.
    // Use `dirs::canonicalize` (not `std::fs::canonicalize`) so the
    // result on Windows is a plain drive path, not the `\\?\C:\…`
    // verbatim form. `link_bin_entries` later concatenates this dir
    // into the relative bin-shim target via `%~dp0\{rel}`; a verbatim
    // prefix in `{rel}` produces a path neither `cmd.exe` nor Node can
    // resolve and surfaces as `Cannot find module '<bin>\?\<target>'`.
    let install_dir = crate::dirs::canonicalize(&install_dir_raw)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to canonicalize install dir {}",
                install_dir_raw.display()
            )
        })?;
    if let Ok(canon) = crate::dirs::canonicalize(&layout.pkg_dir) {
        layout.pkg_dir = canon;
    }

    // Everything from here until the final `Ok(())` must run under a
    // cleanup guard so a mid-flight failure doesn't leave an orphan dir
    // or a dangling hash pointer under the global pkg dir. We snapshot
    // the pkg dir's existing hash pointers before running, then on
    // error remove any new pointers that appeared plus the install dir.
    let before: std::collections::HashSet<std::path::PathBuf> = std::fs::read_dir(&layout.pkg_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    let result = run_global_inner(packages, &layout, &install_dir).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&install_dir);
        if let Ok(entries) = std::fs::read_dir(&layout.pkg_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if before.contains(&path) {
                    continue;
                }
                // Only unlink pointers that resolved to our install dir —
                // don't touch pointers for other live global installs.
                // Use `dirs::canonicalize` so the equality check against
                // `install_dir` (also stripped of any Windows `\\?\`
                // verbatim prefix) actually matches.
                if let Ok(target) = crate::dirs::canonicalize(&path)
                    && target == install_dir
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    result
}

async fn run_global_inner(
    packages: &[String],
    layout: &super::global::GlobalLayout,
    install_dir: &std::path::Path,
) -> miette::Result<()> {
    use super::global;

    // Seed a minimal package.json so the resolver has a project to work
    // against. We never persist metadata beyond this; the install dir is
    // throwaway and lives only to host `node_modules/`.
    let seed = serde_json::json!({
        "name": "aube-global",
        "version": "0.0.0",
        "private": true,
    });
    let seed_str = serde_json::to_string_pretty(&seed)
        .into_diagnostic()
        .wrap_err("failed to serialize seed package.json")?;
    aube_util::fs_atomic::atomic_write(
        &install_dir.join("package.json"),
        format!("{seed_str}\n").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err("failed to write seed package.json")?;

    // chdir into the install dir before anything reads `dirs::cwd()` so
    // the whole install pipeline targets the fresh directory. See the
    // invariant note on `run_global` above — this works only because
    // nothing upstream has called `dirs::cwd()` yet.
    std::env::set_current_dir(install_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chdir into {}", install_dir.display()))?;
    crate::dirs::set_cwd(install_dir)?;

    // Build registry map before the inner `run` takes its own view of
    // the config — we need it for the cache key hash.
    let npm_config = aube_registry::config::NpmConfig::load(install_dir);
    let mut registries: BTreeMap<String, String> = BTreeMap::new();
    registries.insert("default".to_string(), npm_config.registry.clone());
    for (scope, url) in &npm_config.scoped_registries {
        registries.insert(scope.clone(), url.clone());
    }

    // Re-enter the local add path inside the throwaway project. Global
    // installs pin the exact resolved version — matches pnpm's
    // `pnpm add -g` behavior (no `^` in the synthetic manifest) and
    // keeps the cache key stable across re-adds.
    let inner = AddArgs {
        packages: packages.to_vec(),
        save_dev: false,
        save_exact: true,
        global: false,
        save_optional: false,
        ignore_scripts: false,
        no_save: false,
        save_peer: false,
        // The throwaway install dir is never a workspace root, but
        // `run_global_inner` is the one place in aube that chdirs
        // after startup — if a future refactor reads `dirs::cwd()`
        // before command dispatch the synthetic `AddArgs` could end
        // up being evaluated against the *caller's* cwd. Opting out
        // of the check here keeps `aube add -g` robust against that
        // regression without relying on the chdir-ordering invariant.
        ignore_workspace_root_check: true,
        workspace: false,
    };
    Box::pin(run(
        inner,
        aube_workspace::selector::EffectiveFilter::default(),
    ))
    .await?;

    // Re-read the install dir's package.json to get the resolved alias
    // list. Anything in `dependencies` at this point was added by the
    // inner run; we stamp a hash pointer on that set.
    let manifest_raw = std::fs::read_to_string(install_dir.join("package.json"))
        .into_diagnostic()
        .wrap_err("failed to re-read install dir package.json")?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_raw)
        .into_diagnostic()
        .wrap_err("failed to parse install dir package.json")?;
    let aliases: Vec<String> = manifest_json
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // Commit the new install *before* tearing down any prior ones. If
    // the hash pointer or bin-link step fails, the outer cleanup guard
    // still wipes the new install dir, but the user's previous global
    // install is untouched — they're never left without a working copy.
    // Capture every prior install whose pointer (or aliases) overlaps
    // the new one *before* we touch the filesystem. We can't scan for
    // priors after the new pointer lands, because the overwrite loses
    // the previous target — `find_package` would return our fresh
    // install instead. Two kinds of prior matter:
    //
    // 1. The install a same-hash pointer used to point at (the caller
    //    re-ran `add -g` with the exact same alias set).
    // 2. Installs that own one of the new aliases under a *different*
    //    hash (alias set grew/shrank).
    let hash = global::cache_key(&aliases, &registries);
    let hash_ptr = global::hash_link(&layout.pkg_dir, &hash);
    let mut priors: Vec<global::GlobalPackageInfo> = Vec::new();
    // `dirs::canonicalize` for the same Windows-prefix reason as above —
    // we compare against `install_dir`, which is itself stripped.
    if let Ok(existing_target) = crate::dirs::canonicalize(&hash_ptr)
        && existing_target != install_dir
    {
        priors.extend(
            global::scan_packages(&layout.pkg_dir)
                .into_iter()
                .filter(|p| p.install_dir == existing_target),
        );
    }
    for alias in &aliases {
        if let Some(existing) = global::find_package(&layout.pkg_dir, alias)
            && existing.install_dir != install_dir
            && existing.hash != hash
            && !priors.iter().any(|p| p.hash == existing.hash)
        {
            priors.push(existing);
        }
    }

    // Commit the new install *before* tearing down the priors. If the
    // hash pointer or bin-link step fails, the outer cleanup guard
    // wipes the new install dir but the priors survive — users never
    // end up with no working copy.
    global::symlink_force(install_dir, &hash_ptr)?;
    // Honor extendNodePath / preferSymlinkedExecutables for global bins too —
    // settings resolved from the user's `.npmrc` via the normal cwd-walking
    // chain starting at the throwaway install dir, which lives under
    // `~/.aube/global/` and will still pick up the user-level `.npmrc`.
    let shim_opts = super::with_settings_ctx(install_dir, |ctx| aube_linker::BinShimOptions {
        extend_node_path: aube_settings::resolved::extend_node_path(ctx),
        prefer_symlinked_executables: aube_settings::resolved::prefer_symlinked_executables(ctx),
    });
    let linked = global::link_bins(install_dir, &layout.bin_dir, &aliases, shim_opts)?;

    // Now safe to drop priors. Errors here are non-fatal — the new
    // install is already live — but we still surface them so the user
    // knows they have leftover state.
    //
    // If a prior shares the new hash, its pointer is already pointing
    // at the *new* install dir (we overwrote it a few lines up). Deleting
    // the pointer in that case would break the live install, so we only
    // wipe the prior's physical dir + bins.
    for prior in &priors {
        let res = if prior.hash == hash {
            let bins = global::bin_names_for(&prior.install_dir, &prior.aliases);
            global::unlink_bins(&prior.install_dir, &layout.bin_dir, &bins);
            std::fs::remove_dir_all(&prior.install_dir)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| miette::miette!("failed to remove prior install dir: {e}"))
        } else {
            global::remove_package(prior, layout)
        };
        if let Err(e) = res {
            eprintln!("warning: failed to remove prior global install: {e}");
        }
    }

    if !linked.is_empty() {
        eprintln!(
            "Linked {} into {}",
            pluralizer::pluralize("bin", linked.len() as isize, true),
            layout.bin_dir.display()
        );
    }

    Ok(())
}

/// Atomic file write. Tempfile in the same dir, fsync, rename over
/// the target. Caller uses this for package.json mutation in add /
/// remove / workspace writes so a crash mid-write cannot corrupt
/// the user's manifest. Rename is atomic on POSIX, on Windows
/// MoveFileEx gives the same guarantee post Win10.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pkg_spec_name_only() {
        let s = parse_pkg_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
        assert!(s.alias.is_none());
        assert!(s.jsr_name.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_with_version() {
        let s = parse_pkg_spec("lodash@^4.17.0").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "^4.17.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_exact_version() {
        let s = parse_pkg_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "4.17.21");
    }

    #[test]
    fn test_parse_pkg_spec_scoped() {
        let s = parse_pkg_spec("@babel/core").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_with_version() {
        let s = parse_pkg_spec("@babel/core@^7.24.0").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "^7.24.0");
    }

    #[test]
    fn test_parse_pkg_spec_dist_tag() {
        let s = parse_pkg_spec("lodash@latest").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_npm_bare() {
        // npm:string-width@^4.2.0 — no alias, just resolves real package
        let s = parse_pkg_spec("npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_full() {
        // string-width-cjs@npm:string-width@^4.2.0
        let s = parse_pkg_spec("string-width-cjs@npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("string-width-cjs"));
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_scoped() {
        // my-react@npm:@preact/compat@^17.0.0
        let s = parse_pkg_spec("my-react@npm:@preact/compat@^17.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-react"));
        assert_eq!(s.name, "@preact/compat");
        assert_eq!(s.range, "^17.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_no_version() {
        // my-lodash@npm:lodash
        let s = parse_pkg_spec("my-lodash@npm:lodash").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-lodash"));
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_no_range() {
        // jsr:@std/collections — default alias is the JSR name itself
        let s = parse_pkg_spec("jsr:@std/collections").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "latest");
        assert!(!s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_with_range() {
        let s = parse_pkg_spec("jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_aliased() {
        let s = parse_pkg_spec("collections@jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_rejects_unscoped() {
        let err = parse_pkg_spec("jsr:collections").unwrap_err();
        assert!(
            err.to_string().contains("JSR packages must be scoped"),
            "unexpected error: {err}"
        );
    }
}
