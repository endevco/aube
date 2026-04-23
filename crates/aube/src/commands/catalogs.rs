//! Catalog-aware helpers shared between `aube add` and `aube install`.
//!
//! Two settings land here:
//! - `catalogMode` governs how `add` writes the manifest specifier for a
//!   package that already appears in the default workspace catalog.
//! - `cleanupUnusedCatalogs` trims workspace-yaml catalog entries that
//!   no importer references, after a successful resolve.
//!
//! Both features share the same source of truth (`WorkspaceConfig::catalog`
//! / `catalogs` maps on disk), so the edit-the-yaml plumbing lives here
//! rather than in either command module.

use aube_settings::resolved::CatalogMode;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Outcome of matching an `aube add` spec against the default catalog.
#[derive(Debug)]
pub(crate) enum CatalogRewrite {
    /// Write the user's resolved specifier verbatim — either the mode is
    /// `manual`, the package isn't in the catalog, or `prefer` decided
    /// the user's range was incompatible.
    Manual,
    /// Rewrite the manifest entry to `catalog:` (always the default
    /// catalog — named catalogs require an explicit opt-in spec).
    UseDefaultCatalog,
    /// `strict` mode saw a spec that disagrees with the catalog entry.
    /// Propagate as a hard error.
    StrictMismatch {
        pkg: String,
        catalog_range: String,
        user_range: String,
    },
}

/// Decide whether an `add` specifier should be rewritten to a
/// `catalog:` reference. See `CatalogMode` docs in `settings.toml` for
/// the full semantics; this function only considers the *default*
/// catalog since named catalogs (`catalog:<name>`) always require an
/// explicit user opt-in.
pub(crate) fn decide_add_rewrite(
    mode: CatalogMode,
    default_catalog: Option<&BTreeMap<String, String>>,
    pkg_name: &str,
    user_range: &str,
    has_explicit_range: bool,
    resolved_version: &str,
    exclude_from_catalog: bool,
) -> CatalogRewrite {
    if exclude_from_catalog {
        return CatalogRewrite::Manual;
    }
    let Some(catalog) = default_catalog else {
        return CatalogRewrite::Manual;
    };
    let Some(catalog_range) = catalog.get(pkg_name) else {
        return CatalogRewrite::Manual;
    };
    match mode {
        CatalogMode::Manual => CatalogRewrite::Manual,
        CatalogMode::Prefer => {
            if range_compatible(
                user_range,
                has_explicit_range,
                catalog_range,
                resolved_version,
            ) {
                CatalogRewrite::UseDefaultCatalog
            } else {
                CatalogRewrite::Manual
            }
        }
        CatalogMode::Strict => {
            if !has_explicit_range
                || range_compatible(
                    user_range,
                    has_explicit_range,
                    catalog_range,
                    resolved_version,
                )
            {
                CatalogRewrite::UseDefaultCatalog
            } else {
                CatalogRewrite::StrictMismatch {
                    pkg: pkg_name.to_string(),
                    catalog_range: catalog_range.to_string(),
                    user_range: user_range.to_string(),
                }
            }
        }
    }
}

/// Treat the user's range as compatible with the catalog when it is
/// either (a) the exact same string — the common case for projects
/// that already standardized on catalog ranges — or (b) the catalog's
/// range would also accept the version we just resolved, so swapping
/// `catalog:` in won't silently install a different version.
fn range_compatible(
    user_range: &str,
    has_explicit_range: bool,
    catalog_range: &str,
    resolved_version: &str,
) -> bool {
    if !has_explicit_range {
        return true;
    }
    if user_range == catalog_range {
        return true;
    }
    let Ok(catalog_parsed) = node_semver::Range::parse(catalog_range) else {
        return false;
    };
    let Ok(version) = node_semver::Version::parse(resolved_version) else {
        return false;
    };
    version.satisfies(&catalog_parsed)
}

