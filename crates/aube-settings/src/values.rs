//! Resolve typed setting values using the [`meta`](super::meta)
//! registry as the single source of truth for *which* keys map to
//! which setting.
//!
//! The registry (generated at build time from `settings.toml`)
//! records, for every setting:
//!
//!   - its canonical pnpm name
//!   - the `sources.npmrc` keys that populate it from `.npmrc`
//!   - the `sources.workspaceYaml` keys that populate it from
//!     `pnpm-workspace.yaml` / `aube-workspace.yaml`
//!   - the `sources.env` variables that populate it from the shell
//!   - the `sources.cli` flags that populate it from clap
//!   - the type of the value
//!
//! This module is the *value* side of the same registry: given a
//! setting name and a bag of raw source inputs, it walks the metadata
//! and returns the resolved value. Adding a new setting is then a
//! one-place change in `settings.toml` — no corresponding edit in the
//! `NpmConfig::apply` parser or anywhere else.
//!
//! Supported scalar types are `bool`, `string` (including `path` and
//! quoted-union enum strings), `int` (as `u64`), and `list<string>`.
//! Supported sources are `.npmrc` entries, a raw `pnpm-workspace.yaml`
//! map, captured environment variables, and parsed CLI flags.

use crate::meta;

/// Bundle of source inputs consumed by the per-setting typed
/// accessors in [`resolved`]. Each field is a borrowed view so
/// callers can reuse the same owned values across many lookups
/// without cloning.
pub struct ResolveCtx<'a> {
    /// Merged `.npmrc` entries (user-scope first, project-scope last),
    /// as returned by `aube_registry::config::load_npmrc_entries`.
    pub npmrc: &'a [(String, String)],
    /// Raw top-level map from `pnpm-workspace.yaml` /
    /// `aube-workspace.yaml`, as returned by
    /// `aube_manifest::workspace::load_raw`.
    pub workspace_yaml: &'a std::collections::BTreeMap<String, serde_yaml::Value>,
    /// Captured environment variables relevant to settings. In
    /// production this is populated by [`capture_env`]; tests build a
    /// literal slice. Lookups iterate from the end so later entries
    /// win, matching the `.npmrc` convention.
    pub env: &'a [(String, String)],
    /// Parsed CLI flag values for the command being executed. Each
    /// entry is a `(flag_name, value)` pair where `flag_name` matches
    /// a `sources.cli` alias declared in `settings.toml`. Values
    /// should already be normalized to the raw form the type-specific
    /// parser expects (`"true"`/`"false"` for bools, etc).
    pub cli: &'a [(String, String)],
}

impl<'a> ResolveCtx<'a> {
    /// Construct a context that only sees file-based sources.
    /// Convenience for tests and call sites that haven't wired env/cli
    /// through yet.
    pub fn files_only(
        npmrc: &'a [(String, String)],
        workspace_yaml: &'a std::collections::BTreeMap<String, serde_yaml::Value>,
    ) -> Self {
        Self {
            npmrc,
            workspace_yaml,
            env: &[],
            cli: &[],
        }
    }
}

/// Snapshot the process environment into a `(name, value)` list the
/// resolver can walk. Filtering happens at lookup time against the
/// setting's declared `env_vars` aliases, so this captures everything
/// upfront and lets the metadata decide what's relevant.
///
/// Call this once at program start and share the resulting `Vec` with
/// every `ResolveCtx`. Cheaper than re-reading `std::env` for each
/// setting, and makes behavior deterministic across a single command
/// invocation even if subprocesses mutate the parent env (they can't,
/// but the invariant is still easier to reason about).
pub fn capture_env() -> Vec<(String, String)> {
    std::env::vars().collect()
}

/// Typed per-setting accessors generated at build time from
/// `settings.toml`. One function per scalar setting (`bool`,
/// `string`/`path`, quoted-union enum, `int`, `list<string>`). The
/// function signature *is* the type check — `auto_install_peers`
/// returns `bool`, `store_dir` returns `Option<String>`, and
/// calling either on the wrong type is a compile error.
///
/// Precedence is `cli > env > npmrc > workspaceYaml`. The
/// per-setting `precedence` override in `settings.toml` reorders the
/// file-based sources (`npmrc`, `workspaceYaml`) but cannot demote
/// `cli` or `env` off the top — CLI flags and environment variables
/// always win. Settings with concrete parseable defaults return the
/// defaulted value directly; settings whose default is undefined or
/// contextual still return `Option<T>`.
pub mod resolved {
    use super::ResolveCtx;
    include!(concat!(env!("OUT_DIR"), "/settings_resolved.rs"));
}

