use super::{Location, NpmrcEdit, aube_config, resolve_aliases};
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

    /// Which config location to write to.
    ///
    /// Defaults to `user`. Known aube settings use
    /// `~/.config/aube/config.toml` (user) or
    /// `<cwd>/.config/aube/config.toml` (project); registry/auth and
    /// unknown keys use `~/.npmrc` or `<cwd>/.npmrc` respectively.
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
    if let Some(meta) = aube_config::is_aube_config_key(key) {
        // Project-scope writes prefer an existing workspace yaml so we
        // don't proliferate config files. Settings with no workspace
        // yaml source fall back to project `config.toml`. User-scope
        // writes always land in `~/.config/aube/config.toml`.
        if matches!(location, Location::Project)
            && let Some(yaml_path) = aube_manifest::workspace::workspace_yaml_existing(
                &crate::dirs::project_root_or_cwd()?,
            )
            && let Some(yaml_key) = aube_config::preferred_workspace_yaml_key(meta)
        {
            aube_config::set_workspace_yaml_value(&yaml_path, meta, yaml_key, value)?;
            if report {
                eprintln!("set {}={} ({})", yaml_key, value, yaml_path.display());
            }
            return Ok(());
        }
        let path = match location {
            Location::User | Location::Global => aube_config::user_aube_config_path()?,
            Location::Project => {
                aube_config::project_aube_config_path(&crate::dirs::project_root_or_cwd()?)
            }
        };
        let mut edit = aube_config::AubeConfigEdit::load(&path)?;
        edit.set(meta, value)?;
        edit.save(&path)?;
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

pub(super) fn preferred_write_key(input: &str, aliases: &[String]) -> String {
    if aliases.iter().any(|a| a == input) {
        return input.to_string();
    }
    aliases
        .first()
        .cloned()
        .unwrap_or_else(|| input.to_string())
}
