use super::catalogs::{CatalogRewrite, CatalogUpsert, decide_add_rewrite, range_compatible};
use super::install;
use clap::Args;
use miette::{Context, IntoDiagnostic, miette};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Args)]
pub struct AddArgs {
    /// Package(s) to add
    pub packages: Vec<String>,
    /// Add as dev dependency
    #[arg(short = 'D', long)]
    pub save_dev: bool,
    /// Pin the exact resolved version (no `^` prefix)
    #[arg(short = 'E', long)]
    pub save_exact: bool,
    /// Install the package globally.
    ///
    /// Installs into the aube/pnpm global directory and links its
    /// binaries into the global bin directory. Mirrors `pnpm add -g`.
    #[arg(short = 'g', long)]
    pub global: bool,
    /// Add as optional dependency
    #[arg(short = 'O', long)]
    pub save_optional: bool,
    /// Pre-approve a dependency's lifecycle scripts as part of the add.
    ///
    /// `--allow-build=<pkg>` writes `allowBuilds: { <pkg>: true }` into
    /// the workspace yaml (or `package.json#aube.allowBuilds`) before
    /// the install runs, so the named package's `preinstall` /
    /// `install` / `postinstall` scripts execute on this invocation.
    /// Repeatable — pass the flag once per package.
    ///
    /// Errors when `<pkg>` is already on the allowlist with `false` —
    /// promoting an explicit deny should be a deliberate edit, not a
    /// silent flip. Mirrors `pnpm add --allow-build=<pkg>`.
    ///
    /// Conflicts with `--no-save`: when a workspace yaml exists, the
    /// approval lands there, but `--no-save`'s restore path only
    /// snapshots `package.json` + the lockfile — combining the two
    /// would silently leave an orphaned approval behind. Same
    /// reasoning as `--save-catalog`'s `--no-save` conflict.
    ///
    /// Both bare `--allow-build` and the explicit empty form
    /// `--allow-build=` are rejected with pnpm's verbatim error so
    /// users porting pnpm scripts see the same diagnostic. The
    /// `num_args` plus `default_missing_value` pair routes the bare
    /// form through the same `value_parser` validator that catches
    /// the explicit empty form.
    ///
    /// `require_equals = true` is load-bearing: without it,
    /// `aube add --allow-build esbuild some-pkg` would let clap
    /// silently swallow `esbuild` as the flag's value (since
    /// `num_args` allows 1 value) and leave the positional packages
    /// list empty. Forcing `=` syntax — `--allow-build=esbuild` —
    /// makes the boundary unambiguous and routes every bare-flag
    /// occurrence through `default_missing_value`.
    #[arg(
        long = "allow-build",
        value_name = "PKG",
        conflicts_with = "no_save",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = true,
        value_parser = parse_allow_build_value,
    )]
    pub allow_build: Vec<String>,
    /// Skip lifecycle scripts (no-op; aube already skips by default)
    #[arg(long)]
    pub ignore_scripts: bool,
    /// Install without persisting the dependency to `package.json`.
    ///
    /// Snapshots `package.json` and the lockfile, links the named
    /// packages into `node_modules`, and then restores both files —
    /// so the dependency is usable for the current process but the
    /// project's committed state is untouched.
    ///
    /// Handy for one-off experiments and for scripts that install a
    /// tool transiently. Mirrors `pnpm add --no-save`. Conflicts with
    /// `-g`/`--global`, which has to persist the install to its global
    /// manifest.
    #[arg(long, conflicts_with = "global")]
    pub no_save: bool,
    /// Inverse of `--save-workspace-protocol`.
    ///
    /// Forces the manifest specifier into a registry-style spec
    /// (`^<version>`) for this invocation, even when
    /// `linkWorkspacePackages` matched a local sibling. The install
    /// pipeline still prefers the local workspace copy at resolve
    /// time — this flag only controls what's written to
    /// `package.json`. Mirrors `pnpm add --no-save-workspace-protocol`.
    #[arg(long, overrides_with = "save_workspace_protocol")]
    pub no_save_workspace_protocol: bool,
    /// Save the new dependency into the workspace's default catalog.
    ///
    /// Writes `catalog:` into `package.json` and seeds/upserts the
    /// resolved range under `catalog:` in the workspace yaml. Mirrors
    /// `pnpm add --save-catalog`.
    ///
    /// Workspace and aliased specs (`workspace:*`, `npm:`, `jsr:`) are
    /// never catalogized — the manifest gets the original spec and
    /// the catalog yaml is left alone. If the package is already in
    /// the target catalog, the existing entry is preserved (never
    /// overwritten); the manifest then gets `catalog:` only when the
    /// existing entry is compatible with the user's range.
    ///
    /// Conflicts with `--no-save`: catalog mutations write to the
    /// workspace yaml, which the `--no-save` restore path doesn't
    /// snapshot — combining the two would silently leave an orphaned
    /// catalog entry behind.
    #[arg(long, conflicts_with_all = ["save_catalog_name", "no_save"])]
    pub save_catalog: bool,
    /// Save the new dependency into a *named* catalog.
    ///
    /// Writes the entry to `catalogs.<name>` in the workspace yaml and
    /// `catalog:<name>` into `package.json`. Same workspace/alias
    /// exclusions and `--no-save` conflict as `--save-catalog`. Mirrors
    /// `pnpm add --save-catalog-name=<name>`.
    #[arg(long, value_name = "NAME", conflicts_with = "no_save")]
    pub save_catalog_name: Option<String>,
    /// Add as a peer dependency (written to `peerDependencies` in
    /// package.json).
    ///
    /// By convention you usually pair this with `--save-dev` so the
    /// peer is also installed for local development; that's what pnpm
    /// does.
    #[arg(long, conflicts_with = "save_optional")]
    pub save_peer: bool,
    /// Force the manifest specifier into `workspace:` form for this
    /// invocation, overriding `saveWorkspaceProtocol` from the
    /// workspace yaml / `.npmrc` / env.
    ///
    /// Only meaningful when `linkWorkspacePackages` (or a workspace
    /// sibling already exists for the named package). With this flag
    /// the entry written to `package.json` is `workspace:^` (rolling)
    /// or `workspace:^<version>` (pinned), depending on the resolved
    /// `saveWorkspaceProtocol` value.
    #[arg(long, overrides_with = "no_save_workspace_protocol")]
    pub save_workspace_protocol: bool,
    /// Add the dependency to the workspace root's `package.json`.
    ///
    /// Applies regardless of the current working directory: walks up
    /// from cwd looking for `aube-workspace.yaml`, `pnpm-workspace.yaml`,
    /// or a `package.json` with a `workspaces` field and runs the add
    /// against that directory.
    #[arg(short = 'w', long, conflicts_with = "global")]
    pub workspace: bool,
    /// Allow `add` to run in a workspace root.
    ///
    /// By default aube refuses to add dependencies to the root
    /// `package.json` of a workspace (a directory containing
    /// `aube-workspace.yaml`, `pnpm-workspace.yaml`, or a `package.json`
    /// with a `workspaces` field) because deps added there end up
    /// shared by every package and usually reflect a mistake. Pass
    /// this flag to opt in. Mirrors `pnpm add -W`.
    #[arg(short = 'W', long)]
    pub ignore_workspace_root_check: bool,
    #[command(flatten)]
    pub lockfile: crate::cli_args::LockfileArgs,
    #[command(flatten)]
    pub network: crate::cli_args::NetworkArgs,
    #[command(flatten)]
    pub virtual_store: crate::cli_args::VirtualStoreArgs,
}

/// Parsed result of a package spec like "lodash@^4" or "my-alias@npm:real-pkg@^2".
#[cfg_attr(test, derive(Debug))]
struct ParsedPkgSpec {
    /// The name to use in package.json (alias if provided, otherwise the real name)
    alias: Option<String>,
    /// The real package name on the registry
    name: String,
    /// For `jsr:` specs, the JSR-style name (e.g. `@std/collections`).
    /// `name` has already been translated to the npm-compat form
    /// (`@jsr/std__collections`) so the registry fetch hits
    /// <https://npm.jsr.io>; we keep the original around so the
    /// manifest-write path can round-trip `jsr:…` back into
    /// `package.json`. `None` for non-jsr specs.
    jsr_name: Option<String>,
    /// The version range
    range: String,
    /// `true` when the user wrote an explicit `@<range>` (e.g. `lodash@latest`,
    /// `lodash@^4`). `false` when no version was given and the range was
    /// defaulted to `"latest"` by the parser. Used to decide whether the
    /// configured `tag` setting should override the range.
    has_explicit_range: bool,
    /// Original verbatim spec when the user typed a git URL form
    /// (`kevva/is-negative`, `github:user/repo`, `git+https://…`, …).
    /// `Some(_)` flags the spec for the non-registry git branch:
    /// skip the packument fetch, write the verbatim string into
    /// `package.json`, and let the resolver dispatch the git path.
    git_spec: Option<String>,
    /// Original verbatim spec when the user typed a `file:` / `link:`
    /// path form (`file:./pkg`, `link:../sibling`, `file:./bundle.tgz`).
    /// `Some(_)` flags the spec for the non-registry local branch:
    /// skip the packument fetch, write the verbatim string into
    /// `package.json`, and let the resolver dispatch the local path.
    local_spec: Option<String>,
    /// Set when `linkWorkspacePackages=true` matched a local sibling
    /// for this spec. The string is the sibling's `package.json#version`
    /// (or `"0.0.0"` when the sibling has no version). The packument
    /// fetch is skipped and the manifest-write path branches on
    /// `saveWorkspaceProtocol` to choose between rolling
    /// (`workspace:^`), pinned (`workspace:^<version>`), or
    /// registry-style (`^<version>`) — the resolver picks up the
    /// sibling either way because aube already prefers workspace
    /// matches on bare semver ranges.
    linked_workspace_version: Option<String>,
}

/// Parse a package spec into its components.
///
/// Supported forms:
/// - `lodash` → name=lodash, range=latest
/// - `lodash@^4` → name=lodash, range=^4
/// - `@scope/pkg@latest` → name=@scope/pkg, range=latest
/// - `npm:real-pkg@^4` → name=real-pkg, range=^4 (no alias)
/// - `my-alias@npm:real-pkg@^4` → alias=my-alias, name=real-pkg, range=^4
/// - `jsr:@std/collections@^1` → alias=@std/collections,
///   name=@jsr/std__collections, range=^1 (jsr translation)
/// - `my-alias@jsr:@std/collections@^1` → alias=my-alias,
///   name=@jsr/std__collections, range=^1
/// - `kevva/is-negative` → git: bare GitHub shorthand, name derived
///   from the repo segment of the clone URL
/// - `github:user/repo`, `git+https://host/u/r.git#tag` → git: any
///   form `aube_lockfile::parse_git_spec` recognizes
/// - `my-alias@kevva/is-negative` → git with alias: manifest key is
///   `my-alias`, spec written verbatim
/// - `file:./pkg`, `link:../sibling`, `file:./bundle.tgz` → local:
///   manifest key derived from the path basename (alias overrides)
/// - `my-alias@file:./pkg`, `my-alias@link:../sibling` → local with
///   alias: manifest key is `my-alias`, spec written verbatim
fn parse_pkg_spec(spec: &str) -> miette::Result<ParsedPkgSpec> {
    // Git specs route through their own branch — packument fetch is
    // skipped and the verbatim spec is written to `package.json`.
    // Try the full string first so explicit URL forms shadow the
    // alias check below; then peel a leading `alias@` and re-test.
    if aube_lockfile::parse_git_spec(spec).is_some() {
        return parse_git_pkg_spec(spec, None);
    }
    if let Some((alias, rest)) = split_git_alias(spec)
        && aube_lockfile::parse_git_spec(rest).is_some()
    {
        return parse_git_pkg_spec(rest, Some(alias.to_string()));
    }
    // Scoped alias form `@scope/alias@<git-or-local-spec>` — pnpm
    // supports this for both git and local specs. Routed before the
    // jsr/npm/scoped-name branches below so a scoped name with a
    // git/local tail isn't misclassified as a registry fetch.
    if let Some((alias, rest)) = split_scoped_alias(spec) {
        if aube_lockfile::parse_git_spec(rest).is_some() {
            return parse_git_pkg_spec(rest, Some(alias.to_string()));
        }
        if is_local_path_spec(rest) {
            return parse_local_pkg_spec(rest, Some(alias.to_string()));
        }
    }
    // Local path specs use the same skip-packument routing.
    if is_local_path_spec(spec) {
        return parse_local_pkg_spec(spec, None);
    }
    if let Some((alias, rest)) = split_local_alias(spec)
        && is_local_path_spec(rest)
    {
        return parse_local_pkg_spec(rest, Some(alias.to_string()));
    }

    // Handle full alias form: alias@jsr:@scope/name[@range]
    if let Some(jsr_idx) = spec.find("@jsr:") {
        let before = &spec[..jsr_idx];
        let after_jsr = &spec[jsr_idx + 5..]; // after "jsr:"
        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };
        return parse_jsr_name_range(after_jsr, alias);
    }
    // Handle bare jsr: prefix: jsr:@scope/name[@range]
    if let Some(rest) = spec.strip_prefix("jsr:") {
        return parse_jsr_name_range(rest, None);
    }
    // Handle full alias form: alias@npm:real-pkg@range
    if let Some(npm_idx) = spec.find("@npm:") {
        // Everything before @npm: could be empty (bare npm:pkg@range) or an alias name
        let before = &spec[..npm_idx];
        let after_npm = &spec[npm_idx + 5..]; // after "npm:"

        let alias = if before.is_empty() {
            None
        } else {
            Some(before.to_string())
        };

        // after_npm is "real-pkg@range" or "@scope/pkg@range" or just "real-pkg"
        return Ok(parse_name_range(after_npm, alias));
    }

    // Handle bare npm: prefix: npm:pkg@range
    if let Some(rest) = spec.strip_prefix("npm:") {
        return Ok(parse_name_range(rest, None));
    }

    // Normal spec: name[@range]
    Ok(parse_name_range(spec, None))
}

