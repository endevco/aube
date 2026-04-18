//! `aube logout` — remove a registry auth token from the user's `~/.npmrc`.
//!
//! Inverse of `aube login`. Strips `//host/:_authToken` (and, when a
//! `--scope` is passed, the matching `@scope:registry` mapping) from the
//! user-level `.npmrc` in place, leaving every other entry untouched.

use crate::commands::npmrc::{NpmrcEdit, registry_host_key, resolve_registry, user_npmrc_path};
use clap::Args;
use miette::miette;

#[derive(Debug, Args)]
pub struct LogoutArgs {
    /// Registry URL to log out of.
    ///
    /// Defaults to the registry resolved from the current project /
    /// user `.npmrc`.
    #[arg(long, value_name = "URL")]
    pub registry: Option<String>,

    /// Scope whose registry mapping should also be removed (e.g. `@myorg`).
    #[arg(long, value_name = "SCOPE")]
    pub scope: Option<String>,
}

pub async fn run(args: LogoutArgs) -> miette::Result<()> {
    if let Some(scope) = &args.scope
        && !scope.starts_with('@')
    {
        return Err(miette!("--scope must start with `@` (got `{scope}`)"));
    }

    let registry = resolve_registry(args.registry.as_deref(), args.scope.as_deref())?;
    let host_key = registry_host_key(&registry);

    let path = user_npmrc_path()?;
    if !path.exists() {
        eprintln!("No ~/.npmrc to edit; nothing to do");
        return Ok(());
    }

    let mut edit = NpmrcEdit::load(&path)?;
    let removed_token = edit.remove(&format!("{host_key}:_authToken"));
    let removed_scope = match &args.scope {
        Some(scope) => edit.remove(&format!("{scope}:registry")),
        None => false,
    };

    if removed_token || removed_scope {
        edit.save(&path)?;
        eprintln!("Logged out of {registry}");
    } else {
        eprintln!("No credentials found for {registry}; nothing to do");
    }
    Ok(())
}
