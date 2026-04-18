//! `aube config` — read/write settings in `.npmrc`.
//!
//! Four subcommands mirroring pnpm's surface: `get`, `set`, `delete`,
//! `list`. All four share a single idea: the set of "known" settings is
//! derived *dynamically* from [`aube_settings::meta::SETTINGS`], which is
//! generated at build time from the workspace-root `settings.toml`. That
//! means adding a new setting to `settings.toml` automatically teaches
//! this command:
//!
//! - which `.npmrc` keys the setting reads from (so `get`/`delete` can
//!   resolve a canonical name like `autoInstallPeers` to its
//!   `auto-install-peers` alias and vice versa),
//! - what the setting's type is (for future value validation),
//! - and a human-readable description / default for `list --all`.
//!
//! We stay permissive about unknown keys on purpose: pnpm's `.npmrc` is
//! a free-form file, and auth-token entries like
//! `//registry.npmjs.org/:_authToken` are written by name rather than by
//! canonical setting. Unknown keys are accepted verbatim — the registry
//! is there to *enhance* the UX for settings aube models, not to gate the
//! file.

use crate::commands::npmrc::{NpmrcEdit, user_npmrc_path};
use aube_settings::meta as settings_meta;
use clap::{Args, Subcommand, ValueEnum};
use miette::miette;
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Delete a key from the selected `.npmrc` file
    #[command(visible_aliases = ["rm", "remove", "unset"])]
    Delete(KeyArgs),
    /// Print the effective value of a key
    Get(GetArgs),
    /// Print every key/value from the selected `.npmrc` file(s)
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Write a key=value pair to the selected `.npmrc` file
    Set(SetArgs),
}

#[derive(Debug, Args)]
pub struct KeyArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Which `.npmrc` file to act on.
    ///
    /// Defaults to `user` (`~/.npmrc`), matching pnpm.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

#[derive(Debug, Args)]
pub struct GetArgs {
    /// The setting key.
    ///
    /// Accepts either a pnpm canonical name (e.g. `autoInstallPeers`)
    /// or an `.npmrc` alias (e.g. `auto-install-peers`).
    pub key: String,

    /// Emit the value as JSON.
    ///
    /// Matches `pnpm config get --json`: a missing key renders as
    /// `undefined`, a found value is JSON-encoded.
    #[arg(long)]
    pub json: bool,

    /// Which `.npmrc` file(s) to read.
    ///
    /// Defaults to `merged` — the last-write-wins view of `~/.npmrc`
    /// then `./.npmrc`, matching what install actually sees. Use
    /// `user` or `project` to restrict the lookup to a single file.
    #[arg(long, value_enum, default_value_t = ListLocation::Merged)]
    pub location: ListLocation,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    /// Setting key (canonical name or `.npmrc` alias).
    pub key: String,

    /// Value to write. Stored verbatim after `key=`.
    pub value: String,

    /// Which `.npmrc` file to write to.
    ///
    /// Defaults to `user`.
    #[arg(long, value_enum, default_value_t = Location::User)]
    pub location: Location,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Also list settings that have no value set — one row per setting
    /// in `settings.toml`, with the default and description rendered.
    ///
    /// Only valid with `--location merged` (the default), since a
    /// per-file view can't distinguish "not set anywhere" from "set in
    /// the other file" and would render misleading defaults.
    #[arg(long)]
    pub all: bool,

    /// Emit all entries as a JSON object keyed by setting name, matching
    /// `pnpm config list --json`.
    ///
    /// Honors `--all` and `--location` the same way the default text
    /// output does.
    #[arg(long)]
    pub json: bool,