/// Split `alias@<rest>` for the git-spec alias form. Returns
/// `Some((alias, rest))` when the input has a non-empty alias that
/// looks like a plain npm name. Scoped npm names (`@scope/pkg`)
/// start with `@` and never aliased a git spec in any package
/// manager. A `:` in the alias would mean we caught a protocol
/// prefix (`jsr:`, `npm:`, `github:`, `git+…`) — those cases are
/// handled by their own branches in `parse_pkg_spec` and must not
/// be reinterpreted as a git alias.
fn split_git_alias(spec: &str) -> Option<(&str, &str)> {
    split_protocol_alias(spec)
}

/// `true` when `spec` is a local-path form: an explicit `file:` /
/// `link:` prefix, or a bare path the user typed at the shell
/// (`./foo`, `/abs/foo`, `~/foo`, `C:/foo`). Mirrors pnpm's
/// `parseBareSpecifier` so `aube add /path/to/lib` no longer falls
/// through to the registry path and 405s. Any `file:` URL form that
/// `parse_git_spec` recognizes (e.g. `file:///host/repo.git`) is
/// treated as a git spec instead — same precedence the resolver's
/// `is_non_registry_specifier` uses.
fn is_local_path_spec(spec: &str) -> bool {
    if spec.starts_with("link:") {
        return true;
    }
    if spec.starts_with("file:") {
        // `file:` git URLs (`file:///host/repo.git`) belong on the git
        // branch, not here. The bare-path branch below has no such
        // collision because git URL forms always use a protocol.
        return aube_lockfile::parse_git_spec(spec).is_none();
    }
    looks_like_path(spec)
}

/// `true` when `s` looks like a path the user typed at the shell:
/// absolute, relative, home-relative, or a Windows drive-letter form.
/// Deliberately narrower than pnpm's `includes(path.sep)` rule so
/// scoped registry names like `@babel/core` don't get mistaken for a
/// directory path. The drive-letter branch requires a `/` or `\` after
/// the colon so a single-character alias like `a:1.0.0` isn't
/// reclassified.
fn looks_like_path(s: &str) -> bool {
    if s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with("~/")
        || s.starts_with("~\\")
        || s.starts_with('\\')
        || s.starts_with(".\\")
        || s.starts_with("..\\")
    {
        return true;
    }
    let bytes = s.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn is_tarball_suffix(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.ends_with(".tgz") || lower.ends_with(".tar.gz") || lower.ends_with(".tar")
}

/// Expand a leading `~/` (or `~\`) to the user's home directory.
/// Returns the input unchanged when there's no tilde. Used at parse
/// time so the verbatim spec written to the manifest is something the
/// resolver (which has no tilde-expansion of its own) can actually
/// open. Errors when `$HOME` is unavailable rather than letting the
/// literal `~` leak into a `cwd`-joined path the resolver can't make
/// sense of.
fn expand_tilde(s: &str) -> miette::Result<String> {
    let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) else {
        return Ok(s.to_string());
    };
    let home = aube_util::env::home_dir().ok_or_else(|| {
        miette!(
            "cannot expand `~/` in `{s}` — $HOME is not set; \
             pass an absolute path or set $HOME"
        )
    })?;
    Ok(home.join(rest).to_string_lossy().into_owned())
}

/// Normalize a bare local-path spec into its `file:` / `link:` form
/// (pnpm parity: directories default to `link:`, tarballs to `file:`).
/// Returns the input unchanged when it already carries an explicit
/// protocol prefix.
fn prefix_bare_local_path(spec: &str) -> miette::Result<String> {
    if spec.starts_with("file:") || spec.starts_with("link:") {
        return Ok(spec.to_string());
    }
    let expanded = expand_tilde(spec)?;
    let prefix = if is_tarball_suffix(&expanded) {
        "file:"
    } else {
        "link:"
    };
    Ok(format!("{prefix}{expanded}"))
}

/// Split `alias@<rest>` for the local-spec alias form. Mirrors
/// `split_git_alias` — same shape, different caller intent.
fn split_local_alias(spec: &str) -> Option<(&str, &str)> {
    split_protocol_alias(spec)
}

/// Shared helper for `split_git_alias` and `split_local_alias`. The
/// alias-form rules are identical for both: peel a leading `name@`
/// where `name` is a plain (non-scoped, non-protocol) npm-style id.
/// Scoped aliases (`@scope/alias@<spec>`) are caught by
/// [`split_scoped_alias`] one branch up; the leading `@` is rejected
/// here via the `at == 0` guard.
fn split_protocol_alias(spec: &str) -> Option<(&str, &str)> {
    let at = spec.find('@')?;
    if at == 0 {
        return None;
    }
    let alias = &spec[..at];
    if alias.contains(':') {
        return None;
    }
    Some((alias, &spec[at + 1..]))
}

/// Split `@scope/alias@<rest>` so scoped names can serve as the
/// manifest key for git/local specs. Returns `Some((alias, rest))`
/// when the input looks like a scoped npm name followed by `@<spec>`.
/// Mirrors pnpm's behavior: `@my-scope/alias@file:./pkg` writes the
/// manifest entry under `@my-scope/alias`.
fn split_scoped_alias(spec: &str) -> Option<(&str, &str)> {
    if !spec.starts_with('@') {
        return None;
    }
    let slash = spec.find('/')?;
    let after_slash = &spec[slash + 1..];
    let at_in_after = after_slash.find('@')?;
    let alias_end = slash + 1 + at_in_after;
    if alias_end == 0 {
        return None;
    }
    Some((&spec[..alias_end], &spec[alias_end + 1..]))
}

/// Build a `ParsedPkgSpec` for a git-form spec. The manifest key is
/// the user-supplied alias when given, otherwise the repo segment of
/// the clone URL (e.g. `kevva/is-negative` → `is-negative`). The
/// `range` field carries the verbatim spec so `git_spec` and `range`
/// agree, and the install pipeline's lockfile reader sees the same
/// string the user typed.
fn parse_git_pkg_spec(verbatim: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let (clone_url, _committish, _subpath) = aube_lockfile::parse_git_spec(verbatim)
        .ok_or_else(|| miette!("expected git spec, got `{verbatim}`"))?;
    // Only derive a name from the URL when the user didn't supply an
    // alias — a trailing-slash or otherwise pathless URL would
    // otherwise hard-fail even though the alias makes the derivation
    // unnecessary.
    let name = match &alias {
        Some(a) => a.clone(),
        None => repo_name_from_clone_url(&clone_url).ok_or_else(|| {
            miette!(
                "could not derive a package name from git URL `{clone_url}`; \
                 pass an alias (e.g. `my-name@{verbatim}`)"
            )
        })?,
    };
    Ok(ParsedPkgSpec {
        alias,
        name,
        jsr_name: None,
        range: verbatim.to_string(),
        has_explicit_range: true,
        git_spec: Some(verbatim.to_string()),
        local_spec: None,
        linked_workspace_version: None,
    })
}

/// Build a `ParsedPkgSpec` for a local-path spec. The manifest key is
/// the user-supplied alias when given, otherwise the basename of the
/// path (e.g. `file:./packages/foo` → `foo`). The `range` field
/// carries the verbatim spec so `local_spec` and `range` agree, and
/// the install pipeline's lockfile reader sees the same string the
/// user typed.
///
/// Bare paths (`./foo`, `/abs/foo`, `~/foo`) are normalized into their
/// `file:` / `link:` form before being stored — pnpm parity for
/// `aube add /path/to/lib`. `~/` is expanded eagerly because the
/// resolver has no tilde handling.
fn parse_local_pkg_spec(input: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let verbatim = prefix_bare_local_path(input)?;
    let path = verbatim
        .strip_prefix("file:")
        .or_else(|| verbatim.strip_prefix("link:"))
        .ok_or_else(|| miette!("expected file:/link: spec, got `{verbatim}`"))?;
    // Only derive a name from the path when the user didn't supply an
    // alias — a bare `file:` (empty path) would otherwise hard-fail
    // even though the alias makes the derivation unnecessary.
    let name = match &alias {
        Some(a) => a.clone(),
        None => basename_from_local_path(path).ok_or_else(|| {
            miette!(
                "could not derive a package name from local spec `{verbatim}`; \
                 pass an alias (e.g. `my-name@{verbatim}`)"
            )
        })?,
    };
    Ok(ParsedPkgSpec {
        alias,
        name,
        jsr_name: None,
        range: verbatim.clone(),
        has_explicit_range: true,
        git_spec: None,
        local_spec: Some(verbatim),
        linked_workspace_version: None,
    })
}

/// Pull the repo segment out of a git clone URL, stripping a trailing
/// `.git`. Used as the manifest key when the user didn't supply an
/// alias. `None` when the URL has no path segment to slice (which
/// shouldn't happen for `parse_git_spec` outputs but the caller guards
/// against the edge case anyway).
fn repo_name_from_clone_url(url: &str) -> Option<String> {
    let body = url.split_once('?').map(|(b, _)| b).unwrap_or(url);
    let body = body.split_once('#').map(|(b, _)| b).unwrap_or(body);
    let last = body.rsplit('/').next()?;
    let stripped = last.strip_suffix(".git").unwrap_or(last);
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_string())
}

/// Derive the manifest key from the path portion of a `file:` /
/// `link:` spec. Strips a trailing `.tgz` / `.tar.gz` so a tarball
/// like `file:./bundle.tgz` lands as `"bundle"` in the manifest.
/// Returns `None` for empty / pathless inputs. Splits on both `/` and
/// `\` so a Windows path like `c:\projects\lib` resolves to `"lib"`,
/// not the whole string.
fn basename_from_local_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let last = trimmed.rsplit(['/', '\\']).next()?;
    // `.tar.gz` checked before `.tgz` / `.tar` so a doubly-suffixed
    // name strips both compression and archive in one pass.
    let stripped = last
        .strip_suffix(".tar.gz")
        .or_else(|| last.strip_suffix(".tgz"))
        .or_else(|| last.strip_suffix(".tar"))
        .unwrap_or(last);
    if stripped.is_empty() || stripped == "." || stripped == ".." {
        return None;
    }
    Some(stripped.to_string())
}

fn parse_name_range(s: &str, alias: Option<String>) -> ParsedPkgSpec {
    // Handle scoped packages: @scope/name@range
    if s.starts_with('@') {
        if let Some(slash_idx) = s.find('/') {
            let after_slash = &s[slash_idx + 1..];
            if let Some(at_idx) = after_slash.find('@') {
                return ParsedPkgSpec {
                    alias,
                    name: s[..slash_idx + 1 + at_idx].to_string(),
                    jsr_name: None,
                    range: after_slash[at_idx + 1..].to_string(),
                    has_explicit_range: true,
                    git_spec: None,
                    local_spec: None,
                    linked_workspace_version: None,
                };
            }
        }
        return ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        };
    }

    // Unscoped: name@range
    if let Some(at_idx) = s.find('@') {
        ParsedPkgSpec {
            alias,
            name: s[..at_idx].to_string(),
            jsr_name: None,
            range: s[at_idx + 1..].to_string(),
            has_explicit_range: true,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        }
    } else {
        ParsedPkgSpec {
            alias,
            name: s.to_string(),
            jsr_name: None,
            range: "latest".to_string(),
            has_explicit_range: false,
            git_spec: None,
            local_spec: None,
            linked_workspace_version: None,
        }
    }
}

/// Parse the `@scope/name[@range]` tail of a `jsr:` spec and translate
/// the JSR-style scoped name into the npm-compat form served at
/// <https://npm.jsr.io>. JSR packages always use scoped names — we
/// reject anything that doesn't start with `@scope/` so the user gets a
/// real error instead of a `latest` lookup against a garbled package
/// name.
///
/// If `alias` is `None`, we default the manifest key to the JSR name
/// itself so `aube add jsr:@std/collections` lands as
/// `"@std/collections": "jsr:…"` — matching pnpm's behavior.
fn parse_jsr_name_range(s: &str, alias: Option<String>) -> miette::Result<ParsedPkgSpec> {
    let inner = parse_name_range(s, None);
    let jsr_name = inner.name.clone();
    let npm_name = aube_registry::jsr::jsr_to_npm_name(&jsr_name).ok_or_else(|| {
        miette!(
            "invalid jsr: spec — expected `jsr:@scope/name[@range]`, got `jsr:{s}` \
             (JSR packages must be scoped, e.g. `jsr:@std/collections`)"
        )
    })?;
    let final_alias = alias.or_else(|| Some(jsr_name.clone()));
    Ok(ParsedPkgSpec {
        alias: final_alias,
        name: npm_name,
        jsr_name: Some(jsr_name),
        range: inner.range,
        has_explicit_range: inner.has_explicit_range,
        git_spec: None,
        local_spec: None,
        linked_workspace_version: None,
    })
}

