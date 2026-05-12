use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases};
use miette::miette;
use std::path::{Path, PathBuf};

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    if let Some(meta) = aube_config::is_aube_config_key(&args.key) {
        let location = args.effective_location();
        let mut removed_paths: Vec<PathBuf> = Vec::new();

        // Project-scope deletes also sweep the workspace yaml. We must
        // not short-circuit after a successful yaml removal — a setting
        // can exist in both files (e.g. written to `config.toml` first,
        // overridden in yaml later), and leaving the `config.toml`
        // copy behind would silently restore the deleted value.
        if matches!(location, Location::Project)
            && let Some(yaml_path) = aube_manifest::workspace::workspace_yaml_existing(
                &crate::dirs::project_root_or_cwd()?,
            )
            && aube_config::remove_workspace_yaml_aliases(&yaml_path, meta)?
        {
            removed_paths.push(yaml_path);
        }

        let config_path = match location {
            Location::User | Location::Global => aube_config::user_aube_config_path()?,
            Location::Project => {
                aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
            }
        };
        let mut edit = aube_config::AubeConfigEdit::load(&config_path)?;
        if edit.remove_aliases(&aliases) {
            edit.save(&config_path)?;
            removed_paths.push(config_path.clone());
        }

        if removed_paths.is_empty() {
            return Err(missing_aube_key_error(
                &args.key,
                &aliases,
                &config_path,
                location,
            ));
        }
        let joined = removed_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("deleted {} ({})", args.key, joined);
        return Ok(());
    }

    // Free-form / npm-shared key: try aube config first (where writes
    // since the inverted routing land for unknowns), then fall back to
    // `.npmrc` (where npm-shared keys and legacy writes live).
    let location = args.effective_location();
    let config_path = match location {
        Location::User | Location::Global => aube_config::user_aube_config_path()?,
        Location::Project => {
            aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
        }
    };
    let mut removed_paths: Vec<PathBuf> = Vec::new();
    let mut config_edit = aube_config::AubeConfigEdit::load(&config_path)?;
    if config_edit.remove(&args.key) {
        config_edit.save(&config_path)?;
        removed_paths.push(config_path);
    }

    let npmrc_path = location.path()?;
    if npmrc_path.exists() {
        let mut edit = NpmrcEdit::load(&npmrc_path)?;
        let mut removed = false;
        for alias in &aliases {
            if edit.remove(alias) {
                removed = true;
            }
        }
        if removed {
            edit.save(&npmrc_path)?;
            removed_paths.push(npmrc_path.clone());
        }
    }

    if removed_paths.is_empty() {
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

/// Build the error when an aube-known key isn't in the expected
/// `config.toml`. Surfaces a stale `.npmrc` entry (typically left by an
/// older aube that wrote aube-owned keys there) so the user knows where
/// the value is actually coming from. aube intentionally doesn't modify
/// `.npmrc` for aube-known keys — it's shared with npm/pnpm/yarn — so
/// the message tells the user to clear the line themselves.
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
