//! `aube find-hash <hash>` — reverse lookup from a file hash to the
//! `<name>@<version>` packages whose index references it.
//!
//! Walks `~/.cache/aube/index/*.json`, parses each cached `PackageIndex`,
//! and prints every `<name>@<version>` whose index contains the given
//! hash along with the relative path the hash lives at. Accepts both
//! `sha512-<base64>` integrity strings and raw hex CAS digests;
//! the integrity form is normalized to hex before comparison.
//!
//! Complement to `cat-file` / `cat-index`: once you have a store file
//! open via `cat-file <hash>`, `find-hash <hash>` tells you which
//! package(s) shipped it. Helpful for debugging linker behavior,
//! content-addressable dedup, and "who owns this random file".
//!
//! This is a read-only introspection command — no project lock, no
//! lockfile, no node_modules.

use clap::Args;
use miette::{IntoDiagnostic, miette};
use std::collections::BTreeMap;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  # Accepts integrity strings
  $ aube find-hash sha512-abc123...
  lodash@4.17.21\tpackage/lodash.js
  express@4.19.2\tnode_modules/lodash/lodash.js

  # ...or raw hex digests
  $ aube find-hash 5d41402abc4b2a76b9719d911017c592...

  # Machine-readable
  $ aube find-hash --json sha512-abc123...
";

#[derive(Debug, Args)]
pub struct FindHashArgs {
    /// Hash to look up.
    ///
    /// Accepts `sha512-<base64>` (pnpm integrity format) or a raw hex
    /// CAS digest.
    pub hash: String,

    /// Emit a machine-readable JSON array of
    /// `{ "name", "version", "path" }` objects instead of a plain text
    /// listing.
    #[arg(long)]
    pub json: bool,
}

/// A single match: one package's index referenced the queried hash via
/// the given in-package relative path.
#[derive(Debug, serde::Serialize)]
struct Match {
    name: String,
    version: String,
    path: String,
}

pub async fn run(args: FindHashArgs) -> miette::Result<()> {
    // Normalize input: integrity → hex, or validate raw hex up front so
    // we don't compare nonsense strings against every cached index.
    let target_hex = if args.hash.starts_with("sha512-") {
        aube_store::integrity_to_hex(&args.hash)
            .ok_or_else(|| miette!("invalid integrity hash: {}", args.hash))?
    } else {
        if args.hash.is_empty() || !args.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(miette!(
                "invalid hash: {}\nhelp: expected a `sha512-<base64>` integrity string or a hex CAS digest",
                args.hash
            ));
        }
        args.hash.to_ascii_lowercase()
    };

    let cwd = crate::dirs::project_root_or_cwd()?;
    let store = crate::commands::open_store(&cwd)?;

    let index_dir = store.index_dir();
    if !index_dir.exists() {
        return Err(miette!(
            "index cache is empty at {}\nhelp: run `aube install` or `aube fetch` first to populate the store",
            index_dir.display()
        ));
    }

    let matches = scan_index_dir(&index_dir, &target_hex)?;

    // Always print the output before deciding the exit status, so scripts
    // that pipe `find-hash --json` into `jq` still get parseable output
    // (an empty `[]` array) on a miss — but the exit code is consistent
    // between text and JSON modes: non-zero when nothing matched.
    if args.json {
        let json = serde_json::to_string_pretty(&matches)
            .into_diagnostic()
            .map_err(|e| miette!("failed to serialize matches: {e}"))?;
        println!("{json}");
    } else {
        for m in &matches {
            println!("{}@{}\t{}", m.name, m.version, m.path);
        }
    }

    if matches.is_empty() {
        return Err(miette!(
            "no package index references hash {}\nhelp: the file may belong to a package aube hasn't fetched yet, or the hash may be wrong",
            args.hash
        ));
    }

    Ok(())
}