pub async fn run(
    args: AddArgs,
    filter: aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    args.network.install_overrides();
    args.lockfile.install_overrides();
    args.virtual_store.install_overrides();
    if !filter.is_empty() && !args.global && !args.workspace {
        return run_filtered(args, &filter).await;
    }

    let AddArgs {
        packages,
        global,
        save_dev,
        save_optional,
        save_exact,
        save_peer,
        save_workspace_protocol,
        no_save_workspace_protocol,
        workspace,
        ignore_scripts: _,
        no_save,
        ignore_workspace_root_check,
        save_catalog,
        save_catalog_name,
        allow_build,
        lockfile,
        network,
        virtual_store,
    } = args;
    let save_catalog_target = save_catalog_name.or_else(|| {
        if save_catalog {
            Some("default".to_string())
        } else {
            None
        }
    });
    let packages = &packages[..];
    if packages.is_empty() {
        return Err(miette!("no packages specified"));
    }

    if global {
        return run_global(packages, allow_build, lockfile, network, virtual_store).await;
    }

    // `--workspace` / `-w`: redirect the add at the workspace root
    // (directory containing `aube-workspace.yaml` / `pnpm-workspace.yaml`)
    // before anything reads `dirs::cwd()`. We chdir into it so the
    // downstream install pipeline treats the root as the project.
    if workspace {
        let start = std::env::current_dir()
            .into_diagnostic()
            .wrap_err("failed to read current dir")?;
        let root = super::find_workspace_root(&start).wrap_err("--workspace")?;
        if root != start {
            std::env::set_current_dir(&root)
                .into_diagnostic()
                .wrap_err_with(|| format!("failed to chdir into {}", root.display()))?;
        }
        crate::dirs::set_cwd(&root)?;
    }

    // pnpm `install <pkg>` (= aube `add <pkg>`) creates an empty
    // package.json when run in a directory with no manifest, so users
    // can bootstrap a project with a single command. Match that: if no
    // ancestor has a package.json (within the home boundary), write a
    // minimal `{}` in cwd before resolving the project root. The
    // `--global`/`-g` path returned earlier; `--workspace` already
    // redirected to a known root above.
    let initial_cwd = crate::dirs::cwd()?;
    if crate::dirs::find_project_root(&initial_cwd).is_none() {
        std::fs::write(initial_cwd.join("package.json"), "{}\n")
            .into_diagnostic()
            .wrap_err("failed to create package.json")?;
    }
    let cwd = crate::dirs::project_root()?;

    // Refuse to add into a workspace root unless the caller opts out.
    // Matches pnpm: deps added here are shared by every workspace
    // package and usually reflect a mistake. `-W` /
    // `--ignore-workspace-root-check` bypasses the check, and `-w` /
    // `--workspace` implies the bypass since the user explicitly
    // targeted the root. We trip on a *declared* package-pattern list,
    // not on the materialized glob — an empty `packages/*` directory
    // is still a workspace root the user should opt into. Bare
    // catalog-only yaml is not a workspace root, and a `package.json`
    // without a `workspaces` field isn't either.
    if !ignore_workspace_root_check && !workspace {
        // `WorkspaceConfig::load` already returns an empty `packages`
        // list when no yaml exists, so propagating errors here only
        // surfaces genuine yaml problems (permission denied, malformed
        // YAML) instead of silently letting `add` proceed against what
        // might actually be a workspace root.
        let ws = aube_manifest::WorkspaceConfig::load(&cwd)
            .into_diagnostic()
            .wrap_err("failed to read workspace config")?;
        let yaml_has_packages = !ws.packages.is_empty();
        // `package.json` read errors fall through intentionally: the
        // install pipeline below re-reads and parses the same file and
        // surfaces a richer miette diagnostic pointing at the offending
        // byte. Duplicating that error here would double-report.
        let pkg_json_has_workspaces =
            aube_manifest::PackageJson::from_path(&cwd.join("package.json"))
                .ok()
                .and_then(|m| m.workspaces)
                .is_some_and(|w| !w.patterns().is_empty());
        if yaml_has_packages || pkg_json_has_workspaces {
            return Err(miette!(
                "refusing to add dependencies to the workspace root. \
                 If this is intentional, pass --ignore-workspace-root-check (-W)."
            ));
        }
    }

    let _lock = super::take_project_lock(&cwd)?;
    let manifest_path = cwd.join("package.json");

    // 1. Read existing package.json. Snapshot the raw bytes when
    // `--no-save` is in effect so we can restore both the manifest
    // *and* the lockfile after the resolver/install pipeline (both
    // re-read from disk) has done its work — the user gets the new
    // package linked into `node_modules` while their committed
    // project state stays exactly as they wrote it.
    //
    // The lockfile path matches whatever
    // `write_lockfile_preserving_existing` will write to: detect the
    // existing lockfile kind on disk (pnpm, npm, yarn, bun, …) so a
    // project using `pnpm-lock.yaml` doesn't end up with both a
    // restored aube-lock.yaml *and* a leftover modified pnpm-lock.yaml.
    // When no lockfile exists yet the resolver falls back to aube's
    // own format, so we target that path and the restore step deletes
    // it (since `lockfile_bytes` is `None`).
    let lockfile_path = lockfile_path_for_project(&cwd);
    let no_save_snapshot = if no_save {
        let manifest_bytes = std::fs::read(&manifest_path)
            .into_diagnostic()
            .wrap_err("failed to snapshot package.json for --no-save")?;
        let lockfile_bytes = match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        };
        Some(NoSaveSnapshot {
            manifest_bytes,
            lockfile_bytes,
        })
    } else {
        None
    };
    // `--allow-build=<pkg>` pre-approves dep lifecycle scripts as part
    // of the add. The conflict check (refuses to flip a pre-existing
    // `false` to `true`) runs BEFORE `update_manifest_for_add` so a
    // failure can't leave the manifest with new deps written but no
    // matching install. Approval bytes themselves are also written here
    // — the order doesn't matter for the install pipeline (it re-reads
    // both files from disk) but keeps the failure-mode reasoning local.
    if !allow_build.is_empty() {
        apply_allow_build_flags(&cwd, &allow_build)?;
    }

    // Bun-style pluggable security scanner. Runs before manifest
    // mutation so a `fatal` advisory refuses the add and leaves
    // `package.json` untouched. Skipped on git/local/workspace/jsr
    // specs — same registry-bound filter as the OSV gate.
    let scanner = super::with_settings_ctx(&cwd, aube_settings::resolved::security_scanner);
    if !scanner.is_empty() {
        let scanner_packages = scanner_packages_for(packages);
        super::security_scanner::run_scanner(&scanner, &cwd, &scanner_packages).await?;
    }

    update_manifest_for_add(
        &cwd,
        packages,
        AddManifestOptions {
            save_dev,
            save_exact,
            save_optional,
            save_peer,
            save_catalog: save_catalog_target,
            workspace_protocol_override: workspace_protocol_override_from_flags(
                save_workspace_protocol,
                no_save_workspace_protocol,
            ),
        },
        !no_save,
    )
    .await?;

    // 4. Run install. It re-reads the mutated package.json, runs the
    // resolver (reusing locked entries for unchanged specs), writes the
    // lockfile, and links node_modules in one pipeline. `Fix` mode is
    // the right semantic here: package.json just gained a new spec,
    // so the lockfile is by definition stale on that one entry — Prefer
    // would risk taking the from-lockfile fast path and missing the
    // new dep. Wrapping in a `Result` so the restore step below runs
    // even on failure — a network error mid-resolve would otherwise
    // leave the mutated `package.json` on disk, breaking `--no-save`.
    // `with_mode()` already skips root lifecycle hooks (chained-call
    // contract) so `aube add` doesn't re-run the root postinstall /
    // prepare on every invocation.
    let pipeline_result: miette::Result<()> = install::run(install::InstallOptions::with_mode(
        super::chained_frozen_mode(install::FrozenMode::Fix),
    ))
    .await;

    // 5. Under `--no-save`, restore the snapshotted `package.json` and
    // lockfile so neither shows up in `git status`. The user's
    // `node_modules` keeps the freshly linked package — matching
    // pnpm's `--no-save` semantics. We do this regardless of whether
    // the install succeeded so failures still leave the project
    // pristine. If the lockfile didn't exist before, delete the one
    // we just wrote.
    //
    // Both restores are attempted independently — if the manifest
    // write fails, we still try the lockfile restore so the project
    // doesn't get stuck in a half-mutated state. Any errors from this
    // step (and the captured `pipeline_result`) are folded together
    // before returning, so the caller sees the *first* relevant
    // failure rather than silently dropping later ones.
    let restore_errors = if let Some(snapshot) = no_save_snapshot {
        let mut errors: Vec<miette::Report> = Vec::new();
        if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &snapshot.manifest_bytes)
        {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original package.json after --no-save")
                    .unwrap_err(),
            );
        }
        let lockfile_restore = match &snapshot.lockfile_bytes {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!("Restored package.json and lockfile (--no-save)");
        }
        errors
    } else {
        Vec::new()
    };

    // Order matters: surface the pipeline error first when present —
    // it's the root cause and the restore errors are downstream
    // fallout. With no pipeline error, surface the first restore
    // failure (subsequent ones are usually variants of the same
    // filesystem problem).
    pipeline_result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

/// Bytes captured from disk before `aube add --no-save` mutated the
/// manifest and lockfile, used to put both back exactly as the user had
/// them once the install pipeline (which insists on reading from disk)
/// has finished linking `node_modules`.
struct NoSaveSnapshot {
    manifest_bytes: Vec<u8>,
    /// `None` means the lockfile didn't exist before the add — in that
    /// case the restore step deletes whatever the resolver wrote.
    lockfile_bytes: Option<Vec<u8>>,
}

#[derive(Clone)]
struct AddManifestOptions {
    save_dev: bool,
    save_exact: bool,
    save_optional: bool,
    save_peer: bool,
    /// Target catalog for `--save-catalog` / `--save-catalog-name`.
    /// `None` means neither flag was passed and the catalog yaml is
    /// left untouched. `Some("default")` is `--save-catalog`;
    /// `Some(other)` is `--save-catalog-name=<other>`.
    save_catalog: Option<String>,
    /// `--save-workspace-protocol` / `--no-save-workspace-protocol`
    /// per-invocation override. `None` defers to the resolved
    /// `saveWorkspaceProtocol` setting; `Some(true)` forces the
    /// `workspace:` form regardless of the setting; `Some(false)`
    /// forces a registry-style spec even when `linkWorkspacePackages`
    /// matched a sibling.
    workspace_protocol_override: Option<bool>,
}

impl AddManifestOptions {
    fn from_args(args: &AddArgs) -> Self {
        Self {
            save_dev: args.save_dev,
            save_exact: args.save_exact,
            save_optional: args.save_optional,
            save_peer: args.save_peer,
            save_catalog: args.save_catalog_name.clone().or_else(|| {
                if args.save_catalog {
                    Some("default".to_string())
                } else {
                    None
                }
            }),
            workspace_protocol_override: workspace_protocol_override_from_flags(
                args.save_workspace_protocol,
                args.no_save_workspace_protocol,
            ),
        }
    }
}

/// Map the paired `--save-workspace-protocol` / `--no-save-workspace-protocol`
/// flags to a tri-state. `clap`'s `overrides_with` ensures only the
/// last-typed flag survives, so at most one of the two is `true` at a
/// time and we don't need to disambiguate. Takes the two flag bools
/// directly so the call site in `run()` (which destructures `AddArgs`
/// into locals) and `from_args` can share the logic without going
/// back through the struct.
fn workspace_protocol_override_from_flags(save: bool, no_save: bool) -> Option<bool> {
    if save {
        Some(true)
    } else if no_save {
        Some(false)
    } else {
        None
    }
}

