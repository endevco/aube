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
        let path = aube_config_target(location, meta)?;
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("yaml"))
            && let Some(yaml_key) = aube_config::preferred_workspace_yaml_key(meta)
        {
            aube_config::set_workspace_yaml_value(&path, meta, yaml_key, value)?;
            if report {
                eprintln!("set {}={} ({})", yaml_key, value, path.display());
            }
            return Ok(());
        }
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

/// Decide where to write an aube-known setting for the given location.
/// Project-scope writes prefer an existing workspace yaml when no
/// project `config.toml` has been adopted yet — keeps the per-project
/// config story in a single file. Once `config.toml` exists, all
/// project writes go there (otherwise a yaml write would be silently
/// shadowed by the higher-precedence `config.toml` entry on read).
fn aube_config_target(
    location: Location,
    meta: &aube_settings::meta::SettingMeta,
) -> miette::Result<std::path::PathBuf> {
    match location {
        Location::User | Location::Global => aube_config::user_aube_config_path(),
        Location::Project => {
            let cwd = crate::dirs::project_root_or_cwd()?;
            let config_path = aube_config::project_aube_config_path(&cwd);
            if !config_path.exists()
                && aube_config::preferred_workspace_yaml_key(meta).is_some()
                && let Some(yaml_path) = aube_manifest::workspace::workspace_yaml_existing(&cwd)
            {
                return Ok(yaml_path);
            }
            Ok(config_path)
        }
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
