//! `aube cat-index <pkg@version>` — print the cached package index JSON.
//!
//! Prints the index that `aube fetch`/`aube install` writes under
//! `~/.cache/aube/index/`. Integrity-keyed entries live under
//! `<16 hex>/<name>@<version>.json` (subdirectory keyed by the
//! tarball's SHA-512 prefix); integrity-less entries live at
//! `<name>@<version>.json` in the root. The filename alone never
//! carries discriminating data, so a version with semver build
//! metadata can't collide with an integrity-keyed entry.
//!
//! The package must have been fetched by aube at least once — if the cache
//! is cold for that version, we surface a friendly error pointing at
//! `aube fetch`. This is a read-only introspection command: no lockfile,
//! no node_modules, no project lock.
//!
//! If the user has fetched multiple distinct tarballs under the same
//! `(name, version)` — e.g. a github codeload archive and the
//! npm-published bytes — the command lists every cached integrity and
//! asks the user to disambiguate.

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

    // Validate through the same grammar `Store::save_index` enforces
    // so a user passing `aube cat-index ../../evil 1.0.0` gets a clear
    // refusal instead of a surprising path outside the cache.
    let _safe_name = aube_store::validate_and_encode_name(name)
        .ok_or_else(|| miette!("invalid package name: {name:?}"))?;
    if !aube_store::validate_version(version) {
        return Err(miette!("invalid version: {version:?}"));
    }
    let matches: Vec<_> = store
        .cached_indices_unverified()
        .map_err(|e| miette!("failed to read cached indices: {e}"))?
        .into_iter()
        .filter(|entry| entry.name == name && entry.version == version)
        .collect();
    let index = match matches.as_slice() {
        [] => {
            return Err(miette!(
                "no cached index for {name}@{version}\nhelp: run `aube fetch` or `aube install` to populate the store first"
            ));
        }
        [entry] => &entry.index,
        many => {
            let mut msg = format!(
                "{} distinct cached tarballs for {name}@{version}:\n",
                many.len()
            );
            for entry in many {
                let integrity = entry.integrity.as_deref().unwrap_or("<none>");
                msg.push_str(&format!("- integrity: {integrity}\n"));
            }
            msg.push_str("help: re-run `aube fetch` in the project whose tarball you want.");
            return Err(miette!("{msg}"));
        }
    };

    let json = serde_json::to_string_pretty(&index)
        .into_diagnostic()
        .map_err(|e| miette!("failed to serialize index: {e}"))?;
    println!("{json}");

    Ok(())
}

/// Find every cached index whose filename matches `filename`. Looks
/// at the index-root for the integrity-less entry, then peeks into
/// each `<16 hex>/` subdir for the integrity-keyed variants. Returns
/// the discovered paths sorted for stable output.
#[cfg(test)]
fn scan_matches(
    index_dir: &std::path::Path,
    filename: &str,
) -> miette::Result<Vec<std::path::PathBuf>> {
    let mut matches = Vec::new();

    // Integrity-less entry (no `dist.integrity` on the tarball).
    let plain = index_dir.join(filename);
    if plain.is_file() {
        matches.push(plain);
    }

    // Integrity-keyed entries under `<16 hex>/` subdirs.
    let entries = match std::fs::read_dir(index_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(miette!("failed to read {}: {e}", index_dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(filename);
        if candidate.is_file() {
            matches.push(candidate);
        }
    }
    matches.sort();
    Ok(matches)
}

/// Split `name@version` into its parts, respecting scoped packages.
/// Returns `None` if no `@version` is present, or if the version half
/// is empty (`lodash@`, `@babel/core@`) — cat-index needs an exact
/// version, so both bare names and trailing-`@` typos are rejected up
/// front so the user gets the format hint instead of the misleading
/// "cache cold, run aube fetch" error.
fn split_name_version(input: &str) -> Option<(&str, &str)> {
    let (name, version) = aube_util::pkg::split_name_spec(input);
    let version = version?;
    if version.is_empty() {
        return None;
    }
    Some((name, version))
}

#[cfg(test)]
mod tests {
    use super::{scan_matches, split_name_version};

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

    #[test]
    fn scan_matches_finds_integrity_less_entry_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("pkg@1.0.0.json"), "{}").unwrap();

        let matches = scan_matches(dir, "pkg@1.0.0.json").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file_name().unwrap(), "pkg@1.0.0.json");
    }

    #[test]
    fn scan_matches_finds_integrity_keyed_entry_in_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let subdir = dir.join("aabbccddeeff0011");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("pkg@1.0.0.json"), "{}").unwrap();

        let matches = scan_matches(dir, "pkg@1.0.0.json").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].parent().unwrap().file_name().unwrap(),
            "aabbccddeeff0011"
        );
    }

    #[test]
    fn scan_matches_separates_integrity_from_build_metadata() {
        // Regression: the old flat layout with a `+<hex>` filename
        // suffix could conflate an integrity-keyed entry for
        // version `1.0.0` with an integrity-less entry for version
        // `1.0.0+build123`. The subdir layout forecloses that: the
        // build-metadata version lives at
        // `pkg@1.0.0+build123.json` (its own file, different stem),
        // while the integrity-keyed `pkg@1.0.0` lives at
        // `<16 hex>/pkg@1.0.0.json`.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let subdir = dir.join("deadbeefdeadbeef");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("pkg@1.0.0.json"), "{}").unwrap();
        std::fs::write(dir.join("pkg@1.0.0+build123.json"), "{}").unwrap();

        let matches = scan_matches(dir, "pkg@1.0.0.json").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].parent().unwrap().file_name().unwrap(),
            "deadbeefdeadbeef"
        );

        // Separately, the build-metadata version has its own distinct
        // filename and is discoverable under its own query.
        let bmeta = scan_matches(dir, "pkg@1.0.0+build123.json").unwrap();
        assert_eq!(bmeta.len(), 1);
        assert_eq!(bmeta[0].parent().unwrap(), dir);
    }

    #[test]
    fn scan_matches_returns_both_variants_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let subdir = dir.join("1122334455667788");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("pkg@1.0.0.json"), "{}").unwrap();
        std::fs::write(dir.join("pkg@1.0.0.json"), "{}").unwrap();

        let matches = scan_matches(dir, "pkg@1.0.0.json").unwrap();
        assert_eq!(matches.len(), 2);
    }
}
