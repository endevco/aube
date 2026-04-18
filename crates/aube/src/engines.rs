//! `engines` field validation.
//!
//! Checks each package's declared `engines.node` constraint against the
//! Node version the current install is running under. Mismatches are
//! surfaced as warnings by default; when `engine-strict` is set in
//! `.npmrc` (or on the root package.json), they hard-fail the install.
//!
//! Non-node engines (`npm`, `pnpm`, `yarn`, `vscode`, etc.) are ignored —
//! only the `node` key is checked, matching the field most users set.

use aube_lockfile::LockfileGraph;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use aube_store::PackageIndex;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

/// Outcome of checking a single package's `engines.node` against the
/// current Node version.
#[derive(Debug)]
pub struct Mismatch {
    pub package: String,
    pub declared: String,
    pub current: String,
}

/// Resolve the Node version to check against:
///
/// 1. `node-version` override from `.npmrc` if present;
/// 2. otherwise `node --version` (stripping the leading `v`);
/// 3. otherwise `None` (we silently skip the check — a user on a machine
///    without Node installed shouldn't be blocked from installing).
pub fn resolve_node_version(override_: Option<&str>) -> Option<String> {
    if let Some(v) = override_ {
        return Some(v.trim().trim_start_matches('v').to_string());
    }
    // Memoize the `node --version` probe. Spawning a process is cheap
    // individually but this is called once per install and may end up
    // called again by future callers in the same process (workspace
    // installs, `aube add` chaining into `install`, tests). OnceLock
    // gives us zero-cost lookups after the first probe.
    static PROBED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PROBED.get_or_init(probe_node_version).clone()
}

fn probe_node_version() -> Option<String> {
    let output = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    Some(s.trim().trim_start_matches('v').to_string())
}

/// Test whether `version` satisfies `range`. A version or range we can't
/// parse is treated as "no opinion" (returns `true`) — matches pnpm's
/// leniency and avoids failing installs over malformed `engines.node`
/// fields or unusual Node builds that report non-standard version strings
/// (e.g. nightly builds, custom forks).
fn node_range_satisfied(version: &str, range: &str) -> bool {
    let Ok(v) = node_semver::Version::parse(version) else {
        return true;
    };
    let Ok(r) = node_semver::Range::parse(range) else {
        return true;
    };
    v.satisfies(&r)
}

/// Check a single `engines` map. Returns `Some(declared_range)` on
/// mismatch, `None` otherwise.
fn check_engines_node(engines: &BTreeMap<String, String>, node_version: &str) -> Option<String> {
    let node_range = engines.get("node")?;
    if node_range_satisfied(node_version, node_range) {
        None
    } else {
        Some(node_range.clone())
    }
}