async fn update_manifest_for_add(
    cwd: &Path,
    packages: &[String],
    opts: AddManifestOptions,
    print_updated: bool,
) -> miette::Result<()> {
    // Resolve settings (savePrefix, tag, catalogMode, link/save
    // workspace protocol) from .npmrc / workspace yaml. `catalog_mode`
    // decides whether a newly-added dep that already lives in the
    // default workspace catalog gets rewritten to `catalog:` (see
    // `commands::catalogs::decide_add_rewrite`).
    //
    // The two settings the workspace yaml owns end-to-end
    // (`linkWorkspacePackages`, `saveWorkspaceProtocol`) read from
    // the workspace yaml root so a sub-project's `aube add` honors
    // the workspace-wide policy declared in
    // `pnpm-workspace.yaml`/`aube-workspace.yaml`. Everything else
    // (`tag`, `savePrefix`, `catalogMode`) reads from the project's
    // own dir so a sub-project's `.npmrc` still wins — switching the
    // entire context to the workspace root would silently drop those
    // overrides, since `load_npmrc_entries` doesn't walk up.
    let (default_tag, default_prefix, catalog_mode) = super::with_settings_ctx(cwd, |ctx| {
        let tag = aube_settings::resolved::tag(ctx);
        let prefix = if opts.save_exact {
            String::new()
        } else {
            let raw = aube_settings::resolved::save_prefix(ctx);
            // Validate: only ^, ~, or empty are valid prefixes.
            match raw.as_str() {
                "^" | "~" | "" => raw,
                _ => {
                    tracing::warn!(
                        code = aube_codes::warnings::WARN_AUBE_INVALID_SAVE_PREFIX,
                        "ignoring invalid save-prefix={raw:?}, falling back to ^"
                    );
                    "^".to_string()
                }
            }
        };
        let catalog_mode = aube_settings::resolved::catalog_mode(ctx);
        (tag, prefix, catalog_mode)
    });
    let workspace_settings_cwd = crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let (link_workspace_packages, save_workspace_protocol_setting) =
        super::with_settings_ctx(&workspace_settings_cwd, |ctx| {
            (
                aube_settings::resolved::link_workspace_packages(ctx),
                aube_settings::resolved::save_workspace_protocol(ctx),
            )
        });
    // Load the workspace catalog map up front — the resolver needs it
    // later, but `catalogMode` consults the default catalog while we
    // build the specifier below. Pass the same map to the resolver to
    // avoid re-reading the workspace file.
    let workspace_catalogs = super::load_workspace_catalogs(cwd)?;
    let default_catalog = workspace_catalogs.get("default");
    let manifest_path = cwd.join("package.json");
    let mut manifest = super::load_manifest(&manifest_path)?;

    // `--save-catalog` / `--save-catalog-name` queue: each newly-added
    // package that should land in a workspace catalog records its
    // (catalog, package, range) here. Applied to the workspace yaml in
    // a single pass after the manifest loop so the file is rewritten
    // at most once per `aube add` invocation.
    let mut catalog_upserts: Vec<CatalogUpsert> = Vec::new();

    // Parse all specs and fetch packuments concurrently.
    let client = std::sync::Arc::new(super::make_client(cwd));
    let mut parsed: Vec<_> = packages
        .iter()
        .map(|s| {
            let mut spec = parse_pkg_spec(s)?;
            // Replace the implicit default tag with the configured one
            // so that `aube add lodash` respects `tag=next` in .npmrc.
            // Only applies when the user didn't write an explicit version
            // or tag — `aube add lodash@latest` always means `latest`.
            if !spec.has_explicit_range && default_tag != "latest" {
                spec.range = default_tag.clone();
            }
            Ok::<_, miette::Report>(spec)
        })
        .collect::<miette::Result<Vec<_>>>()?;

    // `linkWorkspacePackages=true` (or the `--save-workspace-protocol`
    // flag) makes `aube add <name>` look the package up in the local
    // workspace before falling back to the registry. Build the
    // (name → version) map once for this invocation and tag any
    // matching specs so the packument-fetch loop skips them and the
    // manifest-write path branches into the workspace formatter.
    if link_workspace_packages || matches!(opts.workspace_protocol_override, Some(true)) {
        let workspace_versions = collect_workspace_versions(cwd);
        for spec in &mut parsed {
            if spec.linked_workspace_version.is_some() {
                continue;
            }
            // Only registry-shaped, non-aliased specs are eligible:
            // workspace/git/local/jsr/npm-aliased specs already have
            // their own routing and the user typed them on purpose.
            // Aliased specs (`my-alias@project-2`) need to skip the
            // workspace path too — `workspace:` resolves by manifest
            // key, so writing `"my-alias": "workspace:^"` would point
            // the resolver at a sibling named `my-alias` (which
            // doesn't exist) and 404 on the registry fallback.
            if aube_util::pkg::is_workspace_spec(&spec.range)
                || aube_util::pkg::is_catalog_spec(&spec.range)
                || aube_util::pkg::is_npm_spec(&spec.range)
                || aube_util::pkg::is_jsr_spec(&spec.range)
                || spec.git_spec.is_some()
                || spec.local_spec.is_some()
                || spec.jsr_name.is_some()
                || spec.alias.is_some()
            {
                continue;
            }
            let Some(version) = workspace_versions.get(&spec.name) else {
                continue;
            };
            // When the user typed an explicit range
            // (`aube add pkg@^1.2.0`), the sibling's version must
            // satisfy it — otherwise we'd silently link an
            // incompatible local copy. Fall through to the registry
            // path on a mismatch (and on unparseable ranges, where
            // the registry path's error message is more useful than
            // a workspace mismatch). Bare adds (no `@<range>`) carry
            // `range = "latest"` from the parser; the implicit
            // dist-tag never blocks a workspace match.
            if spec.has_explicit_range
                && let (Ok(parsed_version), Ok(parsed_range)) = (
                    node_semver::Version::parse(version),
                    node_semver::Range::parse(&spec.range),
                )
                && !parsed_version.satisfies(&parsed_range)
            {
                continue;
            }
            spec.linked_workspace_version = Some(version.clone());
        }
    }

    // Skip packument fetches for `workspace:*` / `workspace:^` /
    // `workspace:<range>` specs — they resolve against the local
    // workspace, not the registry. Same skip applies to git specs
    // (`kevva/is-negative`, `github:user/repo`, …) and `file:` /
    // `link:` local-path specs which the resolver dispatches via the
    // git or local branch respectively. Specs that the
    // `linkWorkspacePackages` pass tagged with a sibling version
    // also bypass the registry — the workspace is the source of
    // truth for those names. Without these guards the parallel
    // fetch below would 404 on the non-registry name.
    let mut handles = Vec::new();
    for spec in &parsed {
        if aube_util::pkg::is_workspace_spec(&spec.range)
            || spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.linked_workspace_version.is_some()
        {
            continue;
        }
        let client = client.clone();
        let name = spec.name.clone();
        let handle = tokio::spawn(async move {
            let packument = client
                .fetch_packument(&name)
                .await
                .map_err(|e| miette!("failed to fetch {name}: {e}"))?;
            Ok::<_, miette::Report>((name, packument))
        });
        handles.push(handle);
    }

    let mut packuments = BTreeMap::new();
    for handle in handles {
        let (name, packument) = handle.await.into_diagnostic()??;
        packuments.insert(name, packument);
    }

    // Resolve each package and update manifest.
    for (spec, orig) in parsed.iter().zip(packages.iter()) {
        let pkg_name_for_manifest = spec.alias.as_deref().unwrap_or(&spec.name);

        // Workspace-protocol specs (`pkg@workspace:*`, `pkg@workspace:^`,
        // `pkg@workspace:1.2.0`) bypass the registry path entirely:
        // resolve against the local workspace, write the user's spec
        // verbatim to the manifest, and skip catalog logic (workspace
        // deps are never catalogized).
        if aube_util::pkg::is_workspace_spec(&spec.range) {
            apply_workspace_spec_to_manifest(
                cwd,
                &mut manifest,
                spec,
                pkg_name_for_manifest,
                &opts,
            )?;
            continue;
        }

        // Git specs (`kevva/is-negative`, `github:user/repo`,
        // `git+https://…#tag`) bypass the registry path: write the
        // user's verbatim spec into the manifest and let the resolver
        // dispatch the git branch on the next install. Catalog logic
        // is skipped (catalogs are for registry deps).
        if let Some(verbatim) = spec.git_spec.as_deref() {
            apply_git_spec_to_manifest(&mut manifest, pkg_name_for_manifest, verbatim, &opts);
            continue;
        }

        // `file:` / `link:` local-path specs are handled the same way
        // as git: skip the registry, write the verbatim spec, let the
        // resolver dispatch the local branch on next install.
        if let Some(verbatim) = spec.local_spec.as_deref() {
            apply_local_spec_to_manifest(&mut manifest, pkg_name_for_manifest, verbatim, &opts);
            continue;
        }

        // `linkWorkspacePackages=true` matched a sibling for this
        // spec. Write either a workspace-form specifier (rolling /
        // pinned) or a registry-form specifier per the resolved
        // `saveWorkspaceProtocol` setting and the per-invocation
        // override; the resolver picks the local copy regardless of
        // the form because it already prefers workspace siblings on
        // bare semver ranges.
        if let Some(version) = spec.linked_workspace_version.as_deref() {
            apply_linked_workspace_to_manifest(
                &mut manifest,
                pkg_name_for_manifest,
                version,
                save_workspace_protocol_setting,
                opts.workspace_protocol_override,
                &default_prefix,
                &opts,
            );
            continue;
        }

        let packument = packuments.get(&spec.name).unwrap();

        eprintln!("Resolving {}@{}...", spec.name, spec.range);

        // Resolve "latest" and other dist-tags to a version range.
        let effective_range = if let Some(tagged_version) = packument.dist_tags.get(&spec.range) {
            tagged_version.clone()
        } else {
            spec.range.clone()
        };

        // Find highest matching version. Reused below when a
        // `catalogMode` rewrite redirects resolution to the catalog's
        // range — the display version should match what will actually
        // get installed, not what the user's original range resolved
        // to, so we call this twice when the rewrite fires.
        //
        // Parse every candidate version once (skipping invalid ones
        // entirely) and sort the parsed pairs. Comparator-only parsing
        // burned ~2N parses per add; pre-parse turns it into N + log N
        // and lets the satisfies-scan reuse the parsed `Version`.
        let mut parsed_versions: Vec<(&String, node_semver::Version)> = packument
            .versions
            .keys()
            .filter_map(|v| node_semver::Version::parse(v).ok().map(|p| (v, p)))
            .collect();
        parsed_versions.sort_by(|a, b| b.1.cmp(&a.1));
        let highest_satisfying = |range_str: &str| -> Option<String> {
            let range = node_semver::Range::parse(range_str).ok()?;
            // Mirror `aube_resolver::pick_version`: prefer the
            // `dist-tags.latest` version when it satisfies the range.
            // npm and pnpm both pin toward the publisher's tagged
            // build over the strictly-highest matching version, and
            // the display line here must agree with what the
            // resolver actually installs.
            if let Some(latest) = packument.dist_tags.get("latest")
                && let Ok(parsed_latest) = node_semver::Version::parse(latest)
                && parsed_latest.satisfies(&range)
                && packument.versions.contains_key(latest)
            {
                return Some(latest.clone());
            }
            parsed_versions
                .iter()
                .find(|(_, parsed)| parsed.satisfies(&range))
                .map(|(raw, _)| (*raw).clone())
        };
        let resolved_version = highest_satisfying(&effective_range)
            .ok_or_else(|| miette!("no version of {} matches {effective_range}", spec.name))?;

        // Build the specifier for package.json.
        // Dist-tags (including "latest") are written as ^version — this matches pnpm's behavior
        // where the lockfile records the resolved version, not the tag name.
        // `--save-exact` drops the `^` so the manifest pins the resolved version.
        //
        // The `npm:` protocol must survive every branch: either the user wrote
        // an alias (`foo@npm:real@range`), which produced `spec.alias`, or they
        // used the bare form (`npm:real@range`), which leaves `alias` empty but
        // keeps the prefix on `orig`. Both cases round-trip back as `npm:...`.
        // `jsr:` is handled separately below, because the manifest form omits
        // the name when the alias equals the JSR name (matching pnpm).
        let is_jsr = spec.jsr_name.is_some();
        let needs_npm_prefix = !is_jsr && (spec.alias.is_some() || orig.starts_with("npm:"));
        let prefix = &default_prefix;
        let pin_to_resolved = spec.range == default_tag
            || packument.dist_tags.contains_key(&spec.range)
            || opts.save_exact;
        // Dist-tags and `--save-exact` both resolve to a concrete version
        // with the configured prefix (empty when `--save-exact`). Non-dist-tag
        // explicit ranges (e.g. `lodash@^4`) are preserved as-is.
        let manual_specifier = if let Some(jsr_name) = spec.jsr_name.as_deref() {
            // jsr:<range> when the manifest key matches the JSR name (the
            // default when the user didn't supply an alias); otherwise we
            // embed the JSR name so the resolver can rebuild the npm-compat
            // name on its next read.
            let effective_range = if pin_to_resolved {
                format!("{prefix}{resolved_version}")
            } else {
                spec.range.clone()
            };
            let alias_matches_jsr_name =
                spec.alias.as_deref() == Some(jsr_name) || spec.alias.is_none();
            if alias_matches_jsr_name {
                format!("jsr:{effective_range}")
            } else {
                format!("jsr:{jsr_name}@{effective_range}")
            }
        } else if pin_to_resolved {
            if needs_npm_prefix {
                format!("npm:{}@{prefix}{resolved_version}", spec.name)
            } else {
                format!("{prefix}{resolved_version}")
            }
        } else if needs_npm_prefix {
            // Preserve npm: protocol for aliases and bare-prefix specs.
            format!("npm:{}@{}", spec.name, spec.range)
        } else {
            spec.range.clone()
        };
        // `--save-catalog` / `--save-catalog-name` short-circuits the
        // `catalogMode` decision: the user explicitly asked for the
        // dep to land in a catalog. `npm:`, `jsr:`, `workspace:`, and
        // pre-`catalog:` specs can't be re-expressed as a catalog
        // reference, so they fall back to the manual specifier and the
        // catalog yaml is left untouched (matches pnpm's `--save-catalog`
        // behavior on workspace deps).
        let exclude_from_catalog = needs_npm_prefix
            || is_jsr
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range);
        let (specifier, display_version) = if let Some(target) = opts.save_catalog.as_deref() {
            decide_save_catalog(
                target,
                &workspace_catalogs,
                spec,
                exclude_from_catalog,
                &manual_specifier,
                &resolved_version,
                &mut catalog_upserts,
                highest_satisfying,
            )
        } else {
            // Apply `catalogMode`. Only the default catalog participates —
            // named catalogs still require the user to write `catalog:<name>`
            // explicitly. `npm:`/alias specs can't be re-expressed as a
            // catalog reference, so they opt out regardless of mode.
            match decide_add_rewrite(
                catalog_mode,
                default_catalog,
                &spec.name,
                &spec.range,
                spec.has_explicit_range,
                &resolved_version,
                needs_npm_prefix || is_jsr,
            ) {
                CatalogRewrite::Manual => (manual_specifier, resolved_version.clone()),
                CatalogRewrite::UseDefaultCatalog => {
                    // The install will resolve against the catalog's range,
                    // not the user's original spec — so the printed version
                    // should reflect what actually lands in `node_modules`.
                    // `strict` + bare `aube add <pkg>` is the case this
                    // matters most for: the user never gave a range, so
                    // `resolved_version` comes from `latest` and can easily
                    // disagree with what the catalog entry picks. Fall back
                    // to `resolved_version` only when the catalog range
                    // can't resolve a version from the packument (shouldn't
                    // happen in practice, but we'd rather print something
                    // than fail the command on a display edge case).
                    let cat_range = default_catalog
                        .and_then(|c| c.get(&spec.name))
                        .cloned()
                        .unwrap_or_default();
                    let catalog_version = highest_satisfying(&cat_range).unwrap_or_else(|| {
                        tracing::debug!(
                            "catalog range {cat_range:?} for {} did not match any packument version; \
                             falling back to user-resolved version for display",
                            spec.name
                        );
                        resolved_version.clone()
                    });
                    ("catalog:".to_string(), catalog_version)
                }
                CatalogRewrite::StrictMismatch {
                    pkg,
                    catalog_range,
                    user_range,
                } => {
                    return Err(miette!(
                        "catalogMode=strict: {pkg}@{user_range} does not match the \
                         default catalog entry `{catalog_range}`. Update the catalog \
                         or rerun with the catalog range."
                    ));
                }
            }
        };

        eprintln!("  + {pkg_name_for_manifest}@{display_version} (specifier: {specifier})");

        // Remove from all dep sections first to avoid duplicates across
        // sections. `--save-peer` intentionally does NOT clear the peer
        // section (see below) — we may end up writing to both peer and
        // dev simultaneously, which is pnpm's `--save-peer` behavior.
        manifest.dependencies.remove(pkg_name_for_manifest);
        manifest.optional_dependencies.remove(pkg_name_for_manifest);
        if !opts.save_peer {
            manifest.peer_dependencies.remove(pkg_name_for_manifest);
        }
        if !(opts.save_peer && opts.save_dev) {
            manifest.dev_dependencies.remove(pkg_name_for_manifest);
        }

        // Add to the appropriate section. When `--save-peer` is paired
        // with `--save-dev`, pnpm writes to BOTH peerDependencies and
        // devDependencies — the peer entry declares what downstream
        // consumers need, and the dev entry makes the local project
        // actually install it for tests and tooling.
        let dep_name = pkg_name_for_manifest.to_string();
        if opts.save_peer {
            manifest
                .peer_dependencies
                .insert(dep_name.clone(), specifier.clone());
            if opts.save_dev {
                manifest.dev_dependencies.insert(dep_name, specifier);
            }
        } else if opts.save_dev {
            manifest.dev_dependencies.insert(dep_name, specifier);
        } else if opts.save_optional {
            manifest.optional_dependencies.insert(dep_name, specifier);
        } else {
            manifest.dependencies.insert(dep_name, specifier);
        }
    }

    // Write the updated package.json. Under `--no-save` callers still
    // write the mutated manifest to disk for the duration of the
    // resolver + install pipeline (both re-read from disk), then
    // restore the original bytes from their snapshot before returning.
    super::write_manifest_dep_sections(&manifest_path, &manifest)?;
    if print_updated {
        eprintln!("Updated package.json");
    }

    // Apply queued `--save-catalog` upserts. Lands once at the end of
    // the per-package loop so the workspace yaml is rewritten at most
    // once per command — `edit_workspace_yaml` no-ops when nothing
    // structural changes (preserving comments under filtered/recursive
    // re-runs that all target the same catalog).
    if !catalog_upserts.is_empty() {
        let yaml_root = crate::dirs::find_workspace_yaml_root(cwd)
            .or_else(|| crate::dirs::find_workspace_root(cwd))
            .unwrap_or_else(|| cwd.to_path_buf());
        let yaml_path = aube_manifest::workspace::workspace_yaml_target(&yaml_root);
        super::catalogs::upsert_catalog_entries(&yaml_path, &catalog_upserts)?;
    }

    Ok(())
}