/// Walk every `*.json` file under the cache dir, parse it as a
/// `PackageIndex`, and collect every entry whose `hex_hash` matches
/// `target_hex`. Cache entries that fail to parse or can't be decoded
/// into `{name}@{version}` are skipped silently — the cache is a
/// best-effort artifact, not a source of truth.
fn scan_index_dir(index_dir: &std::path::Path, target_hex: &str) -> miette::Result<Vec<Match>> {
    let mut matches: Vec<Match> = Vec::new();

    let entries = std::fs::read_dir(index_dir)
        .into_diagnostic()
        .map_err(|e| miette!("failed to read {}: {e}", index_dir.display()))?;

    for entry in entries {
        let entry = entry
            .into_diagnostic()
            .map_err(|e| miette!("failed to read directory entry: {e}"))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((name, version)) = split_stem(stem) else {
            continue;
        };

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(index): Result<BTreeMap<String, aube_store::StoredFile>, _> =
            serde_json::from_str(&content)
        else {
            continue;
        };

        for (rel_path, file) in index {
            if file.hex_hash == target_hex {
                matches.push(Match {
                    name: name.clone(),
                    version: version.clone(),
                    path: rel_path,
                });
            }
        }
    }

    matches.sort_by(|a, b| (&a.name, &a.version, &a.path).cmp(&(&b.name, &b.version, &b.path)));
    Ok(matches)
}

/// Reverse the `{safe_name}@{version}+{integrity_short}.json` naming
/// scheme used by `Store::save_index`. The safe-name rule is
/// `/` → `__`, which isn't globally bijective — a non-scoped package
/// whose name legitimately contains `__` (e.g. `foo__bar`) would
/// round-trip back as `foo/bar` under naive `replace("__", "/")`.
///
/// Exploit the structure of npm names: only scoped packages contain
/// `/`, and always exactly one — between the scope and the package
/// name. So:
///   - Non-scoped (no leading `@`): keep the safe name verbatim; the
///     `__` must be literal.
///   - Scoped (leading `@`): replace only the *first* `__` with `/`.
///
/// (A scoped package whose scope itself contains `__` — e.g.
/// `@foo__bar/baz` — is still ambiguous vs. `@foo/bar__baz` and
/// decodes to the latter. That's a limitation of the underlying
/// `save_index` encoding, not fixable in the reverse direction alone.)
///
/// The trailing `+{16 hex chars}` integrity suffix is stripped before
/// splitting on `@`, so the printed `{name}@{version}` output stays
/// stable across the cache-key change. Stems from older aube versions
/// (without a `+<hex>` suffix) still parse correctly.
fn split_stem(stem: &str) -> Option<(String, String)> {
    let stem = strip_integrity_suffix(stem);
    let at = stem.rfind('@')?;
    if at == 0 {
        return None;
    }
    let safe_name = &stem[..at];
    let version = &stem[at + 1..];
    if version.is_empty() {
        // A stem like `lodash@` (corrupt cache file `lodash@.json`)
        // would otherwise emit a `lodash@\tindex.js` line — drop it,
        // matching what `scan_index_dir`'s doc comment promises.
        return None;
    }
    let name = if let Some(rest) = safe_name.strip_prefix('@') {
        if let Some(sep) = rest.find("__") {
            format!("@{}/{}", &rest[..sep], &rest[sep + 2..])
        } else {
            safe_name.to_string()
        }
    } else {
        safe_name.to_string()
    };
    Some((name, version.to_string()))
}