    /// Which `.npmrc` file(s) to list.
    ///
    /// `merged` (default) walks `~/.npmrc` then the project's
    /// `.npmrc` with last-write-wins precedence, matching how install
    /// reads config.
    #[arg(long, value_enum, default_value_t = ListLocation::Merged)]
    pub location: ListLocation,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Location {
    /// `~/.npmrc`
    User,
    /// `<cwd>/.npmrc`
    Project,
    /// Alias for `user` — aube has no separate global config file.
    Global,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListLocation {
    /// Merge `~/.npmrc` + project `.npmrc`, last-write-wins (same
    /// precedence install uses).
    Merged,
    /// Only `~/.npmrc`
    User,
    /// Only `<cwd>/.npmrc`
    Project,
    /// Alias for `user`.
    Global,
}

impl Location {
    fn path(self) -> miette::Result<PathBuf> {
        match self {
            Location::User | Location::Global => user_npmrc_path(),
            Location::Project => Ok(crate::dirs::project_root_or_cwd()?.join(".npmrc")),
        }
    }
}

pub async fn run(args: ConfigArgs) -> miette::Result<()> {
    match args.command {
        ConfigCommand::Get(a) => get(a),
        ConfigCommand::Set(a) => set(a),
        ConfigCommand::Delete(a) => delete(a),
        ConfigCommand::List(a) => list(a),
    }
}

/// True for entries in `SettingMeta::npmrc_keys` that are real, literal
/// `.npmrc` keys — not pattern templates like `@scope:registry` or
/// `//host/:_authToken`. The `registries` setting documents both the
/// literal key (`registry`) and the templates side-by-side; this filter
/// keeps `resolve_aliases` from treating the templates as sibling
/// aliases of `registry`, which would let `config set @scope:registry …`
/// silently delete the user's real `registry` entry.
fn is_literal_alias(key: &str) -> bool {
    !key.starts_with("//") && !key.contains(':')
}

/// Expand a user-supplied key into the full set of `.npmrc` aliases it
/// covers. If the input matches a setting's canonical name, return its
/// literal `npmrc_keys`. If the input matches one of those literal
/// aliases, return the same set (so `get auto-install-peers` also sees
/// a value written under `autoInstallPeers`). Pattern-template entries
/// in `npmrc_keys` (e.g. `@scope:registry`) are filtered out — see
/// [`is_literal_alias`]. Otherwise fall back to the literal input.
fn resolve_aliases(key: &str) -> Vec<String> {
    if let Some(meta) = settings_meta::find(key) {
        let literals = literal_aliases(meta.npmrc_keys);
        if !literals.is_empty() {
            return literals;
        }
    }
    for meta in settings_meta::all() {
        let literals = literal_aliases(meta.npmrc_keys);
        if literals.iter().any(|a| a == key) {
            return literals;
        }
    }
    vec![key.to_string()]
}

fn literal_aliases(keys: &[&'static str]) -> Vec<String> {
    keys.iter()
        .filter(|k| is_literal_alias(k))
        .map(|s| s.to_string())
        .collect()
}

/// Pick which alias a `set` should write to. Prefers the user-typed key
/// if it's already one of the setting's recognized aliases (so
/// `config set auto-install-peers true` doesn't rewrite it to
/// `autoInstallPeers`), otherwise the first declared alias, otherwise
/// the literal input.
fn preferred_write_key(input: &str, aliases: &[String]) -> String {
    if aliases.iter().any(|a| a == input) {
        return input.to_string();
    }
    aliases
        .first()
        .cloned()
        .unwrap_or_else(|| input.to_string())
}

pub fn get(args: GetArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let cwd = crate::dirs::project_root_or_cwd()?;
    // `merged` is the default because "what would install actually
    // see?" is the useful question most of the time. `user` / `project`
    // exist so callers can scope the lookup to a single file (e.g. to
    // answer "is this key set at the project level?") — dispatching
    // per-location here matches what `list` does.
    let entries: Vec<(String, String)> = match args.location {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => read_single(&user_npmrc_path()?)?,
        ListLocation::Project => read_single(&cwd.join(".npmrc"))?,
    };

    // Walk in reverse so the last-written entry wins, matching the
    // precedence install uses.
    for (k, v) in entries.iter().rev() {
        if aliases.iter().any(|a| a == k) {
            if args.json {
                println!("{}", serde_json::Value::String(v.clone()));
            } else {
                println!("{v}");
            }
            return Ok(());
        }
    }
    // pnpm prints `undefined` for a missing key; we match (in both
    // text and JSON modes — `undefined` isn't valid JSON, but that's
    // what pnpm emits and downstream tooling expects it).
    println!("undefined");
    Ok(())
}

pub fn set(args: SetArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let write_key = preferred_write_key(&args.key, &aliases);
    let path = args.location.path()?;
    let mut edit = NpmrcEdit::load(&path)?;
    // Remove every known alias before writing so that a prior
    // `auto-install-peers=false` doesn't linger after the user runs
    // `config set autoInstallPeers true`.
    for alias in &aliases {
        if alias != &write_key {
            edit.remove(alias);
        }
    }
    edit.set(&write_key, &args.value);
    edit.save(&path)?;
    eprintln!("set {}={} ({})", write_key, args.value, path.display());
    Ok(())
}

fn delete(args: KeyArgs) -> miette::Result<()> {
    let aliases = resolve_aliases(&args.key);
    let path = args.location.path()?;
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

fn list(args: ListArgs) -> miette::Result<()> {
    // `--all` only makes sense against the merged view. With a per-file
    // location, a key set in the *other* file looks "unset" to this
    // command and we'd print its default alongside a real value, which
    // is misleading.
    if args.all && !matches!(args.location, ListLocation::Merged) {
        return Err(miette!(
            "--all is only supported with --location merged (the default)"
        ));
    }
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries: Vec<(String, String)> = match args.location {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => read_single(&user_npmrc_path()?)?,
        ListLocation::Project => read_single(&cwd.join(".npmrc"))?,
    };

    // Collapse duplicate keys to the last-written value so `list` matches
    // what `get` would print for each key. Cross-alias duplicates collapse
    // too: a setting written under `auto-install-peers` in one file and
    // `autoInstallPeers` in another is a single entry in the output,
    // keyed on the setting's primary alias, with the value of whichever
    // spelling was encountered last. Without this canonicalization,
    // `get auto-install-peers` and `list` could disagree — `get` resolves
    // all aliases, `list` would show both rows unchanged.
    let mut seen: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (k, v) in entries {
        let canonical = canonical_list_key(&k);
        seen.insert(canonical, v);
    }

    // Keys filled in from `settings.toml` defaults rather than from the
    // `.npmrc` file(s). Tracked separately from `seen` so the text output
    // can append a `(default)` annotation while the JSON output emits a
    // plain value — matching `pnpm config list --json --all`, which is
    // shape-clean (no baked-in annotation).
    let mut defaults: std::collections::HashSet<String> = std::collections::HashSet::new();
    if args.all {
        // Merge in every known setting from the registry so the user can
        // see defaults for keys they've never set. Env-only settings
        // (those with no `.npmrc` source) are skipped, and pattern-template
        // entries in `npmrc_keys` are filtered out so `registries` doesn't
        // print a row for `@scope:registry (default)`.
        for meta in settings_meta::all() {
            let literals = literal_aliases(meta.npmrc_keys);
            let Some(primary) = literals.first().cloned() else {
                continue;
            };
            // Don't overwrite a real value with the default.
            if !literals.iter().any(|k| seen.contains_key(k)) {
                seen.insert(primary.clone(), meta.default.to_string());
                defaults.insert(primary);
            }
        }
    }

    if args.json {
        let obj: serde_json::Map<String, serde_json::Value> = seen
            .into_iter()
            .map(|(k, v)| (k, serde_json::Value::String(v)))
            .collect();
        let out = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
            .map_err(|e| miette!("failed to serialize config: {e}"))?;
        println!("{out}");
    } else {
        for (k, v) in &seen {
            if defaults.contains(k) {
                println!("{k}={v} (default)");
            } else {
                println!("{k}={v}");
            }
        }
    }
    Ok(())
}

/// Map a raw `.npmrc` key onto the form `list` should display it under.
/// For a key that matches a setting's `.npmrc` alias set (in either
/// direction), the primary alias (`npmrc_keys[0]`) is returned so that
/// cross-spelling duplicates collapse into a single row. Unknown keys
/// (auth tokens, scoped registries, anything not modeled in
/// `settings.toml`) pass through verbatim.
fn canonical_list_key(key: &str) -> String {
    let aliases = resolve_aliases(key);
    if aliases.len() == 1 && aliases[0] == key {
        // Unknown key: `resolve_aliases` returned the identity fallback.
        return key.to_string();
    }
    aliases.first().cloned().unwrap_or_else(|| key.to_string())
}

/// Read `~/.npmrc` then `<cwd>/.npmrc` and return every entry in file
/// order (user-first, project-second) so a later duplicate wins on a
/// reverse walk. Unlike the install-time reader in `aube_registry`,
/// this path deliberately does **not** substitute `${VAR}` references:
/// `config` commands inspect and mutate the file verbatim, so echoing
/// a resolved token would both surprise users and leak secrets. A
/// per-file `--location` read goes through the same `NpmrcEdit`-based
/// parser, which keeps merged and per-file output consistent.
fn read_merged(cwd: &Path) -> miette::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    if let Ok(user) = user_npmrc_path() {
        out.extend(read_single(&user)?);
    }
    out.extend(read_single(&cwd.join(".npmrc"))?);
    Ok(out)
}

fn read_single(path: &std::path::Path) -> miette::Result<Vec<(String, String)>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    // Reuse NpmrcEdit's parser so we share the single source of truth
    // for .npmrc line handling (comments, blanks, key=value splitting).
    let edit = NpmrcEdit::load(path)?;
    Ok(edit.entries())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_list_key_collapses_alias_to_primary() {
        // autoInstallPeers has two declared aliases, with
        // `auto-install-peers` first → both spellings should canonicalize
        // to the primary.
        assert_eq!(canonical_list_key("autoInstallPeers"), "auto-install-peers");
        assert_eq!(
            canonical_list_key("auto-install-peers"),
            "auto-install-peers"
        );
    }

    #[test]
    fn canonical_list_key_passthrough_for_unknown_key() {
        // Unknown keys (auth tokens, scoped registries) pass through so
        // `config list` still shows them under their literal name.
        assert_eq!(
            canonical_list_key("//registry.example.com/:_authToken"),
            "//registry.example.com/:_authToken"
        );
    }

    #[test]
    fn resolve_aliases_canonical_name() {
        // autoInstallPeers declares both camelCase and kebab-case aliases.
        let aliases = resolve_aliases("autoInstallPeers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_from_alias() {
        // Starting from the alias side must return the same set.
        let aliases = resolve_aliases("auto-install-peers");
        assert!(aliases.iter().any(|a| a == "auto-install-peers"));
        assert!(aliases.iter().any(|a| a == "autoInstallPeers"));
    }

    #[test]
    fn resolve_aliases_registry_excludes_template_keys() {
        // `registries` declares both the literal `registry` key and three
        // pattern templates (`@scope:registry`, `//host/:_authToken`,
        // `//host/:_auth`). Only the literal should land in the alias set
        // — otherwise `config set @scope:registry …` would resolve to the
        // registries group and `config set` would clobber the user's
        // real `registry` entry via the stale-alias removal pass.
        let aliases = resolve_aliases("registry");
        assert_eq!(aliases, vec!["registry".to_string()]);
        for a in &aliases {
            assert!(is_literal_alias(a), "leaked template alias: {a}");
        }
    }

    #[test]
    fn resolve_aliases_template_input_is_identity() {
        // A user typing the template verbatim should be treated as an
        // unknown key (identity fallback), not silently mapped onto the
        // registries alias group.
        for template in [
            "@scope:registry",
            "//registry.example.com/:_authToken",
            "//registry.example.com/:_auth",
        ] {
            assert_eq!(
                resolve_aliases(template),
                vec![template.to_string()],
                "{template} should be identity, not registries-grouped"
            );
        }
    }

    #[test]
    fn is_literal_alias_recognizes_templates() {
        assert!(is_literal_alias("registry"));
        assert!(is_literal_alias("auto-install-peers"));
        assert!(!is_literal_alias("@scope:registry"));
        assert!(!is_literal_alias("//host/:_authToken"));
        assert!(!is_literal_alias("//host/:_auth"));
    }

    #[test]
    fn resolve_aliases_unknown_key_is_identity() {
        let aliases = resolve_aliases("//registry.example.com/:_authToken");
        assert_eq!(
            aliases,
            vec!["//registry.example.com/:_authToken".to_string()]
        );
    }

    #[test]
    fn preferred_write_key_keeps_user_typed_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        assert_eq!(
            preferred_write_key("autoInstallPeers", &aliases),
            "autoInstallPeers"
        );
        assert_eq!(
            preferred_write_key("auto-install-peers", &aliases),
            "auto-install-peers"
        );
    }

    #[test]
    fn preferred_write_key_falls_back_to_first_alias() {
        let aliases = vec![
            "auto-install-peers".to_string(),
            "autoInstallPeers".to_string(),
        ];
        // Input isn't an alias → write to the first declared one.
        assert_eq!(
            preferred_write_key("something-else", &aliases),
            "auto-install-peers"
        );
    }
}
