//! Workspace support for aube.
//!
//! Reads pnpm-workspace.yaml to discover workspace packages, falling back
//! to `package.json`'s `workspaces` field (yarn/npm/bun shape) when no
//! yaml is present. Supports the `workspace:` protocol for inter-package
//! dependencies.

pub mod selector;

use std::path::{Path, PathBuf};

pub use aube_manifest::workspace::WorkspaceConfig;
pub use selector::{Selector, WorkspacePkg};

/// Discover workspace package directories.
///
/// Precedence:
/// 1. `aube-workspace.yaml` / `pnpm-workspace.yaml` `packages:` (authoritative
///    when present — pnpm/aube projects keep yaml as source of truth).
/// 2. `package.json#workspaces` (yarn/npm/bun shape — array form or the
///    `{ packages: [...] }` object form).
pub fn find_workspace_packages(project_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let config = WorkspaceConfig::load(project_dir).map_err(|e| match e {
        aube_manifest::Error::Io(p, e) => Error::Io(p, e),
        aube_manifest::Error::YamlParse(p, e) => Error::Parse(p, e),
        aube_manifest::Error::Parse(pe) => Error::ParseDiag(pe),
    })?;

    let patterns: Vec<String> = if !config.packages.is_empty() {
        config.packages.clone()
    } else {
        package_json_workspace_patterns(project_dir)?
    };

    if patterns.is_empty() {
        return Ok(vec![]);
    }

    let mut neg_matchers = Vec::new();
    let mut positives = Vec::new();
    for raw in &patterns {
        if let Some(rest) = raw.strip_prefix('!') {
            let mk = |p: &str| {
                glob::Pattern::new(p).map_err(|e| {
                    Error::Parse(project_dir.join("pnpm-workspace.yaml"), e.to_string())
                })
            };
            // pnpm uses micromatch where `**` matches zero-or-more
            // path components, so `!**/example/**` excludes the
            // directory `example` itself. The `glob` crate requires
            // `**` to consume at least one component, so emit a
            // companion matcher with the trailing `/**` stripped to
            // catch the directory itself in addition to its descendants.
            neg_matchers.push(mk(rest)?);
            if let Some(self_form) = rest.strip_suffix("/**") {
                neg_matchers.push(mk(self_form)?);
            }
        } else {
            positives.push(raw.as_str());
        }
    }

    // Overlapping patterns are valid and common — a project may list
    // both `packages/*` and a specific `packages/slack` entry, or mix
    // a glob with an explicit nested path (`packages/sdk/js`). Dedupe
    // so downstream consumers (linker importer iteration, bin wiring,
    // filter matching) see each workspace package exactly once;
    // otherwise `link_workspace` tries to symlink the same top-level
    // dep twice and blows up with EEXIST.
    let mut seen = std::collections::HashSet::new();
    let mut packages = Vec::new();
    for pattern in &positives {
        for pkg_dir in expand_workspace_pattern(project_dir, pattern)? {
            let rel = pkg_dir.strip_prefix(project_dir).unwrap_or(&pkg_dir);
            if neg_matchers.iter().any(|m| m.matches_path(rel)) {
                continue;
            }
            if seen.insert(pkg_dir.clone()) {
                packages.push(pkg_dir);
            }
        }
    }

    packages.sort_unstable();
    Ok(packages)
}

fn expand_workspace_pattern(project_dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, Error> {
    let matcher = glob::Pattern::new(pattern)
        .map_err(|e| Error::Parse(project_dir.join("pnpm-workspace.yaml"), e.to_string()))?;
    if !pattern.contains("**") {
        return Ok(glob_workspace_pattern(project_dir, pattern));
    }

    let mut packages = Vec::new();
    let mut stack = vec![workspace_pattern_root(project_dir, pattern)];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == "node_modules" {
                continue;
            }
            stack.push(path.clone());
            let Ok(rel_path) = path.strip_prefix(project_dir) else {
                continue;
            };
            if matcher.matches_path(rel_path) && path.join("package.json").is_file() {
                packages.push(path);
            }
        }
    }
    Ok(packages)
}

