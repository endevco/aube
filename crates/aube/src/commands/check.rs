//! `aube check` — verify `node_modules/` symlink tree integrity.
//!
//! Walks every package materialized under `node_modules/.aube/<cell>/node_modules/`,
//! reads its `package.json`, and confirms that every declared `dependencies`
//! entry has a corresponding sibling inside the same cell directory — the
//! shape Node's module resolver expects when walking up from a package's
//! location. Missing entries are reported as broken links.
//!
//! `peerDependencies` are out of scope — `aube peers check` validates
//! those against the lockfile.  `optionalDependencies` that the platform
//! filter legitimately skipped would look broken here, so we scope the
//! check to `dependencies` only. `devDependencies` don't ship inside
//! non-root packages' manifests, so they never appear in cell lookups.
//!
//! Exits with status 1 when at least one broken link is found, so it's
//! CI-friendly as a post-install gate.

use clap::Args;
use miette::IntoDiagnostic;
use std::collections::BTreeMap;
use std::path::Path;

pub const AFTER_LONG_HELP: &str = "\
Examples:

  $ aube check
  node_modules symlink tree is consistent (checked 248 packages).

  # With issues
  $ aube check
  2 broken dependency links found:

    vscode-languageserver@9.0.1
      ✕ cannot resolve: vscode-languageserver-protocol@3.17.5

    vscode-languageserver-protocol@3.17.5
      ✕ cannot resolve: vscode-languageserver-types@3.17.5
      ✕ cannot resolve: vscode-jsonrpc@8.2.1

  # Machine-readable
  $ aube check --json
";

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Emit a JSON report instead of the human-readable list.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(args: CheckArgs) -> miette::Result<()> {
    let cwd = crate::dirs::project_root()?;
    let report = run_report(&cwd)?;

    if args.json {
        print_json(&report);
    } else {
        print_human(&report);
    }

    if !report.issues.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Result of scanning the virtual store.
#[derive(Debug, Default)]
pub(crate) struct CheckReport {
    /// Number of package manifests we successfully inspected.
    pub(crate) checked: usize,
    /// Broken dependency links, stable-sorted.
    pub(crate) issues: Vec<BrokenLink>,
}

#[derive(Debug, Clone)]
pub(crate) struct BrokenLink {
    pub(crate) consumer_name: String,
    pub(crate) consumer_version: String,
    pub(crate) dep_name: String,
    pub(crate) dep_range: String,
}

/// Walk the virtual store under `cwd` and collect broken dependency links.
///
/// Reusable from `aube doctor` — pass `cwd` = project root. Returns an
/// empty report (0 checked, no issues) if the virtual store doesn't
/// exist yet (never installed, or hoisted layout without an isolated
/// tree); callers that want to treat that as an error do so themselves.
pub(crate) fn run_report(cwd: &Path) -> miette::Result<CheckReport> {
    let aube_dir = super::resolve_virtual_store_dir_for_cwd(cwd);
    let mut report = CheckReport::default();

    let Ok(cells) = std::fs::read_dir(&aube_dir) else {
        return Ok(report);
    };

    for entry in cells.flatten() {
        let cell_path = entry.path();
        if !cell_path.is_dir() {
            continue;
        }
        let cell_nm = cell_path.join("node_modules");
        if !cell_nm.is_dir() {
            continue;
        }
        scan_cell(&cell_nm, &mut report)?;
    }

    report.issues.sort_by(|a, b| {
        (&a.consumer_name, &a.consumer_version, &a.dep_name).cmp(&(
            &b.consumer_name,
            &b.consumer_version,
            &b.dep_name,
        ))
    });

    Ok(report)
}

/// Walk one `<cell>/node_modules/` directory. Each first-level entry is
/// either a package directory (`foo/`) or a scope directory (`@scope/`)
/// containing scoped packages. Entries that are symlinks to sibling
/// deps are skipped — we only audit the manifests that actually live
/// in this cell (i.e. are real directories).
fn scan_cell(cell_nm: &Path, report: &mut CheckReport) -> miette::Result<()> {
    for entry in std::fs::read_dir(cell_nm).into_diagnostic()?.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        if let Some(scope) = name_str.strip_prefix('@') {
            let Ok(inner) = std::fs::read_dir(&path) else {
                continue;
            };
            for scoped in inner.flatten() {
                let sp = scoped.path();
                if sp.is_symlink() || !sp.is_dir() {
                    continue;
                }
                let Some(pkg) = scoped.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                check_package(cell_nm, &sp, &format!("@{scope}/{pkg}"), report)?;
            }
        } else {
            check_package(cell_nm, &path, name_str, report)?;
        }
    }
    Ok(())
}