/// Resolve a `bool` setting by walking its declared `.npmrc` source
/// keys in reverse order (so a later `.npmrc` entry overrides an
/// earlier one). Returns `None` if the metadata entry doesn't exist,
/// the setting isn't a bool, or no source key was found in `entries`.
///
/// `entries` is the raw merged key/value list from
/// `aube_registry::config::load_npmrc_entries`. Precedence is already
/// baked in by the time entries reach this function: project `.npmrc`
/// appears after user `.npmrc`, so iterating from the end gives
/// last-write-wins behavior.
pub(crate) fn bool_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str())
            && let Some(v) = parse_bool(raw)
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `string` setting by walking its declared `.npmrc` source
/// keys in reverse order. Mirrors [`bool_from_npmrc`] but returns the
/// raw value verbatim — trimming and further interpretation are left
/// to the caller, since "string" settings (e.g. `nodeVersion`,
/// registry URLs) have per-setting normalization rules.
pub fn string_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str()) {
            return Some(raw.clone());
        }
    }
    None
}

/// Resolve a `bool` setting from a raw `pnpm-workspace.yaml` map,
/// walking the declared `sources.workspaceYaml` aliases. Returns
/// `None` if no alias is present in the map, the setting isn't a
/// bool, or the value isn't a boolean (or boolean-like string).
///
/// Aliases are walked in the order they appear in
/// `workspace_yaml_keys`; pnpm files don't permit duplicate top-level
/// keys, so precedence among aliases within one file is moot —
/// whichever one is present wins.
pub(crate) fn bool_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            serde_yaml::Value::Bool(b) => return Some(*b),
            serde_yaml::Value::String(s) => {
                if let Some(b) = parse_bool(s) {
                    return Some(b);
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `string` setting from a raw `pnpm-workspace.yaml` map,
/// walking the declared `sources.workspaceYaml` aliases. Returns
/// `None` if no alias is present in the map, the setting isn't a
/// string, or the value is not a YAML string/number/bool scalar.
///
/// Non-string scalars (numbers, booleans) are coerced to their
/// lexical form. Complex values (sequences, mappings) return `None`
/// rather than a bogus rendering.
pub fn string_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            serde_yaml::Value::String(s) => return Some(s.clone()),
            serde_yaml::Value::Number(n) => return Some(n.to_string()),
            serde_yaml::Value::Bool(b) => return Some(b.to_string()),
            _ => {}
        }
    }
    None
}

/// True if this setting's declared type is one the generic string
/// helpers should accept: `string`, `path`, or an enum-style union
/// literal like `"highest" | "time-based"`. Mirrors the type set the
/// build-time generator emits as `Option<String>` accessors.
fn is_stringish(ty: &str) -> bool {
    matches!(ty, "string" | "path") || ty.starts_with('"')
}

/// Resolve an `int` setting from `.npmrc` entries, parsed as `u64`.
/// Mirrors [`bool_from_npmrc`].
pub(crate) fn u64_from_npmrc(setting: &str, entries: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str())
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            return Some(v);
        }
    }
    None
}

/// Resolve an `int` setting from a raw `pnpm-workspace.yaml` map.
/// Accepts YAML integers and stringified numbers.
pub(crate) fn u64_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            serde_yaml::Value::Number(n) => {
                if let Some(u) = n.as_u64() {
                    return Some(u);
                }
            }
            serde_yaml::Value::String(s) => {
                if let Ok(u) = s.trim().parse::<u64>() {
                    return Some(u);
                }
            }
            _ => {}
        }
    }
    None
}

/// Resolve a `list<string>` setting from `.npmrc` entries. pnpm and
/// npm accept either a JSON-ish array (`["a","b"]`) or a
/// comma-separated bare string (`a,b`).
pub(crate) fn string_list_from_npmrc(
    setting: &str,
    entries: &[(String, String)],
) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for (key, raw) in entries.iter().rev() {
        if meta.npmrc_keys.contains(&key.as_str()) {
            return Some(parse_string_list(raw));
        }
    }
    None
}