/// Read each locked package's `package.json` and collect any
/// `engines.node` mismatches. Runs in parallel via rayon — each
/// read is small and independent.
///
/// Reads `package.json` from the materialized location at
/// `node_modules/.aube/<escaped dep_path>/node_modules/<name>/package.json`,
/// not from `indices[dep_path]`. The fetch phase's `AlreadyLinked`
/// fast path skips `load_index` for packages whose virtual-store
/// entry already exists (which is every package on a warm
/// re-install), so the `indices` map is sparse and a lookup-through
/// pattern would silently drop every already-linked package. That's
/// dangerous for the engine-strict use case: switching Node
/// versions (e.g. via nvm) and running `aube install` would
/// *appear* to succeed while missing every `engines.node` check
/// except the root. Reading the hardlinked file avoids that trap —
/// same bytes the CAS would point us at, with zero dependency on
/// the sparse indices map.
///
/// `indices` is still plumbed through because the CAS-pathed read
/// is a viable fallback for packages whose virtual-store entry is
/// missing or in the middle of being materialized (e.g. under
/// `aube install --lockfile-only`, where the linker never runs).
///
/// Error policy: `NotFound` on the materialized read falls through
/// to the CAS fallback; `NotFound` on the CAS fallback means we
/// have no `package.json` to check and we skip the dep. Any other
/// I/O error (permission denied, disk corruption, partial read) on
/// **either** path propagates as `miette::Error` so the user sees
/// the real problem — swallowing it would silently turn
/// `engine-strict` into a no-op on the affected package, which is
/// exactly the trap `run_dep_lifecycle_scripts` and
/// `read_materialized_pkg_json` already closed elsewhere in the
/// PR.
pub fn collect_dep_mismatches(
    aube_dir: &Path,
    graph: &LockfileGraph,
    indices: &BTreeMap<String, PackageIndex>,
    node_version: &str,
    virtual_store_dir_max_length: usize,
) -> miette::Result<Vec<Mismatch>> {
    use miette::miette;

    // Rayon: collect into `Result<Vec<Option<Mismatch>>>` so the
    // first real I/O error short-circuits the whole scan. The
    // `Option` captures "checked cleanly, no mismatch" vs "no
    // package.json available at all (skipped)".
    let per_pkg: miette::Result<Vec<Option<Mismatch>>> = graph
        .packages
        .par_iter()
        .map(|(dep_path, pkg)| -> miette::Result<Option<Mismatch>> {
            // Primary read path: materialized `package.json`. The
            // `virtual_store_dir_max_length` must match the value
            // the linker was built with — see `install::run` for
            // the single source of truth — or long `dep_path`s that
            // trip `dep_path_to_filename`'s truncate-and-hash
            // fallback will encode to a different filename than the
            // linker wrote and we'll silently miss the check.
            // `aube_dir` is the resolved `virtualStoreDir` — the
            // install driver threads it in via
            // `commands::resolve_virtual_store_dir` so custom
            // overrides land on the same path the linker wrote to.
            let materialized = aube_dir
                .join(dep_path_to_filename(dep_path, virtual_store_dir_max_length))
                .join("node_modules")
                .join(&pkg.name)
                .join("package.json");
            let content = match std::fs::read_to_string(&materialized) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Fallback: CAS read via the package index, which
                    // is still populated for packages whose
                    // virtual-store entry hadn't been created at
                    // fetch time (e.g. `--lockfile-only`).
                    let Some(stored) = indices
                        .get(dep_path)
                        .and_then(|idx| idx.get("package.json"))
                    else {
                        // No materialized file *and* no CAS entry —
                        // nothing to check against, skip the dep.
                        // This happens legitimately for `link:` deps
                        // and for packages that ship without a
                        // top-level `package.json`.
                        return Ok(None);
                    };
                    match std::fs::read_to_string(&stored.store_path) {
                        Ok(s) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                        Err(e) => {
                            return Err(miette!(
                                "failed to read CAS `package.json` for {}@{} at {}: {e}",
                                pkg.name,
                                pkg.version,
                                stored.store_path.display()
                            ));
                        }
                    }
                }
                Err(e) => {
                    return Err(miette!(
                        "failed to read materialized `package.json` for {}@{} at {}: {e}",
                        pkg.name,
                        pkg.version,
                        materialized.display()
                    ));
                }
            };
            // Parse errors propagate. A truncated or corrupted
            // `package.json` is the kind of thing a user genuinely
            // wants to see, not an excuse to skip the check.
            let parsed: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
                miette!(
                    "failed to parse `package.json` for {}@{}: {e}",
                    pkg.name,
                    pkg.version
                )
            })?;
            let Some(engines) = parsed.get("engines").and_then(|v| v.as_object()) else {
                return Ok(None);
            };
            let Some(node_range) = engines.get("node").and_then(|v| v.as_str()) else {
                return Ok(None);
            };
            if node_range_satisfied(node_version, node_range) {
                Ok(None)
            } else {
                Ok(Some(Mismatch {
                    package: format!("{}@{}", pkg.name, pkg.version),
                    declared: node_range.to_string(),
                    current: node_version.to_string(),
                }))
            }
        })
        .collect();

    Ok(per_pkg?.into_iter().flatten().collect())
}

/// Check the root manifest's `engines.node` constraint. Returns a
/// mismatch labeled with the project name (or "(root)" when unnamed).
pub fn check_root(manifest: &aube_manifest::PackageJson, node_version: &str) -> Option<Mismatch> {
    let declared = check_engines_node(&manifest.engines, node_version)?;
    Some(Mismatch {
        package: manifest
            .name
            .clone()
            .unwrap_or_else(|| "(root)".to_string()),
        declared,
        current: node_version.to_string(),
    })
}