fn glob_workspace_pattern(project_dir: &Path, pattern: &str) -> Vec<PathBuf> {
    let full_pattern = project_dir.join(pattern).join("package.json");
    let mut packages = Vec::new();
    if let Ok(entries) = glob::glob(full_pattern.to_str().unwrap_or_default()) {
        for entry in entries.flatten() {
            if entry.components().any(|c| c.as_os_str() == "node_modules") {
                continue;
            }
            if let Some(parent) = entry.parent()
                && is_under_project(project_dir, parent)
            {
                packages.push(parent.to_path_buf());
            }
        }
    }
    packages
}

/// Check that `candidate` resolves underneath `project_dir`. Patterns
/// containing `..` (e.g. `../sibling`) lexically still start with the
/// project prefix but escape the root via parent components. pnpm
/// rejects these and so do we. Falls back to lexical compare when
/// canonicalization fails (permission error, mid-walk race) so a path
/// the glob already returned still gets a containment check.
fn is_under_project(project_dir: &Path, candidate: &Path) -> bool {
    if let (Ok(root), Ok(child)) = (project_dir.canonicalize(), candidate.canonicalize()) {
        return child.starts_with(root);
    }
    let no_parent_dir = candidate
        .components()
        .all(|c| !matches!(c, std::path::Component::ParentDir));
    no_parent_dir && candidate.starts_with(project_dir)
}

fn workspace_pattern_root(project_dir: &Path, pattern: &str) -> PathBuf {
    let wildcard_idx = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    let literal_prefix = &pattern[..wildcard_idx];
    let root = literal_prefix
        .trim_end_matches('/')
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    project_dir.join(root)
}

