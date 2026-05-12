use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases};
use miette::miette;
use std::path::{Path, PathBuf};

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let location = args.effective_location();
    let meta = aube_config::is_aube_config_key(&args.key);
    // `set` routes npm-shared keys to `.npmrc` even when they're also
    // known aube settings (e.g. `engineStrict`, `strict-ssl`,
    // `ignore-scripts`). Delete must follow the same routing or it'll
    // skip the file the value actually lives in.
    let key_is_npm_shared = super::is_npm_shared_key(&args.key);

    let mut removed_paths: Vec<PathBuf> = Vec::new();

    // Project scope: sweep the workspace yaml first when the key is a
    // known aube setting. A setting can exist in both yaml and
    // `config.toml` (yaml-overrides-toml on read), so we never
    // short-circuit after a successful yaml removal.
    if let Some(meta) = meta
        && matches!(location, Location::Project)
        && let Some(yaml_path) =
            aube_manifest::workspace::workspace_yaml_existing(&crate::dirs::project_root_or_cwd()?)
        && aube_config::remove_workspace_yaml_aliases(&yaml_path, meta)?
    {
        removed_paths.push(yaml_path);
    }

    // Aube `config.toml`: sweep canonical name + every literal alias
    // + the raw key. The raw key covers free-form writes and the
    // canonical-name catch covers older configs that stored the key
    // under its `meta.name` even when the user typed an alias.
    let config_path = match location {
        Location::User | Location::Global => aube_config::user_aube_config_path()?,
        Location::Project => {
            aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
        }
    };
    let mut config_edit = aube_config::AubeConfigEdit::load(&config_path)?;
    let mut sweep: Vec<String> = aliases.clone();
    if let Some(meta) = meta
        && !sweep.iter().any(|s| s == meta.name)
    {
        sweep.push(meta.name.to_string());
    }
    if !sweep.iter().any(|s| s == &args.key) {
        sweep.push(args.key.clone());
    }
    if config_edit.remove_aliases(&sweep) {
        config_edit.save(&config_path)?;
        removed_paths.push(config_path.clone());
    }

    // `.npmrc`: sweep when the key is npm-shared (the canonical home
    // for those after `set`) or when it's a free-form / unknown key
    // (which may legitimately live in `.npmrc`). Aube-owned settings
    // that are *not* npm-shared (`autoInstallPeers`,
    // `minimumReleaseAge`, …) are intentionally not swept — `.npmrc`
    // is shared with npm/pnpm/yarn and an aube-known entry there is
    // typically a hand-edit or a pre-#517 leftover the user wants to
    // control.
    let should_sweep_npmrc = key_is_npm_shared || meta.is_none();
    let npmrc_path = location.path()?;
    if should_sweep_npmrc && npmrc_path.exists() {
        let mut edit = NpmrcEdit::load(&npmrc_path)?;
        let mut removed = false;
        for alias in &aliases {
            if edit.remove(alias) {
                removed = true;
            }
        }
        if !aliases.iter().any(|a| a == &args.key) && edit.remove(&args.key) {
            removed = true;
        }
        if removed {
            edit.save(&npmrc_path)?;
            removed_paths.push(npmrc_path.clone());
        }
    }

    if removed_paths.is_empty() {
        // For aube-only settings (not in the npm-shared allowlist) we
        // intentionally skipped the `.npmrc` sweep — surface a stale
        // entry there so the user knows what's still shadowing the
        // delete. npm-shared and free-form keys fall through to the
        // simpler "not set anywhere" message.
        if let Some(_meta) = meta
            && !key_is_npm_shared
        {
            return Err(missing_aube_key_error(
                &args.key,
                &aliases,
                &config_path,
                location,
            ));
        }
        return Err(miette!(
            "{} not set in aube config or {}",
            args.key,
            npmrc_path.display()
        ));
    }

    let joined = removed_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("deleted {} ({})", args.key, joined);
    Ok(())
}

/// Build the error when an aube-only setting isn't in the expected
/// `config.toml`. Surfaces a stale `.npmrc` entry (typically left by
/// an older aube that wrote aube-owned keys there) so the user knows
/// where the value is actually coming from. Aube intentionally doesn't
/// modify `.npmrc` for aube-only settings — it's shared with
/// npm/pnpm/yarn — so the message tells the user to clear the line
/// themselves.
fn missing_aube_key_error(
    key: &str,
    aliases: &[String],
    config_path: &Path,
    location: Location,
) -> miette::Report {
    if let Ok(npmrc_path) = location.path()
        && npmrc_path.exists()
        && let Ok(edit) = NpmrcEdit::load(&npmrc_path)
        && edit.entries().iter().any(|(k, _)| aliases.contains(k))
    {
        return miette!(
            "{key} is not set in {} but a stale entry exists in {}.\n\
             aube no longer modifies `.npmrc` for known settings (it's shared with \
             npm/pnpm/yarn) — edit that file directly to remove it, or run \
             `aube config set {key} <value>` to override it from {}.",
            config_path.display(),
            npmrc_path.display(),
            config_path.display(),
        );
    }
    miette!("{key} not set in {}", config_path.display())
}
