use super::{KeyArgs, NpmrcEdit, resolve_aliases};
use miette::miette;

pub type DeleteArgs = KeyArgs;

pub fn run(args: DeleteArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
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
