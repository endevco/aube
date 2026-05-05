use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases, user_npmrc_path};
use miette::miette;

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    if matches!(args.effective_location(), Location::User | Location::Global)
        && aube_config::is_aube_config_key(&args.key).is_some()
    {
        let path = aube_config::user_aube_config_path()?;
        let mut removed = false;
        if path.exists() {
            let mut edit = aube_config::AubeConfigEdit::load(&path)?;
            removed |= edit.remove_aliases(&aliases);
            if removed {
                edit.save(&path)?;
            }
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
            }
        }
        if !removed {
            return Err(miette!("{} not set in {}", args.key, path.display()));
        }
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