/// Inspect one package's `package.json` and check that each declared
/// dependency has a sibling in `cell_nm/`.
fn check_package(
    cell_nm: &Path,
    pkg_dir: &Path,
    pkg_name_from_path: &str,
    report: &mut CheckReport,
) -> miette::Result<()> {
    let manifest_path = pkg_dir.join("package.json");
    let Ok(manifest) = aube_manifest::PackageJson::from_path(&manifest_path) else {
        // Packages that fail to parse their own manifest are an
        // install-layer problem, not a link-tree problem — skip and
        // let `aube install` surface the real error.
        return Ok(());
    };

    report.checked += 1;

    let consumer_name = manifest
        .name
        .clone()
        .unwrap_or_else(|| pkg_name_from_path.to_string());
    let consumer_version = manifest.version.clone().unwrap_or_default();

    let bundled = manifest
        .bundled_dependencies
        .as_ref()
        .map(|b| {
            b.names(&manifest.dependencies)
                .into_iter()
                .map(String::from)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();

    for (dep_name, dep_range) in &manifest.dependencies {
        if bundled.contains(dep_name) {
            // Bundled deps ship inside the tarball at `<pkg>/node_modules/<dep>`,
            // not as a sibling — resolve that way.
            if pkg_dir.join("node_modules").join(dep_name).exists() {
                continue;
            }
        }
        let sibling = cell_nm.join(dep_name);
        if sibling.exists() {
            continue;
        }
        report.issues.push(BrokenLink {
            consumer_name: consumer_name.clone(),
            consumer_version: consumer_version.clone(),
            dep_name: dep_name.clone(),
            dep_range: dep_range.clone(),
        });
    }

    Ok(())
}

fn print_human(report: &CheckReport) {
    if report.issues.is_empty() {
        println!(
            "node_modules symlink tree is consistent (checked {} {}).",
            report.checked,
            if report.checked == 1 {
                "package"
            } else {
                "packages"
            }
        );
        return;
    }

    let mut groups: BTreeMap<(String, String), Vec<&BrokenLink>> = BTreeMap::new();
    for i in &report.issues {
        groups
            .entry((i.consumer_name.clone(), i.consumer_version.clone()))
            .or_default()
            .push(i);
    }

    println!(
        "{} broken dependency {} found:",
        report.issues.len(),
        if report.issues.len() == 1 {
            "link"
        } else {
            "links"
        }
    );
    println!();

    for ((name, version), group) in &groups {
        if version.is_empty() {
            println!("  {name}");
        } else {
            println!("  {name}@{version}");
        }
        for link in group {
            println!("    ✕ cannot resolve: {}@{}", link.dep_name, link.dep_range);
        }
        println!();
    }
}

fn print_json(report: &CheckReport) {
    let mut arr = Vec::with_capacity(report.issues.len());
    for i in &report.issues {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "consumer".into(),
            format!("{}@{}", i.consumer_name, i.consumer_version).into(),
        );
        obj.insert("name".into(), i.dep_name.clone().into());
        obj.insert("range".into(), i.dep_range.clone().into());
        arr.push(serde_json::Value::Object(obj));
    }
    let mut root = serde_json::Map::new();
    root.insert("checked".into(), report.checked.into());
    root.insert("issues".into(), serde_json::Value::Array(arr));
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .unwrap_or_else(|_| "{}".to_string());
    println!("{json}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_pkg(dir: &Path, name: &str, version: &str, deps: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut deps_obj = serde_json::Map::new();
        for (n, v) in deps {
            deps_obj.insert(
                (*n).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        let mut root = serde_json::Map::new();
        root.insert("name".into(), name.into());
        root.insert("version".into(), version.into());
        if !deps_obj.is_empty() {
            root.insert("dependencies".into(), serde_json::Value::Object(deps_obj));
        }
        std::fs::write(
            dir.join("package.json"),
            serde_json::to_string_pretty(&serde_json::Value::Object(root)).unwrap(),
        )
        .unwrap();
    }

    fn symlink(from: &Path, to: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(from, to).unwrap();
        #[cfg(windows)]
        {
            // Tests currently skip Windows since the suite runs on unix CI.
            let _ = (from, to);
            panic!("windows path exercised test");
        }
    }

    /// Build a minimal `.aube/` tree with two cells, `foo@1.0.0` and
    /// `bar@2.0.0`. `foo` declares a dep on `bar`. Caller hooks up the
    /// sibling (or deliberately omits it).
    fn minimal_tree(with_link: bool) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(
            cwd.join("package.json"),
            r#"{"name":"root","version":"0.0.0"}"#,
        )
        .unwrap();

        let aube = cwd.join("node_modules").join(".aube");

        let foo_cell = aube.join("foo@1.0.0").join("node_modules");
        let foo_pkg = foo_cell.join("foo");
        write_pkg(&foo_pkg, "foo", "1.0.0", &[("bar", "^2.0.0")]);

        let bar_cell = aube.join("bar@2.0.0").join("node_modules");
        let bar_pkg = bar_cell.join("bar");
        write_pkg(&bar_pkg, "bar", "2.0.0", &[]);

        if with_link {
            symlink(&bar_pkg, &foo_cell.join("bar"));
        }

        (tmp, cwd)
    }

    #[test]
    fn consistent_tree_reports_zero_issues() {
        let (_tmp, cwd) = minimal_tree(true);
        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 2);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
    }

    #[test]
    fn missing_sibling_is_reported() {
        let (_tmp, cwd) = minimal_tree(false);
        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 2);
        assert_eq!(report.issues.len(), 1);
        let issue = &report.issues[0];
        assert_eq!(issue.consumer_name, "foo");
        assert_eq!(issue.consumer_version, "1.0.0");
        assert_eq!(issue.dep_name, "bar");
    }

    #[test]
    fn missing_virtual_store_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();
        let report = run_report(cwd).unwrap();
        assert_eq!(report.checked, 0);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn scoped_package_is_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        std::fs::write(cwd.join("package.json"), r#"{"name":"root"}"#).unwrap();

        let aube = cwd.join("node_modules").join(".aube");
        let cell = aube.join("@scope+foo@1.0.0").join("node_modules");
        let pkg = cell.join("@scope").join("foo");
        write_pkg(&pkg, "@scope/foo", "1.0.0", &[("@other/missing", "^1")]);

        let report = run_report(&cwd).unwrap();
        assert_eq!(report.checked, 1);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].consumer_name, "@scope/foo");
        assert_eq!(report.issues[0].dep_name, "@other/missing");
    }
}