/// Locate the workspace yaml file (`aube-workspace.yaml` preferred,
/// `pnpm-workspace.yaml` as fallback) inside `project_dir`. Returns
/// `None` when neither file exists — callers treat that as "no catalogs
/// declared, nothing to clean up".
pub(crate) fn workspace_yaml_path(project_dir: &Path) -> Option<PathBuf> {
    for name in ["aube-workspace.yaml", "pnpm-workspace.yaml"] {
        let path = project_dir.join(name);
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// Remove catalog entries from the workspace yaml that the freshly
/// resolved graph didn't reference. Returns the list of `(catalog,
/// package)` pairs that were dropped so the caller can surface a
/// one-line summary.
///
/// The rewrite is a plain `serde_yaml` round trip: comments and custom
/// layout are not preserved. Callers gate this on `cleanupUnusedCatalogs`
/// so the destructive rewrite only happens when the user opted in.
pub(crate) fn prune_unused_catalog_entries(
    workspace_path: &Path,
    declared: &BTreeMap<String, BTreeMap<String, String>>,
    used: &BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>>,
) -> miette::Result<Vec<(String, String)>> {
    use miette::{Context, IntoDiagnostic};

    let mut unused: Vec<(String, String)> = Vec::new();
    for (cat_name, entries) in declared {
        for pkg in entries.keys() {
            let is_used = used
                .get(cat_name)
                .map(|u| u.contains_key(pkg))
                .unwrap_or(false);
            if !is_used {
                unused.push((cat_name.clone(), pkg.clone()));
            }
        }
    }
    if unused.is_empty() {
        return Ok(unused);
    }

    let text = std::fs::read_to_string(workspace_path)
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to read {} for cleanupUnusedCatalogs",
                workspace_path.display()
            )
        })?;
    let mut value: serde_yaml::Value = serde_yaml::from_str(&text)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", workspace_path.display()))?;

    // An empty file deserializes to `null`; nothing to strip.
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(root) = value.as_mapping_mut() else {
        return Ok(Vec::new());
    };

    for (cat_name, pkg_name) in &unused {
        if cat_name == "default" {
            if let Some(map) = root
                .get_mut(serde_yaml::Value::String("catalog".to_string()))
                .and_then(serde_yaml::Value::as_mapping_mut)
            {
                map.remove(serde_yaml::Value::String(pkg_name.clone()));
            }
        } else if let Some(catalogs) = root
            .get_mut(serde_yaml::Value::String("catalogs".to_string()))
            .and_then(serde_yaml::Value::as_mapping_mut)
            && let Some(map) = catalogs
                .get_mut(serde_yaml::Value::String(cat_name.clone()))
                .and_then(serde_yaml::Value::as_mapping_mut)
        {
            map.remove(serde_yaml::Value::String(pkg_name.clone()));
        }
    }

    // Drop now-empty catalog containers so the file doesn't grow
    // meaningless `catalog: {}` / `catalogs:` headers.
    let empty_default = root
        .get(serde_yaml::Value::String("catalog".to_string()))
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(serde_yaml::Mapping::is_empty);
    if empty_default {
        root.remove(serde_yaml::Value::String("catalog".to_string()));
    }
    if let Some(catalogs) = root
        .get_mut(serde_yaml::Value::String("catalogs".to_string()))
        .and_then(serde_yaml::Value::as_mapping_mut)
    {
        let to_drop: Vec<serde_yaml::Value> = catalogs
            .iter()
            .filter_map(|(k, v)| match v.as_mapping() {
                Some(m) if m.is_empty() => Some(k.clone()),
                _ => None,
            })
            .collect();
        for key in to_drop {
            catalogs.remove(key);
        }
    }
    let empty_named = root
        .get(serde_yaml::Value::String("catalogs".to_string()))
        .and_then(serde_yaml::Value::as_mapping)
        .is_some_and(serde_yaml::Mapping::is_empty);
    if empty_named {
        root.remove(serde_yaml::Value::String("catalogs".to_string()));
    }

    let new_text = serde_yaml::to_string(&value)
        .into_diagnostic()
        .wrap_err("failed to serialize workspace yaml after cleanupUnusedCatalogs")?;
    aube_util::fs_atomic::atomic_write(workspace_path, new_text.as_bytes())
        .into_diagnostic()
        .wrap_err_with(|| {
            format!(
                "failed to write {} after cleanupUnusedCatalogs",
                workspace_path.display()
            )
        })?;
    Ok(unused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_catalog() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("lodash".into(), "^4.17.0".into());
        m.insert("react".into(), "^18.2.0".into());
        m
    }

    #[test]
    fn manual_mode_never_rewrites() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Manual,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn prefer_rewrites_matching_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn prefer_falls_back_on_incompatible_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "^3.0.0",
            true,
            "3.10.0",
            false,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn strict_errors_on_conflicting_range() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "^3.0.0",
            true,
            "3.10.0",
            false,
        );
        assert!(matches!(r, CatalogRewrite::StrictMismatch { .. }));
    }

    #[test]
    fn prefer_rewrites_when_range_implicit() {
        // `aube add lodash` with no version: `range_compatible`
        // short-circuits on `!has_explicit_range`, so `prefer` should
        // rewrite to `catalog:` the same way `strict` does. Captured so
        // a future change to `range_compatible` can't silently flip the
        // bare-add case back to manual mode.
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Prefer,
            Some(&cat),
            "lodash",
            "latest",
            false,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn strict_rewrites_when_range_implicit() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "latest",
            false,
            "4.17.21",
            false,
        );
        assert!(matches!(r, CatalogRewrite::UseDefaultCatalog));
    }

    #[test]
    fn no_catalog_entry_always_manual() {
        let cat = default_catalog();
        for mode in [
            CatalogMode::Manual,
            CatalogMode::Prefer,
            CatalogMode::Strict,
        ] {
            let r = decide_add_rewrite(mode, Some(&cat), "axios", "^1.0.0", true, "1.6.0", false);
            assert!(matches!(r, CatalogRewrite::Manual), "mode={mode:?}");
        }
    }

    #[test]
    fn exclude_flag_short_circuits() {
        let cat = default_catalog();
        let r = decide_add_rewrite(
            CatalogMode::Strict,
            Some(&cat),
            "lodash",
            "^4.17.0",
            true,
            "4.17.21",
            true,
        );
        assert!(matches!(r, CatalogRewrite::Manual));
    }

    #[test]
    fn prune_drops_unused_default_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            "catalog:\n  is-odd: ^3.0.1\n  is-even: ^1.0.0\ncatalogs:\n  evens:\n    is-even: ^1.0.0\n",
        )
        .unwrap();

        let mut declared = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("is-odd".to_string(), "^3.0.1".to_string());
        default.insert("is-even".to_string(), "^1.0.0".to_string());
        declared.insert("default".to_string(), default);
        let mut evens = BTreeMap::new();
        evens.insert("is-even".to_string(), "^1.0.0".to_string());
        declared.insert("evens".to_string(), evens);

        let mut used: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        used.entry("default".to_string()).or_default().insert(
            "is-odd".to_string(),
            aube_lockfile::CatalogEntry {
                specifier: "^3.0.1".into(),
                version: "3.0.1".into(),
            },
        );

        let dropped = prune_unused_catalog_entries(&path, &declared, &used).unwrap();
        assert_eq!(
            dropped,
            vec![
                ("default".to_string(), "is-even".to_string()),
                ("evens".to_string(), "is-even".to_string()),
            ]
        );

        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("is-odd"), "expected is-odd retained");
        assert!(
            !rewritten.contains("is-even"),
            "expected is-even pruned from {rewritten}"
        );
        assert!(
            !rewritten.contains("catalogs:"),
            "empty named catalog container should be removed: {rewritten}"
        );
    }

    #[test]
    fn prune_noop_when_all_entries_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pnpm-workspace.yaml");
        let original = "catalog:\n  is-odd: ^3.0.1\n";
        std::fs::write(&path, original).unwrap();

        let mut declared = BTreeMap::new();
        let mut default = BTreeMap::new();
        default.insert("is-odd".to_string(), "^3.0.1".to_string());
        declared.insert("default".to_string(), default);

        let mut used: BTreeMap<String, BTreeMap<String, aube_lockfile::CatalogEntry>> =
            BTreeMap::new();
        used.entry("default".to_string()).or_default().insert(
            "is-odd".to_string(),
            aube_lockfile::CatalogEntry {
                specifier: "^3.0.1".into(),
                version: "3.0.1".into(),
            },
        );

        let dropped = prune_unused_catalog_entries(&path, &declared, &used).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
}
