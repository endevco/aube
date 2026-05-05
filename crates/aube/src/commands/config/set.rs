use super::{Location, NpmrcEdit, aube_config, resolve_aliases, user_npmrc_path};
use clap::Args;

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Setting key (canonical name or `.npmrc` alias).
    pub key: String,

    /// Value to write. Stored verbatim after `key=`.
    pub value: String,

    /// Shortcut for `--location project`.
    #[arg(long, conflicts_with = "location")]
    pub local: bool,

    /// Which `.npmrc` file to write to.
    ///
    /// Defaults to `user`.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

impl SetArgs {
    fn effective_location(&self) -> Location {
        if self.local {
            Location::Project
        } else {
            self.location
        }
    }
}

pub fn run(args: SetArgs) -> miette::Result<()> {
    set_value(&args.key, &args.value, args.effective_location(), true)
}

pub(super) fn set_value(
    key: &str,
    value: &str,
    location: Location,
    report: bool,
) -> miette::Result<()> {
    if matches!(location, Location::User | Location::Global)
        && let Some(meta) = aube_config::is_aube_config_key(key)
    {
        let path = aube_config::user_aube_config_path()?;
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        edit.set(meta, value)?;
        edit.save(&path)?;
        remove_stale_user_npmrc_aliases(key);
        if report {
            eprintln!("set {}={} ({})", meta.name, value, path.display());
        }
        return Ok(());
    }

    let aliases = resolve_aliases(key);
    let write_key = preferred_write_key(key, &aliases);
    let path = location.path()?;
    let mut edit = NpmrcEdit::load(&path)?;
    for alias in &aliases {
        if alias != &write_key {
            edit.remove(alias);
        }
    }
    edit.set(&write_key, value);
    edit.save(&path)?;
    if report {
        eprintln!("set {}={} ({})", write_key, value, path.display());
    }
    Ok(())
}

fn remove_stale_user_npmrc_aliases(key: &str) {
    let Ok(path) = user_npmrc_path() else {
        return;
    };
    if !path.exists() {
        return;
    }
    let Ok(mut edit) = NpmrcEdit::load(&path) else {
        return;
    };
    let mut removed = false;
    for alias in resolve_aliases(key) {
        removed |= edit.remove(&alias);
    }
    if removed {
        let _ = edit.save(&path);
    }
}

pub(super) fn preferred_write_key(input: &str, aliases: &[String]) -> String {
    if aliases.iter().any(|a| a == input) {
        return input.to_string();
    }
    aliases
        .first()
        .cloned()
        .unwrap_or_else(|| input.to_string())
}
