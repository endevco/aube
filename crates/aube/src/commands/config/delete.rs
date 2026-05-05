use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases, user_npmrc_path};
use miette::miette;
use std::path::PathBuf;

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    if matches!(args.effective_location(), Location::User | Location::Global)
        && aube_config::is_aube_config_key(&args.key).is_some()
    {
        let path = aube_config::user_aube_config_path()?;
        let mut removed_paths: Vec<PathBuf> = Vec::new();
        let mut removed = false;
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        if edit.remove_aliases(&aliases) {
            edit.save(&path)?;
            removed = true;
            removed_paths.push(path.clone());
        }
        if let Ok(npmrc_path) = user_npmrc_path()
            && npmrc_path.exists()
        {
            let mut edit = NpmrcEdit::load(&npmrc_path)?;
            let mut npmrc_removed = false;
            for alias in &aliases {
                npmrc_removed |= edit.remove(alias);
            }
            if npmrc_removed {
                removed = true;
                edit.save(&npmrc_path)?;
                removed_paths.push(npmrc_path);
            }
        }
        if !removed {
            return Err(miette!(
                "{} not set in {} or user .npmrc",
                args.key,
                path.display()
            ));
        }
        let paths = removed_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("deleted {} ({})", args.key, paths);
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