/// Resolve a `pkg@workspace:<range>` spec against the local workspace
/// and write the user's spec verbatim into the manifest. Skips the
/// registry path entirely — workspace specs only mean anything inside
/// a workspace, so we look the package up in the workspace's
/// `find_workspace_packages` set and error out clearly if it's missing.
///
/// Mirrors pnpm's `pnpm add foo@workspace:*` shape: the manifest entry
/// keeps the literal `workspace:*` (or `workspace:^`, `workspace:~`,
/// `workspace:1.2.0`, …) the user typed, which the install pipeline
/// later resolves to a `link:../foo` symlink.
fn apply_workspace_spec_to_manifest(
    cwd: &Path,
    manifest: &mut aube_manifest::PackageJson,
    spec: &ParsedPkgSpec,
    pkg_name_for_manifest: &str,
    opts: &AddManifestOptions,
) -> miette::Result<()> {
    // Walk up to the workspace root — the cwd may be a subpackage,
    // and `find_workspace_packages` is anchored at the root yaml. Fall
    // back to cwd so a single-package project with no workspace yaml
    // still surfaces a useful error from the package-lookup below.
    let workspace_root = crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
        .unwrap_or_else(|| cwd.to_path_buf());
    let workspace_pkg_dirs = aube_workspace::find_workspace_packages(&workspace_root)
        .into_diagnostic()
        .wrap_err("failed to discover workspace packages")?;

    // Match by the `name` field in each workspace package's manifest,
    // not by directory name — pnpm semantics. Skip dirs whose
    // package.json is unreadable.
    let mut found_version: Option<String> = None;
    for dir in &workspace_pkg_dirs {
        let pkg_manifest = match aube_manifest::PackageJson::from_path(&dir.join("package.json")) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if pkg_manifest.name.as_deref() == Some(spec.name.as_str()) {
            found_version = Some(pkg_manifest.version.unwrap_or_else(|| "0.0.0".to_string()));
            break;
        }
    }
    let Some(workspace_version) = found_version else {
        return Err(miette!(
            "no workspace package named `{}` found at or above {}; \
             `workspace:` specs only resolve against local workspace packages",
            spec.name,
            workspace_root.display()
        ));
    };

    eprintln!(
        "  + {pkg_name_for_manifest}@{workspace_version} (specifier: {})",
        spec.range
    );

    // Mirror the duplicate-section scrub the registry path does.
    manifest.dependencies.remove(pkg_name_for_manifest);
    manifest.optional_dependencies.remove(pkg_name_for_manifest);
    if !opts.save_peer {
        manifest.peer_dependencies.remove(pkg_name_for_manifest);
    }
    if !(opts.save_peer && opts.save_dev) {
        manifest.dev_dependencies.remove(pkg_name_for_manifest);
    }

    let dep_name = pkg_name_for_manifest.to_string();
    let specifier = spec.range.clone();
    if opts.save_peer {
        manifest
            .peer_dependencies
            .insert(dep_name.clone(), specifier.clone());
        if opts.save_dev {
            manifest.dev_dependencies.insert(dep_name, specifier);
        }
    } else if opts.save_dev {
        manifest.dev_dependencies.insert(dep_name, specifier);
    } else if opts.save_optional {
        manifest.optional_dependencies.insert(dep_name, specifier);
    } else {
        manifest.dependencies.insert(dep_name, specifier);
    }
    Ok(())
}

/// Walk the workspace from `cwd` and build a `name → version` map of
/// every workspace package. Returns an empty map outside a workspace
/// or when discovery fails — `aube add` falls back to the registry
/// path in that case, so a partial workspace shouldn't error here.
fn collect_workspace_versions(cwd: &Path) -> std::collections::HashMap<String, String> {
    let workspace_root = match crate::dirs::find_workspace_yaml_root(cwd)
        .or_else(|| crate::dirs::find_workspace_root(cwd))
    {
        Some(root) => root,
        None => return std::collections::HashMap::new(),
    };
    let mut out = std::collections::HashMap::new();
    let dirs = match aube_workspace::find_workspace_packages(&workspace_root) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for dir in dirs {
        let Ok(pkg) = aube_manifest::PackageJson::from_path(&dir.join("package.json")) else {
            continue;
        };
        if let Some(name) = pkg.name {
            out.insert(name, pkg.version.unwrap_or_else(|| "0.0.0".to_string()));
        }
    }
    out
}

/// Write the manifest entry for a `linkWorkspacePackages` match. The
/// resolved `saveWorkspaceProtocol` and the per-invocation
/// `--save-workspace-protocol` / `--no-save-workspace-protocol`
/// override pick the form:
///
/// - rolling: `workspace:^` (or `~`/`*` per `savePrefix`)
/// - true: `workspace:<prefix><version>` (e.g. `workspace:^1.2.3`)
/// - false: `<prefix><version>` (e.g. `^1.2.3`) — the manifest looks
///   like a registry dep but the resolver still links the local copy
///   because aube prefers workspace siblings on bare semver ranges.
///
/// Mirrors the duplicate-section scrub from the registry path so a
/// follow-up `aube add` after a previous `--save-dev` add overwrites
/// the old entry rather than duplicating across sections.
#[allow(clippy::too_many_arguments)]
fn apply_linked_workspace_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    workspace_version: &str,
    save_workspace_protocol: aube_settings::resolved::SaveWorkspaceProtocol,
    workspace_protocol_override: Option<bool>,
    save_prefix: &str,
    opts: &AddManifestOptions,
) {
    use aube_settings::resolved::SaveWorkspaceProtocol;
    // `--no-save-workspace-protocol` forces registry-style; explicit
    // `--save-workspace-protocol` keeps the configured workspace form
    // (defaulting to `rolling` when nothing else picks); otherwise
    // defer to the resolved setting.
    let effective = match workspace_protocol_override {
        Some(false) => SaveWorkspaceProtocol::False,
        Some(true) if matches!(save_workspace_protocol, SaveWorkspaceProtocol::False) => {
            SaveWorkspaceProtocol::Rolling
        }
        _ => save_workspace_protocol,
    };
    let specifier = match effective {
        SaveWorkspaceProtocol::Rolling => {
            // Rolling form drops the version: `workspace:^`. Empty
            // `savePrefix` (`--save-exact`) collapses to
            // `workspace:*` so the rolling sigil still resolves the
            // sibling regardless of its version.
            let sigil = if save_prefix.is_empty() {
                "*"
            } else {
                save_prefix
            };
            format!("workspace:{sigil}")
        }
        SaveWorkspaceProtocol::True => {
            format!("workspace:{save_prefix}{workspace_version}")
        }
        SaveWorkspaceProtocol::False => {
            format!("{save_prefix}{workspace_version}")
        }
    };

    eprintln!("  + {pkg_name_for_manifest}@{workspace_version} (specifier: {specifier})");

    manifest.dependencies.remove(pkg_name_for_manifest);
    manifest.optional_dependencies.remove(pkg_name_for_manifest);
    if !opts.save_peer {
        manifest.peer_dependencies.remove(pkg_name_for_manifest);
    }
    if !(opts.save_peer && opts.save_dev) {
        manifest.dev_dependencies.remove(pkg_name_for_manifest);
    }

    let dep_name = pkg_name_for_manifest.to_string();
    if opts.save_peer {
        manifest
            .peer_dependencies
            .insert(dep_name.clone(), specifier.clone());
        if opts.save_dev {
            manifest.dev_dependencies.insert(dep_name, specifier);
        }
    } else if opts.save_dev {
        manifest.dev_dependencies.insert(dep_name, specifier);
    } else if opts.save_optional {
        manifest.optional_dependencies.insert(dep_name, specifier);
    } else {
        manifest.dependencies.insert(dep_name, specifier);
    }
}

