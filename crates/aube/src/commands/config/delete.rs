use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases};
use miette::miette;
use std::path::Path;

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    if let Some(meta) = aube_config::is_aube_config_key(&args.key) {
        let location = args.effective_location();
        // Mirror `set --location project`: if the value lives in an
        // existing workspace yaml, remove it from there.
        if matches!(location, Location::Project)
            && let Some(yaml_path) = aube_manifest::workspace::workspace_yaml_existing(
                &crate::dirs::project_root_or_cwd()?,
            )
            && aube_config::remove_workspace_yaml_aliases(&yaml_path, meta)?
        {
            eprintln!("deleted {} ({})", args.key, yaml_path.display());
            return Ok(());
        }
        let path = match location {
            Location::User | Location::Global => aube_config::user_aube_config_path()?,
            Location::Project => {
                aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
            }
        };
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        if !edit.remove_aliases(&aliases) {
            return Err(missing_aube_key_error(&args.key, &aliases, &path, location));
        }
        edit.save(&path)?;
        eprintln!("deleted {} ({})", args.key, path.display());
        return Ok(());
    }

    let path = args.effective_location().path()?;
    if !path.exists() {
        return Err(miette!("no .npmrc at {}", path.display()));
    }
    let mut edit = NpmrcEdit::load(&path)?;
    let mut removed = false;
    for alias in &aliases {
        if edit.remove(alias) {
            removed = true;
        }
    }
    if !removed {
        return Err(miette!("{} not set in {}", args.key, path.display()));
    }
    edit.save(&path)?;
    eprintln!("deleted {} ({})", args.key, path.display());
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