/// Run the full engines check and either emit warnings or hard-fail the
/// install, depending on `strict`. A `None` `node_version` (e.g. no node
/// binary on PATH) short-circuits — nothing to check against.
#[allow(clippy::too_many_arguments)]
pub fn run_checks(
    aube_dir: &Path,
    manifest: &aube_manifest::PackageJson,
    graph: &LockfileGraph,
    indices: &BTreeMap<String, PackageIndex>,
    node_version: Option<&str>,
    strict: bool,
    virtual_store_dir_max_length: usize,
) -> miette::Result<()> {
    let Some(node_version) = node_version else {
        return Ok(());
    };

    let mut mismatches = Vec::new();
    if let Some(m) = check_root(manifest, node_version) {
        mismatches.push(m);
    }
    mismatches.extend(collect_dep_mismatches(
        aube_dir,
        graph,
        indices,
        node_version,
        virtual_store_dir_max_length,
    )?);

    if mismatches.is_empty() {
        return Ok(());
    }

    let header = if strict {
        "Unsupported engine (engine-strict is on)"
    } else {
        "Unsupported engine"
    };
    eprintln!("warn: {header}");
    for m in &mismatches {
        eprintln!(
            "warn:   {}: wanted node {}, got {}",
            m.package, m.declared, m.current,
        );
    }

    if strict {
        return Err(miette::miette!(
            "engine-strict: {} package(s) require a Node version \
             incompatible with {}",
            mismatches.len(),
            node_version,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_satisfied_basic() {
        assert!(node_range_satisfied("18.0.0", ">=16"));
        assert!(!node_range_satisfied("14.0.0", ">=16"));
    }

    #[test]
    fn unparseable_range_is_permissive() {
        // Some real packages ship nonsense here; we don't want to block on it.
        assert!(node_range_satisfied("18.0.0", "this-is-not-a-range"));
    }

    #[test]
    fn check_root_skips_when_no_engines() {
        let m = aube_manifest::PackageJson {
            name: Some("x".into()),
            version: None,
            dependencies: Default::default(),
            dev_dependencies: Default::default(),
            peer_dependencies: Default::default(),
            optional_dependencies: Default::default(),
            update_config: None,
            scripts: Default::default(),
            engines: Default::default(),
            workspaces: None,
            bundled_dependencies: None,
            extra: Default::default(),
        };
        assert!(check_root(&m, "18.0.0").is_none());
    }

    #[test]
    fn check_root_flags_mismatch() {
        let mut engines = BTreeMap::new();
        engines.insert("node".into(), ">=20".into());
        let m = aube_manifest::PackageJson {
            name: Some("x".into()),
            version: None,
            dependencies: Default::default(),
            dev_dependencies: Default::default(),
            peer_dependencies: Default::default(),
            optional_dependencies: Default::default(),
            update_config: None,
            scripts: Default::default(),
            engines,
            workspaces: None,
            bundled_dependencies: None,
            extra: Default::default(),
        };
        assert!(check_root(&m, "18.0.0").is_some());
    }

    #[test]
    fn collect_dep_mismatches_reads_materialized_pkg_json() {
        // Regression: `collect_dep_mismatches` used to look up every
        // dep through `indices.get(dep_path)?` and silently skip on
        // miss. Since `fetch_packages_with_root`'s `AlreadyLinked`
        // fast path omits entries from `package_indices` for every
        // warmly-installed package, that swallowed every engine check
        // on a warm re-install — so switching Node versions (nvm,
        // asdf, mise) and re-running `aube install --engine-strict`
        // would silently pass.
        //
        // The fix is to read each dep's `package.json` from its
        // materialized `.aube/<escaped>/node_modules/<name>/` path
        // first, and only fall back to the CAS via `indices` for
        // packages that aren't linked on disk yet. This test sets up
        // a materialized `package.json` with `engines.node: ">=99"`,
        // passes an **empty** indices map, and asserts the mismatch
        // is still found.
        use aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;
        use aube_lockfile::{DepType, DirectDep, LockedPackage};
        use std::collections::BTreeMap;

        let tmp = tempfile::tempdir().unwrap();
        let dep_path = "pkg@1.0.0";
        let pkg_dir = tmp
            .path()
            .join("node_modules/.aube")
            .join(dep_path_to_filename(
                dep_path,
                DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            ))
            .join("node_modules/pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"pkg","version":"1.0.0","engines":{"node":">=99"}}"#,
        )
        .unwrap();

        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.into(),
            LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "pkg".into(),
                dep_path: dep_path.into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        // Empty indices — the warm-install case after the
        // `AlreadyLinked` fast path omits everything.
        let indices: BTreeMap<String, PackageIndex> = BTreeMap::new();
        let aube_dir = tmp.path().join("node_modules/.aube");
        let mismatches = collect_dep_mismatches(
            &aube_dir,
            &graph,
            &indices,
            "18.0.0",
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        )
        .unwrap();
        assert_eq!(mismatches.len(), 1, "engine mismatch should be surfaced");
        assert_eq!(mismatches[0].package, "pkg@1.0.0");
        assert_eq!(mismatches[0].declared, ">=99");
    }

    // Guard against regressing the error-propagation fix. Unix-only
    // because the test uses `chmod 000` to trigger a permission-denied
    // read, which has no direct Windows equivalent.
    #[cfg(unix)]
    #[test]
    fn collect_dep_mismatches_propagates_non_not_found_io_errors() {
        // Regression: an earlier version of `collect_dep_mismatches`
        // had two match arms with identical bodies — one guarded on
        // `ErrorKind::NotFound`, the fallthrough arm catching every
        // other I/O error — and both silently fell through to a
        // CAS-pathed `.ok()?` that also swallowed errors. The effect
        // was that a real I/O failure on any dep's `package.json`
        // (permission denied, disk corruption, short read) got
        // dropped on the floor and the engine check became a no-op
        // for that package. Under `--engine-strict` this could have
        // let an incompatible Node version through undetected. The
        // fix returns `miette::Result<..>` and propagates any
        // non-`NotFound` I/O error on either read path.
        use aube_lockfile::dep_path_filename::DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH;
        use aube_lockfile::{DepType, DirectDep, LockedPackage};
        use std::collections::BTreeMap;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let dep_path = "pkg@1.0.0";
        let pkg_dir = tmp
            .path()
            .join("node_modules/.aube")
            .join(dep_path_to_filename(
                dep_path,
                DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
            ))
            .join("node_modules/pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        let pkg_json = pkg_dir.join("package.json");
        std::fs::write(&pkg_json, r#"{"name":"pkg","version":"1.0.0"}"#).unwrap();
        // Make the file unreadable. `read_to_string` will return
        // `ErrorKind::PermissionDenied`, which is *not* NotFound and
        // must propagate.
        let mut perms = std::fs::metadata(&pkg_json).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&pkg_json, perms).unwrap();

        let mut graph = LockfileGraph::default();
        graph.packages.insert(
            dep_path.into(),
            LockedPackage {
                name: "pkg".into(),
                version: "1.0.0".into(),
                ..Default::default()
            },
        );
        graph.importers.insert(
            ".".into(),
            vec![DirectDep {
                name: "pkg".into(),
                dep_path: dep_path.into(),
                dep_type: DepType::Production,
                specifier: None,
            }],
        );

        let indices: BTreeMap<String, PackageIndex> = BTreeMap::new();
        let aube_dir = tmp.path().join("node_modules/.aube");
        let result = collect_dep_mismatches(
            &aube_dir,
            &graph,
            &indices,
            "18.0.0",
            DEFAULT_VIRTUAL_STORE_DIR_MAX_LENGTH,
        );
        // Restore perms so the tempdir can clean up cleanly.
        let mut perms = std::fs::metadata(&pkg_json).unwrap().permissions();
        perms.set_mode(0o644);
        let _ = std::fs::set_permissions(&pkg_json, perms);

        // Skip under root: mode bits don't constrain reads for uid
        // 0, so the test can't observe the permission-denied path.
        // SAFETY: libc::geteuid is a simple syscall with no
        // preconditions or thread-safety concerns.
        let is_root = unsafe { libc::geteuid() } == 0;
        if is_root {
            eprintln!("skipping permission-error test under root");
            return;
        }
        assert!(
            result.is_err(),
            "non-NotFound I/O error must propagate, got {result:?}"
        );
    }

    #[test]
    fn resolve_node_version_strips_v_prefix() {
        assert_eq!(
            resolve_node_version(Some("v18.17.1")).as_deref(),
            Some("18.17.1")
        );
        assert_eq!(
            resolve_node_version(Some("20.0.0")).as_deref(),
            Some("20.0.0")
        );
    }
}
