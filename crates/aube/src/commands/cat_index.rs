//! `aube cat-index <pkg@version>` — print the cached package index JSON.
//!
//! Prints the index that `aube fetch`/`aube install` writes under
//! `~/.cache/aube/index/<name>@<version>.json`: a mapping of relative paths
//! in the package to their store file hashes. Useful for debugging linker
//! behavior or confirming which files landed in the CAS.
//!
//! The package must have been fetched by aube at least once — if the cache
//! is cold for that version, we surface a friendly error pointing at
//! `aube fetch`. This is a read-only introspection command: no lockfile,
//! no node_modules, no project lock.

use clap::Args;
use miette::{IntoDiagnostic, miette};

#[derive(Debug, Args)]
pub struct CatIndexArgs {
    /// Package to inspect, in `name@version` form (e.g. `lodash@4.17.21`,
    /// `@babel/core@7.26.0`).
    ///
    /// An exact version is required — ranges and dist-tags aren't
    /// resolved here.
    pub package: String,
}

pub async fn run(args: CatIndexArgs) -> miette::Result<()> {
    let (name, version) = split_name_version(&args.package).ok_or_else(|| {
        miette!(
            "expected `name@version`, got `{}`\nhelp: specify an exact version like `lodash@4.17.21`",
            args.package
        )
    })?;

    let cwd = crate::dirs::project_root_or_cwd()?;
    let store = crate::commands::open_store(&cwd)?;

    // Read the cached index file directly instead of routing through
    // `Store::load_index` — that helper silently *deletes* the cache
    // entry if it detects the underlying store files are missing, which
    // would be a surprising mutation from a read-only introspection
    // command (the user would see "no cached index" when the JSON was
    // in fact present the moment before and has now been removed).
    // Re-serialize the parsed index so the output is pretty-printed the
    // same way load_index would have given us.
    // Validate through the same grammar `Store::save_index` enforces
    // so a user passing `aube cat-index ../../evil 1.0.0` gets a clear
    // refusal instead of a surprising path outside `index_dir()`.
    let safe_name = aube_store::validate_and_encode_name(name)
        .ok_or_else(|| miette!("invalid package name: {name:?}"))?;
    if !aube_store::validate_version(version) {
        return Err(miette!("invalid version: {version:?}"));
    }
    let index_path = store
        .index_dir()
        .join(format!("{safe_name}@{version}.json"));
    let content = std::fs::read_to_string(&index_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            miette!(
                "no cached index for {name}@{version}\nhelp: run `aube fetch` or `aube install` to populate the store first"
            )
        } else {
            miette!("failed to read {}: {e}", index_path.display())
        }
    })?;
    let index: aube_store::PackageIndex = serde_json::from_str(&content)
        .into_diagnostic()
        .map_err(|e| {
            miette!(
                "cached index for {name}@{version} is corrupt: {e}\nhelp: re-run `aube fetch` to regenerate it"
            )
        })?;

    let json = serde_json::to_string_pretty(&index)
        .into_diagnostic()
        .map_err(|e| miette!("failed to serialize index: {e}"))?;
    println!("{json}");

    Ok(())
}

/// Split `name@version` into its parts, respecting scoped packages.
/// Returns `None` if no `@version` is present, or if the version half
/// is empty (`lodash@`, `@babel/core@`) — cat-index needs an exact
/// version, so both bare names and trailing-`@` typos are rejected up
/// front so the user gets the format hint instead of the misleading
/// "cache cold, run aube fetch" error.
fn split_name_version(input: &str) -> Option<(&str, &str)> {
    let (name, version) = if let Some(rest) = input.strip_prefix('@') {
        // Scoped: @scope/name@version — the first `@` is the scope sigil.
        let slash = rest.find('/')?;
        let after_slash = &rest[slash + 1..];
        let at = after_slash.find('@')?;
        let name_end = 1 + slash + 1 + at;
        (&input[..name_end], &input[name_end + 1..])
    } else {
        let at = input.find('@')?;
        (&input[..at], &input[at + 1..])
    };

    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

#[cfg(test)]
mod tests {
    use super::split_name_version;

    #[test]
    fn plain_name_version() {
        assert_eq!(
            split_name_version("lodash@4.17.21"),
            Some(("lodash", "4.17.21"))
        );
    }

    #[test]
    fn scoped_name_version() {
        assert_eq!(
            split_name_version("@babel/core@7.26.0"),
            Some(("@babel/core", "7.26.0"))
        );
    }

    #[test]
    fn rejects_missing_version() {
        assert_eq!(split_name_version("lodash"), None);
        assert_eq!(split_name_version("@babel/core"), None);
    }

    #[test]
    fn rejects_empty_version_after_at() {
        // Trailing-`@` typos would otherwise reach the store with an
        // empty version string and surface a misleading "cache cold"
        // error instead of the format hint.
        assert_eq!(split_name_version("lodash@"), None);
        assert_eq!(split_name_version("@babel/core@"), None);
    }
}