/// Read the `workspaces` field from `<project_dir>/package.json`. Returns
/// an empty vec if the file is missing or the field is absent — a bare
/// package.json without `workspaces` is a single-package project, not an
/// error. Parse errors propagate so typos surface instead of silently
/// yielding an empty workspace.
fn package_json_workspace_patterns(project_dir: &Path) -> Result<Vec<String>, Error> {
    let path = project_dir.join("package.json");
    if !path.is_file() {
        return Ok(vec![]);
    }
    let pkg = aube_manifest::PackageJson::from_path(&path).map_err(|e| match e {
        aube_manifest::Error::Io(p, e) => Error::Io(p, e),
        aube_manifest::Error::Parse(pe) => Error::ParseDiag(pe),
        aube_manifest::Error::YamlParse(p, e) => Error::Parse(p, e),
    })?;
    Ok(pkg
        .workspaces
        .as_ref()
        .map(|w| w.patterns().to_vec())
        .unwrap_or_default())
}

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
pub enum Error {
    #[error("I/O error at {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    #[diagnostic(code(ERR_AUBE_WORKSPACE_PARSE))]
    Parse(PathBuf, String),
    /// Parse failure that came in via `aube_manifest::Error::Parse` and
    /// still carries its `NamedSource` + `SourceSpan`. Forwarded via
    /// `#[diagnostic(transparent)]` so `miette`'s `fancy` handler draws
    /// a pointer at the offending byte of the offending `package.json`.
    #[error(transparent)]
    #[diagnostic(transparent)]
    ParseDiag(Box<aube_manifest::ParseError>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn names(packages: Vec<PathBuf>) -> BTreeSet<String> {
        packages
            .into_iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn finds_packages_from_pnpm_workspace_yaml() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/b/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "b"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn falls_back_to_package_json_workspaces_array() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["apps/*","packages/*"]}"#,
        );
        write(&dir.path().join("apps/example/package.json"), "{}");
        write(&dir.path().join("packages/ui/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["example", "ui"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn falls_back_to_package_json_workspaces_object() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":{"packages":["apps/*"]}}"#,
        );
        write(&dir.path().join("apps/example/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["example"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn yaml_wins_when_both_present() {
        // If pnpm-workspace.yaml defines packages, the fallback to
        // package.json#workspaces is not consulted — pnpm-style
        // projects treat yaml as source of truth.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'from-yaml/*'\n",
        );
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["from-json/*"]}"#,
        );
        write(&dir.path().join("from-yaml/y/package.json"), "{}");
        write(&dir.path().join("from-json/j/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(names(found), ["y"].iter().map(|s| s.to_string()).collect());
    }

    #[test]
    fn negation_patterns_exclude_matched_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!**/example/**'\n  - '!**/test/**'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/example/package.json"), "{}");
        write(&dir.path().join("packages/test/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_pattern_excludes_exact_path() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/legacy'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/legacy/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_excluding_everything_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!packages/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn negation_pattern_order_does_not_matter() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - '!**/example/**'\n  - 'packages/*'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/example/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["keep"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_filters_recursive_positive_glob() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/**'\n  - '!**/test/**'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/a/test/package.json"), "{}");
        write(&dir.path().join("packages/b/sub/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "sub"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn negation_does_not_falsely_match_underscore_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '!**/_'\n",
        );
        write(&dir.path().join("packages/keep/package.json"), "{}");
        write(&dir.path().join("packages/_/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        let kept = names(found);
        assert!(
            kept.contains("keep"),
            "underscore-targeted negation must not exclude unrelated dirs; got {kept:?}"
        );
        assert!(
            !kept.contains("_"),
            "literal-underscore directory must still be excluded; got {kept:?}"
        );
    }

    #[test]
    fn negation_with_invalid_glob_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n  - '![bad'\n",
        );
        let err = find_workspace_packages(dir.path()).unwrap_err();
        assert!(
            matches!(err, Error::Parse(_, _)),
            "expected Error::Parse, got {err:?}"
        );
    }

    #[test]
    fn missing_package_json_without_yaml_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn package_json_without_workspaces_field_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name":"solo"}"#);
        let found = find_workspace_packages(dir.path()).unwrap();
        assert!(found.is_empty());
    }

    /// Real projects (opencode, for one) list both a glob and an
    /// explicit nested path that the glob already matches. Without
    /// dedup, `link_workspace` later symlinks the same workspace dep
    /// twice into a downstream importer's `node_modules` and fails
    /// with EEXIST on the second write.
    #[test]
    fn overlapping_patterns_dedupe_matched_packages() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*","packages/slack","packages/sdk/js"]}"#,
        );
        write(&dir.path().join("packages/slack/package.json"), "{}");
        write(&dir.path().join("packages/sdk/js/package.json"), "{}");
        write(&dir.path().join("packages/other/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        // slack is matched by both `packages/*` and the explicit
        // `packages/slack`; must appear exactly once.
        let slack_count = found
            .iter()
            .filter(|p| p.ends_with("packages/slack"))
            .count();
        assert_eq!(slack_count, 1, "slack appeared {slack_count} times");
        // Nested-path entries (`packages/sdk/js`) that a simple
        // `packages/*` glob would NOT match still show up via the
        // explicit pattern.
        assert_eq!(
            names(found),
            ["js", "other", "slack"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
    }

    #[test]
    fn recursive_glob_skips_node_modules_subtrees() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/**/*'\n",
        );
        write(&dir.path().join("packages/a/package.json"), "{}");
        write(&dir.path().join("packages/nested/b/package.json"), "{}");
        write(
            &dir.path()
                .join("packages/a/node_modules/not-a-workspace/package.json"),
            "{}",
        );

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["a", "b"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn recursive_glob_with_mid_component_wildcard_uses_parent_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/prefix-*/**/*'\n",
        );
        write(&dir.path().join("packages/prefix-a/pkg/package.json"), "{}");
        write(
            &dir.path().join("packages/prefix-b/nested/app/package.json"),
            "{}",
        );
        write(&dir.path().join("packages/other/nope/package.json"), "{}");

        let found = find_workspace_packages(dir.path()).unwrap();
        assert_eq!(
            names(found),
            ["app", "pkg"].iter().map(|s| s.to_string()).collect()
        );
    }
}