/// Write a git-form spec verbatim into the manifest. Mirrors the
/// duplicate-section scrub of the registry path so re-running
/// `aube add <git-spec>` after a previous registry add overwrites the
/// old entry instead of duplicating it across `dependencies` and
/// `devDependencies`.
///
/// The manifest carries the literal user-typed string
/// (`kevva/is-negative`, `github:user/repo`, …) — preserving the
/// verbatim form keeps the manifest readable, and aube's resolver
/// handles every form `parse_git_spec` recognizes equivalently.
///
/// Limitation: when the user didn't supply an alias, the manifest key
/// is the repo segment of the clone URL (e.g. `is-negative` for
/// `kevva/is-negative`). If the upstream package's `package.json`
/// `name` differs from the repo segment, pass an alias:
/// `aube add my-pkg@kevva/is-negative`.
fn apply_git_spec_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    verbatim_spec: &str,
    opts: &AddManifestOptions,
) {
    eprintln!("  + {pkg_name_for_manifest} (specifier: {verbatim_spec})");

    manifest.dependencies.remove(pkg_name_for_manifest);
    manifest.optional_dependencies.remove(pkg_name_for_manifest);
    if !opts.save_peer {
        manifest.peer_dependencies.remove(pkg_name_for_manifest);
    }
    if !(opts.save_peer && opts.save_dev) {
        manifest.dev_dependencies.remove(pkg_name_for_manifest);
    }

    let dep_name = pkg_name_for_manifest.to_string();
    let specifier = verbatim_spec.to_string();
    if opts.save_peer {
        manifest
            .peer_dependencies
            .insert(dep_name.clone(), specifier.clone());
        if opts.save_dev {
            manifest.dev_dependencies.insert(dep_name, specifier);
        }
    } else if opts.save_dev {
        manifest.dev_dependencies.insert(dep_name, specifier);
    } else if opts.save_optional {
        manifest.optional_dependencies.insert(dep_name, specifier);
    } else {
        manifest.dependencies.insert(dep_name, specifier);
    }
}

/// Write a `file:` / `link:` spec verbatim into the manifest. Same
/// section-scrub semantics as [`apply_git_spec_to_manifest`] — the
/// only difference is the manifest-key derivation (URL repo segment
/// vs path basename) which lives in the parser.
///
/// The manifest carries the literal user-typed string
/// (`file:./pkg`, `link:../sibling`, …) — preserving the verbatim
/// form keeps the manifest readable, and aube's resolver handles
/// every form `LocalSource::parse` recognizes equivalently.
///
/// Limitation: when the user didn't supply an alias, the manifest
/// key is the basename of the path (e.g. `foo` for
/// `file:./packages/foo`, `bundle` for `file:./bundle.tgz`). Pass an
/// alias when the upstream `package.json` `name` differs from the
/// basename: `aube add my-pkg@file:./packages/foo`.
fn apply_local_spec_to_manifest(
    manifest: &mut aube_manifest::PackageJson,
    pkg_name_for_manifest: &str,
    verbatim_spec: &str,
    opts: &AddManifestOptions,
) {
    apply_git_spec_to_manifest(manifest, pkg_name_for_manifest, verbatim_spec, opts);
}

/// Decide what `aube add --save-catalog[=<name>]` should write to the
/// manifest for one package, and queue any catalog-yaml mutation. Pulls
/// the per-package logic out of `update_manifest_for_add` so the main
/// loop stays readable.
///
/// Returns `(manifest_specifier, display_version)`. The display_version
/// is what gets printed on the `+ pkg@<version>` line and reflects what
/// will actually land in `node_modules` after install — not necessarily
/// the version the user originally typed.
#[allow(clippy::too_many_arguments)]
fn decide_save_catalog(
    target: &str,
    workspace_catalogs: &super::CatalogMap,
    spec: &ParsedPkgSpec,
    exclude_from_catalog: bool,
    manual_specifier: &str,
    resolved_version: &str,
    upserts: &mut Vec<CatalogUpsert>,
    highest_satisfying: impl Fn(&str) -> Option<String>,
) -> (String, String) {
    if exclude_from_catalog {
        return (manual_specifier.to_string(), resolved_version.to_string());
    }
    // Manifest specifier. `default` writes plain `catalog:`, named
    // catalogs use `catalog:<name>` (matches pnpm).
    let manifest_specifier = if target == "default" {
        "catalog:".to_string()
    } else {
        format!("catalog:{target}")
    };
    let target_catalog = workspace_catalogs.get(target);
    if let Some(existing_range) = target_catalog.and_then(|c| c.get(&spec.name)) {
        // Already in target catalog — never overwrite.
        let compatible = range_compatible(
            &spec.range,
            spec.has_explicit_range,
            existing_range,
            resolved_version,
        );
        if compatible {
            // Catalog entry covers the user's range — manifest can use
            // `catalog:` and the install will resolve through the
            // existing entry. Display the catalog's resolved version
            // for the same reason `decide_add_rewrite` does.
            let catalog_version = highest_satisfying(existing_range).unwrap_or_else(|| {
                tracing::debug!(
                    "catalog range {existing_range:?} for {} did not match any \
                     packument version; falling back to user-resolved version for display",
                    spec.name
                );
                resolved_version.to_string()
            });
            return (manifest_specifier, catalog_version);
        }
        // Incompatible — preserve the existing catalog entry and fall
        // back to writing the user's spec into the manifest. Matches
        // pnpm/test/saveCatalog.ts:488 (the "never overwrites existing
        // catalogs" test).
        return (manual_specifier.to_string(), resolved_version.to_string());
    }
    // Not in target catalog — queue the addition. The catalog entry
    // mirrors what we'd otherwise write to `package.json`: `manual_specifier`
    // already encodes the right shape — explicit semver ranges pass through,
    // dist-tags resolve to `<save-prefix><resolved-version>`, bare `aube
    // add <pkg>` defaults to the same prefix+resolved form. The npm:/jsr:
    // cases are unreachable here because they hit the `exclude_from_catalog`
    // early return above.
    upserts.push(CatalogUpsert {
        catalog: target.to_string(),
        package: spec.name.clone(),
        range: manual_specifier.to_string(),
    });
    (manifest_specifier, resolved_version.to_string())
}

/// Reject empty values for the allow-build flag with pnpm's
/// verbatim error message.
///
/// Catches the explicit empty form (`--allow-build=`) and the bare
/// form (`--allow-build`), which clap routes through this validator
/// via the `default_missing_value = ""` arg attribute.
///
/// Wording must stay byte-identical to pnpm's: scripts that grep
/// pnpm's stderr for this exact line continue to work after a swap
/// to aube.
fn parse_allow_build_value(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("The --allow-build flag is missing a package name. \
             Please specify the package name(s) that are allowed to run installation scripts."
            .to_string())
    } else {
        Ok(s.to_string())
    }
}