/// Drop the trailing `+<16 lowercase hex>` integrity suffix if present.
/// Anything shorter, longer, or non-hex is left alone so old-format
/// stems (pre integrity-keyed cache) still parse.
fn strip_integrity_suffix(stem: &str) -> &str {
    let Some(plus) = stem.rfind('+') else {
        return stem;
    };
    let suffix = &stem[plus + 1..];
    if suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        &stem[..plus]
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_stem_plain() {
        assert_eq!(
            split_stem("lodash@4.17.21"),
            Some(("lodash".into(), "4.17.21".into()))
        );
    }

    #[test]
    fn split_stem_scoped() {
        // `@babel/core@7.0.0` is saved as `@babel__core@7.0.0.json`;
        // split_stem receives the stem without the .json.
        assert_eq!(
            split_stem("@babel__core@7.0.0"),
            Some(("@babel/core".into(), "7.0.0".into()))
        );
    }

    #[test]
    fn split_stem_rejects_bare_scope_name() {
        // `@scope` on its own (no / or version) is never a real cache
        // entry and would confuse `rfind('@')`.
        assert_eq!(split_stem("@scope"), None);
    }

    #[test]
    fn split_stem_preserves_double_underscore_in_unscoped_name() {
        // Regression: naive `replace("__", "/")` would corrupt this to
        // `foo/bar`. Non-scoped names can contain `__` legitimately.
        assert_eq!(
            split_stem("foo__bar@1.0.0"),
            Some(("foo__bar".into(), "1.0.0".into()))
        );
    }

    #[test]
    fn split_stem_rejects_empty_version() {
        // Corrupt cache file like `lodash@.json` would otherwise emit
        // a match with an empty version field.
        assert_eq!(split_stem("lodash@"), None);
        assert_eq!(split_stem("@scope__pkg@"), None);
    }

    #[test]
    fn split_stem_scoped_with_trailing_double_underscore_in_pkg() {
        // `@scope/pkg__name@1.0.0` is saved as `@scope__pkg__name@...`
        // The first `__` is the scope/pkg separator; subsequent ones
        // are literal.
        assert_eq!(
            split_stem("@scope__pkg__name@1.0.0"),
            Some(("@scope/pkg__name".into(), "1.0.0".into()))
        );
    }

    #[test]
    fn split_stem_strips_integrity_suffix() {
        // New cache-key format appends `+<16 hex>` as an integrity
        // discriminator. `find_hash`'s output should stay
        // `{name}@{version}` regardless.
        assert_eq!(
            split_stem("lodash@4.17.21+deadbeefdeadbeef"),
            Some(("lodash".into(), "4.17.21".into()))
        );
        assert_eq!(
            split_stem("@babel__core@7.0.0+0123456789abcdef"),
            Some(("@babel/core".into(), "7.0.0".into()))
        );
    }

    #[test]
    fn split_stem_leaves_non_integrity_plus_suffix_intact() {
        // A semver build metadata suffix like `+build123` shouldn't be
        // stripped — only a precise 16-lowercase-hex tail is the
        // integrity marker. `build123` has fewer than 16 chars and is
        // not hex, so it stays as part of the version.
        assert_eq!(
            split_stem("foo@1.0.0+build123"),
            Some(("foo".into(), "1.0.0+build123".into()))
        );
    }

    #[test]
    fn scan_index_dir_finds_a_match() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let index = serde_json::json!({
            "index.js": {
                "hex_hash": "deadbeef",
                "store_path": "/tmp/foo",
                "executable": false,
            },
            "package.json": {
                "hex_hash": "cafefeed",
                "store_path": "/tmp/bar",
                "executable": false,
            }
        });
        std::fs::write(
            dir.join("lodash@4.17.21.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();

        let matches = scan_index_dir(dir, "deadbeef").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "lodash");
        assert_eq!(matches[0].version, "4.17.21");
        assert_eq!(matches[0].path, "index.js");
    }

    #[test]
    fn scan_index_dir_matches_across_files_and_sorts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        let shared_hash = "deadbeef";
        let a_index = serde_json::json!({
            "a.js": { "hex_hash": shared_hash, "store_path": "/tmp/a", "executable": false }
        });
        let b_index = serde_json::json!({
            "b.js": { "hex_hash": shared_hash, "store_path": "/tmp/b", "executable": false }
        });
        std::fs::write(
            dir.join("b-pkg@1.0.0.json"),
            serde_json::to_string(&a_index).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("a-pkg@1.0.0.json"),
            serde_json::to_string(&b_index).unwrap(),
        )
        .unwrap();

        let matches = scan_index_dir(dir, shared_hash).unwrap();
        assert_eq!(matches.len(), 2);
        // Sorted alphabetically by name.
        assert_eq!(matches[0].name, "a-pkg");
        assert_eq!(matches[1].name, "b-pkg");
    }

    #[test]
    fn scan_index_dir_ignores_non_json_and_garbage() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        std::fs::write(dir.join("README"), "not json").unwrap();
        std::fs::write(dir.join("broken@1.0.0.json"), "{ not valid json").unwrap();

        let matches = scan_index_dir(dir, "deadbeef").unwrap();
        assert!(matches.is_empty());
    }
}
