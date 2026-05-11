use super::{KeyArgs, Location, NpmrcEdit, aube_config, resolve_aliases};
use miette::miette;

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    if matches!(args.effective_location(), Location::User | Location::Global)
        && aube_config::is_aube_config_key(&args.key).is_some()
    {
        let path = aube_config::user_aube_config_path()?;
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        if !edit.remove_aliases(&aliases) {
            return Err(miette!("{} not set in {}", args.key, path.display()));
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