/// Apply `--allow-build=<pkg>` flags by writing each package as `true`
/// to the project's `allowBuilds` map (workspace yaml or
/// `package.json#aube.allowBuilds`). Errors when any name is already
/// pinned to `false` — flipping an explicit deny should be a deliberate
/// edit, not a side effect. Mirrors pnpm's `--allow-build=<pkg>` /
/// `allowBuilds: false` conflict check.
///
/// Merge precedence matches `build_policy_from_sources` in
/// `install/lifecycle.rs`: workspace yaml wins over manifest. So a
/// `false` in `pnpm-workspace.yaml` blocks a `--allow-build` even when
/// the manifest's `pnpm.allowBuilds` flips it to `true`.
fn apply_allow_build_flags(cwd: &std::path::Path, names: &[String]) -> miette::Result<()> {
    let manifest_path = cwd.join("package.json");
    let manifest = aube_manifest::PackageJson::from_path(&manifest_path)
        .into_diagnostic()
        .wrap_err("failed to read package.json for --allow-build")?;
    let workspace = aube_manifest::WorkspaceConfig::load(cwd)
        .into_diagnostic()
        .wrap_err("failed to read workspace config for --allow-build")?;

    let mut existing: std::collections::BTreeMap<String, aube_manifest::AllowBuildRaw> =
        manifest.pnpm_allow_builds();
    // Workspace yaml overrides manifest — overwriting (not
    // `or_insert`-ing) keeps the precedence aligned with
    // `build_policy_from_sources`.
    for (k, v) in workspace.allow_builds_raw() {
        existing.insert(k, v);
    }

    let mut conflicts = Vec::new();
    for name in names {
        if matches!(
            existing.get(name),
            Some(aube_manifest::AllowBuildRaw::Bool(false))
        ) {
            conflicts.push(name.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(miette!(
            "The following dependencies are ignored by the root project, but are allowed to be built by the current command: {}",
            conflicts.join(", ")
        ));
    }

    aube_manifest::workspace::add_to_allow_builds(
        cwd,
        names,
        aube_manifest::workspace::AllowBuildsWriteMode::Approve,
    )
    .into_diagnostic()
    .wrap_err("failed to write --allow-build entries")?;
    Ok(())
}

/// Parse the user-typed `aube add` arguments into the
/// `(name, spec)` tuples the Bun-style scanner contract expects.
/// Git/local/workspace/jsr/aliased specs are excluded — they
/// route through code paths the scanner has no useful answer
/// for, and forcing operators to special-case them in every
/// scanner module would be friction without benefit. `spec`
/// is the raw range string (e.g. `^4.17.21`, `latest`) — the
/// scanner decides how (or whether) to apply version-range
/// policy.
fn scanner_packages_for(packages: &[String]) -> Vec<super::security_scanner::ScannerPackage> {
    let mut out = Vec::with_capacity(packages.len());
    for raw in packages {
        let Ok(spec) = parse_pkg_spec(raw) else {
            continue;
        };
        if spec.git_spec.is_some()
            || spec.local_spec.is_some()
            || spec.jsr_name.is_some()
            || spec.alias.is_some()
            || aube_util::pkg::is_workspace_spec(&spec.range)
            || aube_util::pkg::is_catalog_spec(&spec.range)
        {
            continue;
        }
        out.push(super::security_scanner::ScannerPackage {
            name: spec.name,
            spec: spec.range,
        });
    }
    out
}

/// Resolve the on-disk lockfile path that a normal `add` would write
/// to in `project_dir`. Mirrors the `LockfileKind` -> filename mapping
/// inside `aube_lockfile::write_lockfile_as` so the snapshot/restore
/// path under `--no-save` lines up byte-for-byte with whatever
/// `write_lockfile_preserving_existing` produces, including non-aube
/// lockfiles (`pnpm-lock.yaml`, `package-lock.json`, `yarn.lock`,
/// `bun.lock`, `npm-shrinkwrap.json`). When no lockfile exists yet the
/// resolver falls back to aube's own format.
fn lockfile_path_for_project(project_dir: &std::path::Path) -> std::path::PathBuf {
    use aube_lockfile::LockfileKind;
    let kind =
        aube_lockfile::detect_existing_lockfile_kind(project_dir).unwrap_or(LockfileKind::Aube);
    let filename = match kind {
        LockfileKind::Aube => aube_lockfile::aube_lock_filename(project_dir),
        LockfileKind::Pnpm => aube_lockfile::pnpm_lock_filename(project_dir),
        other => other.filename().to_string(),
    };
    project_dir.join(filename)
}

async fn run_filtered(
    args: AddArgs,
    filter: &aube_workspace::selector::EffectiveFilter,
) -> miette::Result<()> {
    if args.packages.is_empty() {
        return Err(miette!("no packages specified"));
    }
    let cwd = crate::dirs::cwd()?;
    // The workspace root — not the child `cwd` — is what owns the
    // lockfile and the project lock in yarn / npm / bun monorepos.
    // Taking the lock or snapshotting the lockfile against `cwd` would
    // target a stale subpackage path, letting `install::run` (which
    // walks up) mutate the real root lockfile and then silently skip
    // the restore under `--no-save`.
    let (root, matched) = super::select_workspace_packages(&cwd, filter, "add")?;
    let _lock = super::take_project_lock(&root)?;

    // `--allow-build=<pkg>` writes against the workspace root (where
    // `allowBuilds` lives) — same as the non-filtered path. Run before
    // any per-package manifest mutation so a conflict can't leave the
    // child manifests half-mutated.
    if !args.allow_build.is_empty() {
        apply_allow_build_flags(&root, &args.allow_build)?;
    }

    // Bun-style scanner runs once against the workspace root for
    // the filtered path — every filter-matched importer shares
    // the same `args.packages` list, so re-scanning per child
    // would just duplicate the work without producing new
    // verdicts.
    let scanner = super::with_settings_ctx(&root, aube_settings::resolved::security_scanner);
    if !scanner.is_empty() {
        let scanner_packages = scanner_packages_for(&args.packages);
        super::security_scanner::run_scanner(&scanner, &root, &scanner_packages).await?;
    }

    let mut snapshots = Vec::new();
    let lockfile_path = lockfile_path_for_project(&root);
    let root_lockfile_snapshot = if args.no_save {
        match std::fs::read(&lockfile_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot lockfile for --no-save");
            }
        }
    } else {
        None
    };

    let result: miette::Result<()> = async {
        for pkg in &matched {
            let manifest_path = pkg.dir.join("package.json");
            if args.no_save {
                let manifest_bytes = std::fs::read(&manifest_path)
                    .into_diagnostic()
                    .wrap_err("failed to snapshot package.json for --no-save")?;
                snapshots.push((manifest_path.clone(), manifest_bytes));
            }
            update_manifest_for_add(
                &pkg.dir,
                &args.packages,
                AddManifestOptions::from_args(&args),
                !args.no_save,
            )
            .await?;
        }

        let mut install_opts = install::InstallOptions::with_mode(super::chained_frozen_mode(
            install::FrozenMode::Fix,
        ));
        install_opts.workspace_filter = filter.clone();
        install::run(install_opts).await?;
        Ok(())
    }
    .await;

    let restore_errors = if args.no_save {
        let mut errors: Vec<miette::Report> = Vec::new();
        let restored = snapshots.len();
        for (manifest_path, manifest_bytes) in snapshots {
            if let Err(e) = aube_util::fs_atomic::atomic_write(&manifest_path, &manifest_bytes) {
                errors.push(
                    Result::<(), _>::Err(e)
                        .into_diagnostic()
                        .wrap_err_with(|| {
                            format!(
                                "failed to restore original package.json after --no-save at {}",
                                manifest_path.display()
                            )
                        })
                        .unwrap_err(),
                );
            }
        }
        let lockfile_restore = match &root_lockfile_snapshot {
            Some(bytes) => aube_util::fs_atomic::atomic_write(&lockfile_path, bytes),
            None => match std::fs::remove_file(&lockfile_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = lockfile_restore {
            errors.push(
                Result::<(), _>::Err(e)
                    .into_diagnostic()
                    .wrap_err("failed to restore original lockfile after --no-save")
                    .unwrap_err(),
            );
        }
        if errors.is_empty() {
            eprintln!(
                "Restored {} and lockfile (--no-save)",
                pluralizer::pluralize("package.json file", restored as isize, true)
            );
        }
        errors
    } else {
        Vec::new()
    };

    result?;
    if let Some(first) = restore_errors.into_iter().next() {
        return Err(first);
    }
    Ok(())
}

/// `aube add -g <pkg>...` — install into an isolated global install dir
/// and symlink the resulting binaries into the global bin dir.
///
/// The project-local `run` path assumes a `package.json` in the cwd. The
/// global path deliberately does *not* — it creates a fresh install dir
/// under `<pkg_dir>/<pid>-<ts>`, writes a minimal `package.json` so the
/// normal install pipeline has something to resolve against, chdirs into
/// it, and then re-enters `run` with the local flow. After the install
/// lands we scan the install dir's `node_modules/.bin/` and symlink each
/// bin into `<bin_dir>`.
///
/// The freshly-created install dir is cleaned up if *any* step after
/// creation fails — inner install, manifest re-read, hash pointer, or
/// bin linking. Without this guard every failed `add -g` would leak a
/// subdir that `scan_packages` ignores (no hash symlink) but disk space
/// keeps.
async fn run_global(
    packages: &[String],
    allow_build: Vec<String>,
    lockfile: crate::cli_args::LockfileArgs,
    network: crate::cli_args::NetworkArgs,
    virtual_store: crate::cli_args::VirtualStoreArgs,
) -> miette::Result<()> {
    use super::global;

    let mut layout = global::GlobalLayout::resolve()?;
    let install_dir_raw = global::create_install_dir(&layout.pkg_dir)?;

    // Canonicalize both the install dir and the layout's pkg dir so the
    // comparisons downstream (`find_package`, `remove_package`) see the
    // same form regardless of filesystem-level symlinks. On macOS the
    // default temp dir `/var/folders/...` is itself a symlink to
    // `/private/var/folders/...`, and `scan_packages` always canonicalizes
    // the hash-symlink targets — so without normalizing our side the
    // `!=` / `starts_with` checks all come out wrong and we either leak
    // orphan install dirs or leave duplicate hash pointers behind.
    // Use `dirs::canonicalize` (not `std::fs::canonicalize`) so the
    // result on Windows is a plain drive path, not the `\\?\C:\…`
    // verbatim form. `link_bin_entries` later concatenates this dir
    // into the relative bin-shim target via `%~dp0\{rel}`; a verbatim
    // prefix in `{rel}` produces a path neither `cmd.exe` nor Node can
    // resolve and surfaces as `Cannot find module '<bin>\?\<target>'`.
    let install_dir = crate::dirs::canonicalize(&install_dir_raw)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to canonicalize install dir {}",
                install_dir_raw.display()
            )
        })?;
    if let Ok(canon) = crate::dirs::canonicalize(&layout.pkg_dir) {
        layout.pkg_dir = canon;
    }

    // Everything from here until the final `Ok(())` must run under a
    // cleanup guard so a mid-flight failure doesn't leave an orphan dir
    // or a dangling hash pointer under the global pkg dir. We snapshot
    // the pkg dir's existing hash pointers before running, then on
    // error remove any new pointers that appeared plus the install dir.
    let before: std::collections::HashSet<std::path::PathBuf> = std::fs::read_dir(&layout.pkg_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    let result = run_global_inner(
        packages,
        allow_build,
        &layout,
        &install_dir,
        lockfile,
        network,
        virtual_store,
    )
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&install_dir);
        if let Ok(entries) = std::fs::read_dir(&layout.pkg_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if before.contains(&path) {
                    continue;
                }
                // Only unlink pointers that resolved to our install dir —
                // don't touch pointers for other live global installs.
                // Use `dirs::canonicalize` so the equality check against
                // `install_dir` (also stripped of any Windows `\\?\`
                // verbatim prefix) actually matches.
                if let Ok(target) = crate::dirs::canonicalize(&path)
                    && target == install_dir
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    result
}

async fn run_global_inner(
    packages: &[String],
    allow_build: Vec<String>,
    layout: &super::global::GlobalLayout,
    install_dir: &std::path::Path,
    lockfile: crate::cli_args::LockfileArgs,
    network: crate::cli_args::NetworkArgs,
    virtual_store: crate::cli_args::VirtualStoreArgs,
) -> miette::Result<()> {
    use super::global;

    // Seed a minimal package.json so the resolver has a project to work
    // against. We never persist metadata beyond this; the install dir is
    // throwaway and lives only to host `node_modules/`.
    let seed = serde_json::json!({
        "name": "aube-global",
        "version": "0.0.0",
        "private": true,
    });
    let seed_str = serde_json::to_string_pretty(&seed)
        .into_diagnostic()
        .wrap_err("failed to serialize seed package.json")?;
    aube_util::fs_atomic::atomic_write(
        &install_dir.join("package.json"),
        format!("{seed_str}\n").as_bytes(),
    )
    .into_diagnostic()
    .wrap_err("failed to write seed package.json")?;

    // chdir into the install dir before anything reads `dirs::cwd()` so
    // the whole install pipeline targets the fresh directory. See the
    // invariant note on `run_global` above — this works only because
    // nothing upstream has called `dirs::cwd()` yet.
    std::env::set_current_dir(install_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to chdir into {}", install_dir.display()))?;
    crate::dirs::set_cwd(install_dir)?;

    // Build registry map before the inner `run` takes its own view of
    // the config — we need it for the cache key hash.
    let npm_config = aube_registry::config::NpmConfig::load(install_dir);
    let mut registries: BTreeMap<String, String> = BTreeMap::new();
    registries.insert("default".to_string(), npm_config.registry.clone());
    for (scope, url) in &npm_config.scoped_registries {
        registries.insert(scope.clone(), url.clone());
    }

    // Re-enter the local add path inside the throwaway project. Global
    // installs pin the exact resolved version — matches pnpm's
    // `pnpm add -g` behavior (no `^` in the synthetic manifest) and
    // keeps the cache key stable across re-adds.
    let inner = AddArgs {
        packages: packages.to_vec(),
        save_dev: false,
        save_exact: true,
        global: false,
        save_optional: false,
        ignore_scripts: false,
        no_save: false,
        save_peer: false,
        save_workspace_protocol: false,
        no_save_workspace_protocol: false,
        // The throwaway install dir is never a workspace root, but
        // `run_global_inner` is the one place in aube that chdirs
        // after startup — if a future refactor reads `dirs::cwd()`
        // before command dispatch the synthetic `AddArgs` could end
        // up being evaluated against the *caller's* cwd. Opting out
        // of the check here keeps `aube add -g` robust against that
        // regression without relying on the chdir-ordering invariant.
        ignore_workspace_root_check: true,
        workspace: false,
        save_catalog: false,
        save_catalog_name: None,
        // Forward `--allow-build=<pkg>` flags from the outer invocation:
        // the inner `run()` writes them to the throwaway install dir's
        // `package.json#aube.allowBuilds` (no workspace yaml exists
        // there) before lifecycle scripts run, matching pnpm's
        // `pnpm add -g --allow-build=<pkg>` behavior. Without this the
        // outer flag is silently dropped — under `strictDepBuilds=true`
        // the install then errors with "must be reviewed before
        // install" even when the user explicitly approved the dep
        // (see Discussion #617).
        allow_build,
        // Propagate the outer caller's flag groups through so the inner
        // run()'s `install_overrides()` calls reset the global slots to the
        // same values rather than wiping them — `set_registry_override`
        // backs an RwLock that always overwrites, unlike the OnceLock
        // siblings, so a `Default` here would silently drop `--registry`.
        lockfile,
        network,
        virtual_store,
    };
    Box::pin(run(
        inner,
        aube_workspace::selector::EffectiveFilter::default(),
    ))
    .await?;

    // Re-read the install dir's package.json to get the resolved alias
    // list. Anything in `dependencies` at this point was added by the
    // inner run; we stamp a hash pointer on that set.
    let manifest_raw = std::fs::read_to_string(install_dir.join("package.json"))
        .into_diagnostic()
        .wrap_err("failed to re-read install dir package.json")?;
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_raw)
        .into_diagnostic()
        .wrap_err("failed to parse install dir package.json")?;
    let aliases: Vec<String> = manifest_json
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    // Commit the new install *before* tearing down any prior ones. If
    // the hash pointer or bin-link step fails, the outer cleanup guard
    // still wipes the new install dir, but the user's previous global
    // install is untouched — they're never left without a working copy.
    // Capture every prior install whose pointer (or aliases) overlaps
    // the new one *before* we touch the filesystem. We can't scan for
    // priors after the new pointer lands, because the overwrite loses
    // the previous target — `find_package` would return our fresh
    // install instead. Two kinds of prior matter:
    //
    // 1. The install a same-hash pointer used to point at (the caller
    //    re-ran `add -g` with the exact same alias set).
    // 2. Installs that own one of the new aliases under a *different*
    //    hash (alias set grew/shrank).
    let hash = global::cache_key(&aliases, &registries);
    let hash_ptr = global::hash_link(&layout.pkg_dir, &hash);
    let mut priors: Vec<global::GlobalPackageInfo> = Vec::new();
    // `dirs::canonicalize` for the same Windows-prefix reason as above —
    // we compare against `install_dir`, which is itself stripped.
    if let Ok(existing_target) = crate::dirs::canonicalize(&hash_ptr)
        && existing_target != install_dir
    {
        priors.extend(
            global::scan_packages(&layout.pkg_dir)
                .into_iter()
                .filter(|p| p.install_dir == existing_target),
        );
    }
    for alias in &aliases {
        if let Some(existing) = global::find_package(&layout.pkg_dir, alias)
            && existing.install_dir != install_dir
            && existing.hash != hash
            && !priors.iter().any(|p| p.hash == existing.hash)
        {
            priors.push(existing);
        }
    }

    // Commit the new install *before* tearing down the priors. If the
    // hash pointer or bin-link step fails, the outer cleanup guard
    // wipes the new install dir but the priors survive — users never
    // end up with no working copy.
    global::symlink_force(install_dir, &hash_ptr)?;
    // Honor extendNodePath / preferSymlinkedExecutables for global bins too —
    // settings resolved from the user's `.npmrc` via the normal cwd-walking
    // chain starting at the throwaway install dir, which lives under
    // `~/.aube/global/` and will still pick up the user-level `.npmrc`.
    // `hidden_modules_dir = None`: the global install lays packages out
    // at `<install>/node_modules/<name>` (hoisted shape, no `.aube/`
    // store), so the bin's `$basedir/..` already lands the resolver on
    // every transitive — no second NODE_PATH entry needed.
    let shim_opts = super::with_settings_ctx(install_dir, |ctx| aube_linker::BinShimOptions {
        extend_node_path: aube_settings::resolved::extend_node_path(ctx),
        prefer_symlinked_executables: aube_settings::resolved::prefer_symlinked_executables(ctx),
        hidden_modules_dir: None,
    });
    let linked = global::link_bins(install_dir, &layout.bin_dir, &aliases, shim_opts)?;

    // Now safe to drop priors. Errors here are non-fatal — the new
    // install is already live — but we still surface them so the user
    // knows they have leftover state.
    //
    // If a prior shares the new hash, its pointer is already pointing
    // at the *new* install dir (we overwrote it a few lines up). Deleting
    // the pointer in that case would break the live install, so we only
    // wipe the prior's physical dir + bins.
    for prior in &priors {
        let res = if prior.hash == hash {
            let bins = global::bin_names_for(&prior.install_dir, &prior.aliases);
            global::unlink_bins(&prior.install_dir, &layout.bin_dir, &bins);
            std::fs::remove_dir_all(&prior.install_dir)
                .or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })
                .map_err(|e| {
                    miette::miette!(
                        code = aube_codes::errors::ERR_AUBE_REMOVE_PRIOR_INSTALL_DIR,
                        "failed to remove prior install dir: {e}"
                    )
                })
        } else {
            global::remove_package(prior, layout)
        };
        if let Err(e) = res {
            eprintln!("warning: failed to remove prior global install: {e}");
        }
    }

    if !linked.is_empty() {
        eprintln!(
            "Linked {} into {}",
            pluralizer::pluralize("bin", linked.len() as isize, true),
            layout.bin_dir.display()
        );
    }

    Ok(())
}