/// Resolve a `list<string>` setting from a raw workspace yaml map.
/// Accepts YAML sequences of strings, or a single string that gets
/// parsed with [`parse_string_list`] (for pnpm-compat YAML files
/// that stringify the list).
pub(crate) fn string_list_from_workspace_yaml(
    setting: &str,
    raw: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for key in meta.workspace_yaml_keys {
        let Some(val) = workspace_yaml_value(raw, key) else {
            continue;
        };
        match val {
            serde_yaml::Value::Sequence(seq) => {
                let items: Vec<String> = seq
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect();
                return Some(items);
            }
            serde_yaml::Value::String(s) => return Some(parse_string_list(s)),
            _ => {}
        }
    }
    None
}

pub fn workspace_yaml_value<'a>(
    raw: &'a std::collections::BTreeMap<String, serde_yaml::Value>,
    key: &str,
) -> Option<&'a serde_yaml::Value> {
    let mut parts = key.split('.');
    let first = parts.next()?;
    let mut value = raw.get(first)?;
    for part in parts {
        let serde_yaml::Value::Mapping(map) = value else {
            return None;
        };
        value = map.get(serde_yaml::Value::String(part.to_string()))?;
    }
    Some(value)
}

/// Resolve a `bool` setting from a captured environment snapshot,
/// walking the declared `sources.env` aliases in reverse order (later
/// entries win, matching the `.npmrc` helper). Returns `None` on
/// unknown setting, wrong type, or unparseable value.
pub(crate) fn bool_from_env(setting: &str, env: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for (key, raw) in env.iter().rev() {
        if meta.env_vars.contains(&key.as_str())
            && let Some(v) = parse_bool(raw)
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `string` setting from a captured environment snapshot.
pub fn string_from_env(setting: &str, env: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for (key, raw) in env.iter().rev() {
        if meta.env_vars.contains(&key.as_str()) {
            return Some(raw.clone());
        }
    }
    None
}

/// Resolve an `int` setting from a captured environment snapshot.
pub(crate) fn u64_from_env(setting: &str, env: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for (key, raw) in env.iter().rev() {
        if meta.env_vars.contains(&key.as_str())
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `list<string>` setting from a captured environment
/// snapshot. Accepts the same stringified forms as `.npmrc`.
pub(crate) fn string_list_from_env(setting: &str, env: &[(String, String)]) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for (key, raw) in env.iter().rev() {
        if meta.env_vars.contains(&key.as_str()) {
            return Some(parse_string_list(raw));
        }
    }
    None
}

/// Resolve a `bool` setting from a parsed CLI flag bag. The bag
/// entries are whatever each command extracts from its clap struct
/// before building the `ResolveCtx`. Keys must match the
/// `sources.cli` aliases declared in `settings.toml`.
pub(crate) fn bool_from_cli(setting: &str, cli: &[(String, String)]) -> Option<bool> {
    let meta = meta::find(setting)?;
    if meta.type_ != "bool" {
        return None;
    }
    for (key, raw) in cli.iter().rev() {
        if meta.cli_flags.contains(&key.as_str())
            && let Some(v) = parse_bool(raw)
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `string` setting from a parsed CLI flag bag.
pub fn string_from_cli(setting: &str, cli: &[(String, String)]) -> Option<String> {
    let meta = meta::find(setting)?;
    if !is_stringish(meta.type_) {
        return None;
    }
    for (key, raw) in cli.iter().rev() {
        if meta.cli_flags.contains(&key.as_str()) {
            return Some(raw.clone());
        }
    }
    None
}

/// Resolve an `int` setting from a parsed CLI flag bag.
pub(crate) fn u64_from_cli(setting: &str, cli: &[(String, String)]) -> Option<u64> {
    let meta = meta::find(setting)?;
    if meta.type_ != "int" {
        return None;
    }
    for (key, raw) in cli.iter().rev() {
        if meta.cli_flags.contains(&key.as_str())
            && let Ok(v) = raw.trim().parse::<u64>()
        {
            return Some(v);
        }
    }
    None
}

/// Resolve a `list<string>` setting from a parsed CLI flag bag.
pub(crate) fn string_list_from_cli(setting: &str, cli: &[(String, String)]) -> Option<Vec<String>> {
    let meta = meta::find(setting)?;
    if meta.type_ != "list<string>" {
        return None;
    }
    for (key, raw) in cli.iter().rev() {
        if meta.cli_flags.contains(&key.as_str()) {
            return Some(parse_string_list(raw));
        }
    }
    None
}

/// Parse a pnpm/npm-style stringified list. Accepts a JSON-ish array
/// `["a","b"]` or a plain comma-separated list `a,b,c`. Empty entries
/// and surrounding whitespace/quotes are trimmed.
fn parse_string_list(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .split(',')
            .map(|s| {
                s.trim()
                    .trim_matches(|c: char| c == '"' || c == '\'')
                    .to_string()
            })
            .filter(|s| !s.is_empty())
            .collect();
    }
    trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse a `.npmrc`-style boolean. npm/pnpm accept `true`/`false` and
/// the shell-style `"1"` / `"0"`. Anything else returns `None` so the
/// caller's default takes over.
///
/// Public so `aube-registry` and any other crate that hand-parses
/// `.npmrc` scalar values can share the same accept-set — a future
/// tweak (e.g. accepting `yes`/`no`) lands in one place.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn workspace_yaml_value_resolves_dotted_paths() {
        let raw: BTreeMap<String, serde_yaml::Value> =
            serde_yaml::from_str("outer:\n  inner:\n    key: value\n").unwrap();

        assert_eq!(
            workspace_yaml_value(&raw, "outer.inner.key").and_then(|v| v.as_str()),
            Some("value")
        );
        assert!(workspace_yaml_value(&raw, "outer.missing.key").is_none());
    }

    #[test]
    fn resolves_auto_install_peers_kebab_case() {
        let e = entries(&[("auto-install-peers", "false")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(false));
    }

    #[test]
    fn resolves_auto_install_peers_camel_case() {
        // settings.toml lists both spellings in sources.npmrc.
        let e = entries(&[("autoInstallPeers", "true")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(true));
    }

    #[test]
    fn resolves_package_manager_strict_kebab_case() {
        // pnpm's `.npmrc` convention is kebab-case. Real-world yarn/npm
        // projects that want to bypass the guardrail need the kebab
        // spelling to work.
        let e = entries(&[("package-manager-strict", "false")]);
        assert_eq!(bool_from_npmrc("packageManagerStrict", &e), Some(false));
    }

    #[test]
    fn resolves_package_manager_strict_camel_case() {
        let e = entries(&[("packageManagerStrict", "false")]);
        assert_eq!(bool_from_npmrc("packageManagerStrict", &e), Some(false));
    }

    #[test]
    fn resolves_package_manager_strict_version_kebab_case() {
        let e = entries(&[("package-manager-strict-version", "true")]);
        assert_eq!(
            bool_from_npmrc("packageManagerStrictVersion", &e),
            Some(true)
        );
    }

    #[test]
    fn resolves_git_shallow_hosts_kebab_case() {
        // pnpm's `.npmrc` convention is kebab-case; settings.toml
        // must list both spellings so projects copied from a pnpm
        // setup keep working without a rename.
        let e = entries(&[("git-shallow-hosts", "[example.invalid, other.test]")]);
        assert_eq!(
            string_list_from_npmrc("gitShallowHosts", &e),
            Some(vec![
                "example.invalid".to_string(),
                "other.test".to_string(),
            ])
        );
    }

    #[test]
    fn resolves_git_shallow_hosts_camel_case() {
        let e = entries(&[("gitShallowHosts", "example.invalid")]);
        assert_eq!(
            string_list_from_npmrc("gitShallowHosts", &e),
            Some(vec!["example.invalid".to_string()])
        );
    }

    #[test]
    fn returns_none_when_no_key_matches() {
        let e = entries(&[("registry", "https://x.test/")]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), None);
    }

    #[test]
    fn returns_none_for_unknown_setting() {
        let e = entries(&[("auto-install-peers", "false")]);
        assert_eq!(
            bool_from_npmrc("totally-fake-setting", &e),
            None,
            "unknown setting must return None without crashing"
        );
    }

    #[test]
    fn parses_numeric_shell_booleans() {
        assert_eq!(
            bool_from_npmrc("autoInstallPeers", &entries(&[("auto-install-peers", "1")])),
            Some(true)
        );
        assert_eq!(
            bool_from_npmrc("autoInstallPeers", &entries(&[("auto-install-peers", "0")])),
            Some(false)
        );
    }

    #[test]
    fn later_entries_win_over_earlier_ones() {
        // user .npmrc sets false, project .npmrc overrides to true.
        // load_npmrc_entries returns user-first then project-later, so
        // iterating from the end gives the project value.
        let e = entries(&[
            ("auto-install-peers", "false"),
            ("auto-install-peers", "true"),
        ]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(true));
    }

    #[test]
    fn ignores_unparseable_value_and_falls_back() {
        // A garbage value should not poison the lookup — we should
        // fall through to an earlier valid entry.
        let e = entries(&[
            ("auto-install-peers", "false"),
            ("auto-install-peers", "maybe"),
        ]);
        assert_eq!(bool_from_npmrc("autoInstallPeers", &e), Some(false));
    }

    fn raw_yaml(src: &str) -> std::collections::BTreeMap<String, serde_yaml::Value> {
        serde_yaml::from_str(src).expect("test fixture is valid yaml")
    }

    #[test]
    fn workspace_yaml_resolves_bool_field() {
        let m = raw_yaml("autoInstallPeers: false\n");
        assert_eq!(
            bool_from_workspace_yaml("autoInstallPeers", &m),
            Some(false)
        );
    }

    #[test]
    fn workspace_yaml_returns_none_when_absent() {
        let m = raw_yaml("packages:\n  - 'pkgs/*'\n");
        assert_eq!(bool_from_workspace_yaml("autoInstallPeers", &m), None);
    }

    #[test]
    fn workspace_yaml_accepts_stringified_bool() {
        // YAML normally parses bare `true`/`false` as booleans, but a
        // quoted string should still resolve via `parse_bool`.
        let m = raw_yaml("autoInstallPeers: \"false\"\n");
        assert_eq!(
            bool_from_workspace_yaml("autoInstallPeers", &m),
            Some(false)
        );
    }

    #[test]
    fn workspace_yaml_ignores_non_bool_setting() {
        // storeDir is a string setting — the bool helper refuses it.
        let m = raw_yaml("storeDir: /tmp/x\n");
        assert_eq!(bool_from_workspace_yaml("storeDir", &m), None);
    }

    #[test]
    fn workspace_yaml_resolves_string_field() {
        let m = raw_yaml("storeDir: /tmp/my-store\n");
        assert_eq!(
            string_from_workspace_yaml("storeDir", &m),
            Some("/tmp/my-store".to_string())
        );
    }

    #[test]
    fn workspace_yaml_string_ignores_bool_setting() {
        let m = raw_yaml("autoInstallPeers: false\n");
        assert_eq!(string_from_workspace_yaml("autoInstallPeers", &m), None);
    }

    #[test]
    fn workspace_yaml_resolves_nested_string_list_field() {
        let m = raw_yaml("updateConfig:\n  ignoreDependencies:\n    - is-odd\n    - is-even\n");
        assert_eq!(
            string_list_from_workspace_yaml("updateConfig.ignoreDependencies", &m),
            Some(vec!["is-odd".to_string(), "is-even".to_string()])
        );
    }

    #[test]
    fn generated_accessor_walks_npmrc_then_workspace_yaml() {
        // `.npmrc` wins over workspace.yaml.
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let ws = raw_yaml("autoInstallPeers: true\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(!resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn generated_accessor_falls_through_to_workspace_yaml() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("autoInstallPeers: false\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(!resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn generated_accessor_returns_declared_default_when_no_source_matches() {
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn env_resolves_auto_install_peers_via_implicit_alias() {
        // `sources.env` is empty for every setting today, but the
        // build-time generator auto-synthesizes
        // `npm_config_<snake>` (lowercase) and `NPM_CONFIG_<UPPER>`
        // aliases. This test guards both.
        let env_lower = vec![(
            "npm_config_auto_install_peers".to_string(),
            "false".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &env_lower), Some(false));
        let env_upper = vec![(
            "NPM_CONFIG_AUTO_INSTALL_PEERS".to_string(),
            "true".to_string(),
        )];
        assert_eq!(bool_from_env("autoInstallPeers", &env_upper), Some(true));
    }

    #[test]
    fn cli_bag_resolves_resolution_mode_string() {
        // `resolutionMode` is a quoted-union (string) setting with a
        // `sources.cli = ["resolution-mode"]` declaration.
        let cli = vec![("resolution-mode".to_string(), "time-based".to_string())];
        assert_eq!(
            string_from_cli("resolutionMode", &cli),
            Some("time-based".to_string())
        );
    }

    #[test]
    fn cli_beats_env_beats_npmrc_beats_workspace_yaml() {
        // Precedence order is cli > env > npmrc > workspaceYaml. This
        // test hits every layer by setting a unique value at each and
        // asserting the generated accessor returns the CLI value.
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let env = vec![(
            "npm_config_auto_install_peers".to_string(),
            "false".to_string(),
        )];
        let cli = vec![("auto-install-peers".to_string(), "true".to_string())];
        let ctx = ResolveCtx {
            npmrc: &npmrc,
            workspace_yaml: &ws,
            env: &env,
            cli: &cli,
        };
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn env_wins_over_file_sources_when_cli_empty() {
        let npmrc = entries(&[("auto-install-peers", "false")]);
        let ws = raw_yaml("autoInstallPeers: false\n");
        let env = vec![(
            "npm_config_auto_install_peers".to_string(),
            "true".to_string(),
        )];
        let ctx = ResolveCtx {
            npmrc: &npmrc,
            workspace_yaml: &ws,
            env: &env,
            cli: &[],
        };
        assert!(resolved::auto_install_peers(&ctx));
    }

    #[test]
    fn generated_enum_accessor_returns_typed_variant() {
        // `resolutionMode` is an enum-style union with a concrete
        // default. The generator should emit `resolved::ResolutionMode`
        // and a non-optional accessor instead of the old `Option<String>`.
        // Callers match on the variant rather than hand-parsing the raw
        // string.
        let npmrc = entries(&[("resolutionMode", "time-based")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::resolution_mode(&ctx),
            resolved::ResolutionMode::TimeBased
        );
    }

    #[test]
    fn generated_enum_accessor_uses_default_for_unknown_variant() {
        // An unrecognized value should not pollute the result — the
        // accessor falls back to the declared default when it has one.
        let npmrc = entries(&[("nodeLinker", "totally-fake")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Isolated);
    }

    #[test]
    fn generated_enum_accessor_preserves_strict_precedence_on_unknown_value() {
        // Regression: an unrecognized value in a higher-precedence
        // source must NOT fall through to a lower-precedence source.
        // The generator used to apply `from_str_normalized` per-source
        // via `.and_then`, which silently skipped the typo and let the
        // lower source win — a strict precedence violation.
        let npmrc = entries(&[("nodeLinker", "totally-fake")]);
        let ws = raw_yaml("nodeLinker: hoisted\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(
            resolved::node_linker(&ctx),
            resolved::NodeLinker::Isolated,
            ".npmrc had a raw value, even if unparseable — it must win \
             over pnpm-workspace.yaml and fall back to the generated \
             default"
        );
    }

    #[test]
    fn generated_enum_accessor_is_case_insensitive() {
        // pnpm normalizes enum values before matching; the generated
        // `from_str_normalized` mirrors that.
        let npmrc = entries(&[("nodeLinker", "Hoisted")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Hoisted);
    }

    #[test]
    fn generated_enum_accessor_reads_kebab_case_npmrc_alias() {
        // pnpm's `.npmrc` docs use `node-linker=hoisted` (kebab-case).
        // aube must accept it alongside the camelCase `nodeLinker` form —
        // otherwise the setting is silently ignored for anyone copying
        // from pnpm docs.
        let npmrc = entries(&[("node-linker", "hoisted")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::node_linker(&ctx), resolved::NodeLinker::Hoisted);
    }

    #[test]
    fn npmrc_accepts_kebab_alias_for_camel_only_setting() {
        // `virtualStoreDirMaxLength` is declared in settings.toml
        // with the single npmrc key `virtualStoreDirMaxLength`. The
        // generator must auto-synthesize the kebab alias
        // `virtual-store-dir-max-length` so users copying from pnpm's
        // `.npmrc` docs get the expected behaviour.
        let npmrc = entries(&[("virtual-store-dir-max-length", "40")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::virtual_store_dir_max_length(&ctx), Some(40));
    }

    #[test]
    fn npmrc_accepts_camel_alias_for_kebab_only_setting() {
        // Mirror case: `prefer-frozen-lockfile` was declared only in
        // kebab form, so authors writing `preferFrozenLockfile` in
        // `.npmrc` (the pnpm-workspace.yaml spelling) were silently
        // ignored. Auto-synth fills in the camelCase alias too.
        let npmrc = entries(&[("preferFrozenLockfile", "false")]);
        let ws: std::collections::BTreeMap<String, serde_yaml::Value> =
            std::collections::BTreeMap::new();
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::prefer_frozen_lockfile(&ctx), Some(false));
    }

    #[test]
    fn generated_string_accessor_reads_workspace_yaml() {
        // `storeDir` is a string setting with a workspaceYaml source.
        // Before the generator learned about `string_from_workspace_yaml`,
        // this returned `None` — the test guards the fix.
        let npmrc: Vec<(String, String)> = Vec::new();
        let ws = raw_yaml("storeDir: /tmp/from-ws\n");
        let ctx = ResolveCtx::files_only(&npmrc, &ws);
        assert_eq!(resolved::store_dir(&ctx), Some("/tmp/from-ws".to_string()));
    }
}