/// Atomic file write. Tempfile in the same dir, fsync, rename over
/// the target. Caller uses this for package.json mutation in add /
/// remove / workspace writes so a crash mid-write cannot corrupt
/// the user's manifest. Rename is atomic on POSIX, on Windows
/// MoveFileEx gives the same guarantee post Win10.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pkg_spec_name_only() {
        let s = parse_pkg_spec("lodash").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
        assert!(s.alias.is_none());
        assert!(s.jsr_name.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_with_version() {
        let s = parse_pkg_spec("lodash@^4.17.0").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "^4.17.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_exact_version() {
        let s = parse_pkg_spec("lodash@4.17.21").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "4.17.21");
    }

    #[test]
    fn test_parse_pkg_spec_scoped() {
        let s = parse_pkg_spec("@babel/core").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_with_version() {
        let s = parse_pkg_spec("@babel/core@^7.24.0").unwrap();
        assert_eq!(s.name, "@babel/core");
        assert_eq!(s.range, "^7.24.0");
    }

    #[test]
    fn test_parse_pkg_spec_dist_tag() {
        let s = parse_pkg_spec("lodash@latest").unwrap();
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_npm_bare() {
        // npm:string-width@^4.2.0 — no alias, just resolves real package
        let s = parse_pkg_spec("npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_full() {
        // string-width-cjs@npm:string-width@^4.2.0
        let s = parse_pkg_spec("string-width-cjs@npm:string-width@^4.2.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("string-width-cjs"));
        assert_eq!(s.name, "string-width");
        assert_eq!(s.range, "^4.2.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_scoped() {
        // my-react@npm:@preact/compat@^17.0.0
        let s = parse_pkg_spec("my-react@npm:@preact/compat@^17.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-react"));
        assert_eq!(s.name, "@preact/compat");
        assert_eq!(s.range, "^17.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_npm_alias_no_version() {
        // my-lodash@npm:lodash
        let s = parse_pkg_spec("my-lodash@npm:lodash").unwrap();
        assert_eq!(s.alias.as_deref(), Some("my-lodash"));
        assert_eq!(s.name, "lodash");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_no_range() {
        // jsr:@std/collections — default alias is the JSR name itself
        let s = parse_pkg_spec("jsr:@std/collections").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "latest");
        assert!(!s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_bare_with_range() {
        let s = parse_pkg_spec("jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("@std/collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_jsr_aliased() {
        let s = parse_pkg_spec("collections@jsr:@std/collections@^1.0.0").unwrap();
        assert_eq!(s.alias.as_deref(), Some("collections"));
        assert_eq!(s.name, "@jsr/std__collections");
        assert_eq!(s.jsr_name.as_deref(), Some("@std/collections"));
        assert_eq!(s.range, "^1.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_jsr_rejects_unscoped() {
        let err = parse_pkg_spec("jsr:collections").unwrap_err();
        assert!(
            err.to_string().contains("JSR packages must be scoped"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_pkg_spec_git_bare_github_shorthand() {
        let s = parse_pkg_spec("kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.name, "is-negative");
        assert_eq!(s.range, "kevva/is-negative");
        assert!(s.alias.is_none());
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_git_github_protocol() {
        let s = parse_pkg_spec("github:user/repo").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("github:user/repo"));
        assert_eq!(s.name, "repo");
        assert_eq!(s.range, "github:user/repo");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_git_url_with_committish() {
        let spec = "git+https://github.com/owner/repo.git#tag/with/slash";
        let s = parse_pkg_spec(spec).unwrap();
        assert_eq!(s.git_spec.as_deref(), Some(spec));
        // Repo segment derived before the `#`/`?` slice, then `.git` stripped.
        assert_eq!(s.name, "repo");
        assert_eq!(s.range, spec);
    }

    #[test]
    fn test_parse_pkg_spec_git_alias() {
        let s = parse_pkg_spec("my-alias@kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "kevva/is-negative");
    }

    #[test]
    fn test_parse_pkg_spec_git_alias_skips_url_derivation() {
        // When an alias is given, the manifest key comes from the alias
        // — `repo_name_from_clone_url` should not be consulted, so a
        // pathless URL like `git+https://example.com/` doesn't error
        // out the way it does without an alias.
        let s = parse_pkg_spec("my-alias@git+https://example.com/").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("git+https://example.com/"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
    }

    #[test]
    fn test_parse_pkg_spec_file_relative() {
        let s = parse_pkg_spec("file:./local/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./local/pkg"));
        assert_eq!(s.name, "pkg");
        assert_eq!(s.range, "file:./local/pkg");
        assert!(s.alias.is_none());
        assert!(s.has_explicit_range);
    }

    #[test]
    fn test_parse_pkg_spec_link_relative() {
        let s = parse_pkg_spec("link:./local/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:./local/pkg"));
        assert_eq!(s.name, "pkg");
        assert_eq!(s.range, "link:./local/pkg");
        assert!(s.alias.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_file_absolute() {
        let s = parse_pkg_spec("file:/abs/pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:/abs/pkg"));
        assert_eq!(s.name, "pkg");
    }

    #[test]
    fn test_parse_pkg_spec_file_tarball_strips_extension() {
        // Basename of `bundle.tgz` lands as `"bundle"` — the manifest
        // key shouldn't carry the archive suffix.
        let s = parse_pkg_spec("file:./bundle.tgz").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./bundle.tgz"));
        assert_eq!(s.name, "bundle");
    }

    #[test]
    fn test_parse_pkg_spec_file_alias() {
        let s = parse_pkg_spec("my-alias@file:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "file:./pkg");
    }

    #[test]
    fn test_parse_pkg_spec_link_alias() {
        let s = parse_pkg_spec("my-alias@link:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
        assert_eq!(s.range, "link:./pkg");
    }

    #[test]
    fn test_parse_pkg_spec_local_alias_skips_basename_derivation() {
        // When an alias is given, the manifest key comes from the
        // alias — `basename_from_local_path` should not be consulted,
        // so a pathless spec like `file:` doesn't error out the way
        // it does without an alias.
        let s = parse_pkg_spec("my-alias@file:").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:"));
        assert_eq!(s.alias.as_deref(), Some("my-alias"));
        assert_eq!(s.name, "my-alias");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_not_git_or_local() {
        // Scoped npm names must not be misclassified as git or local specs.
        let s = parse_pkg_spec("@scope/pkg").unwrap();
        assert!(s.git_spec.is_none());
        assert!(s.local_spec.is_none());
        assert_eq!(s.name, "@scope/pkg");
        assert_eq!(s.range, "latest");
    }

    #[test]
    fn test_parse_pkg_spec_bare_user_repo_not_local() {
        // `user/repo` is a git shorthand (handled by the git-spec
        // branch) and must not collide with the file/link path here.
        let s = parse_pkg_spec("kevva/is-negative").unwrap();
        assert!(s.local_spec.is_none());
        assert!(s.git_spec.is_some());
    }

    #[test]
    fn test_parse_pkg_spec_scoped_alias_for_local() {
        // `@scope/alias@file:./pkg` — the scoped name is the manifest
        // key, the local spec is preserved verbatim.
        let s = parse_pkg_spec("@my-scope/alias@file:./pkg").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./pkg"));
        assert_eq!(s.alias.as_deref(), Some("@my-scope/alias"));
        assert_eq!(s.name, "@my-scope/alias");
    }

    #[test]
    fn test_parse_pkg_spec_scoped_alias_for_git() {
        // Same shape with a git spec on the right-hand side.
        let s = parse_pkg_spec("@my-scope/alias@kevva/is-negative").unwrap();
        assert_eq!(s.git_spec.as_deref(), Some("kevva/is-negative"));
        assert_eq!(s.alias.as_deref(), Some("@my-scope/alias"));
        assert_eq!(s.name, "@my-scope/alias");
    }

    #[test]
    fn test_parse_pkg_spec_file_uncompressed_tarball_strips_extension() {
        // `.tar` (uncompressed) should strip alongside `.tgz` /
        // `.tar.gz` so the manifest key isn't littered with archive
        // suffixes.
        let s = parse_pkg_spec("file:./bundle.tar").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("file:./bundle.tar"));
        assert_eq!(s.name, "bundle");
    }

    #[test]
    fn test_parse_pkg_spec_bare_absolute_path() {
        // discussions/497 — `aube add /path/to/library-foo/` used to
        // fall through to the registry and 405. It now normalizes to
        // `link:/path/to/library-foo/` and routes through the local
        // branch.
        let s = parse_pkg_spec("/path/to/library-foo/").unwrap();
        assert_eq!(s.local_spec.as_deref(), Some("link:/path/to/library-foo/"));
        assert_eq!(s.name, "library-foo");
    }

    #[test]
    fn test_parse_pkg_spec_bare_relative_path() {
        for input in ["./lib", "../lib", "../../foo/bar"] {
            let s = parse_pkg_spec(input).unwrap();
            let local = s.local_spec.expect("relative path should detect as local");
            assert_eq!(local, format!("link:{input}"));
        }
    }

    #[test]
    fn test_parse_pkg_spec_bare_tilde_path_expands() {
        // Resolver has no tilde handling, so the verbatim spec stored
        // in the manifest must already be absolute. The exact home
        // path differs by platform (`/home/…` on Linux,
        // `C:\Users\…` on Windows), and `Path::join` mixes separators
        // (`C:\Users\runneradmin\proj/lib`) — only what matters for
        // this test is that the literal `~` is gone and the basename
        // resolved to `lib`.
        let s = parse_pkg_spec("~/proj/lib").unwrap();
        let local = s.local_spec.expect("~/ path should detect as local");
        assert!(
            local.starts_with("link:"),
            "expected link: prefix in `{local}`"
        );
        assert!(
            !local.contains('~'),
            "tilde must be expanded eagerly, got `{local}`"
        );
        assert_eq!(s.name, "lib");
    }

    #[test]
    fn test_parse_pkg_spec_bare_tarball_uses_file_protocol() {
        // pnpm parity: bare paths default to `link:` for directories
        // and `file:` for tarballs. Basename-strip is shared with the
        // explicit `file:./foo.tgz` branch.
        let s = parse_pkg_spec("./vendor/local-helper-1.0.0.tgz").unwrap();
        assert_eq!(
            s.local_spec.as_deref(),
            Some("file:./vendor/local-helper-1.0.0.tgz")
        );
        assert_eq!(s.name, "local-helper-1.0.0");
    }

    #[test]
    fn test_parse_pkg_spec_short_alias_not_drive_letter() {
        // `a:1.0.0` looks superficially like a Windows drive form
        // (`<letter>:`) but has no path separator after the colon —
        // it's a single-character npm alias and must NOT be
        // reclassified as a local path.
        let s = parse_pkg_spec("a:1.0.0").unwrap();
        assert!(s.local_spec.is_none());
        assert!(s.git_spec.is_none());
    }

    #[test]
    fn test_parse_pkg_spec_windows_drive_letter() {
        for input in ["C:/projects/lib", "c:\\projects\\lib"] {
            let s = parse_pkg_spec(input).unwrap();
            let local = s
                .local_spec
                .expect("drive-letter path should detect as local");
            assert_eq!(local, format!("link:{input}"));
            // Basename derivation must split on `\` too — `lib`, not
            // the whole `c:\projects\lib`.
            assert_eq!(s.name, "lib");
        }
    }

    #[test]
    fn test_parse_pkg_spec_windows_backslash_relative() {
        // `..\lib` and `.\lib` are the Windows shells' path-typing
        // convention; both must route through the local branch and
        // basename-derive to `lib`, not the verbatim string.
        for input in ["..\\lib", ".\\lib"] {
            let s = parse_pkg_spec(input).unwrap();
            assert!(s.local_spec.is_some(), "`{input}` should detect as local");
            assert_eq!(s.name, "lib", "wrong basename for `{input}`");
        }
    }
}
