//! Pre-resolver direct-dep packument prefetch.
//!
//! aube reads `package.json` keys as raw strings and fires packument
//! GETs for every registry-shaped direct dep before the resolver
//! constructs its first task. The resolver then hits the warm
//! on-disk packument cache + warm reqwest pool when it formally
//! requests them, hiding the manifest-validation / pnpmfile-setup /
//! workspace-yaml / settings-resolution cost behind real network work.
//!
//! On top of the direct-dep walk we also expand by one hop through
//! the bundled metadata primer: for each primer-covered direct dep,
//! the names of its preferred-version transitives that the primer
//! does *not* already cover are added to the prefetch set. Those are
//! the packuments the resolver will fetch over the network anyway
//! once it pops past the direct dep. The registry client's per-name
//! single-flight gate keeps a prefetch race with the resolver from
//! issuing duplicate GETs, so overlapping work coalesces to one
//! round-trip instead of doubling bandwidth.
//!
//! The fetches are best-effort fire-and-forget. Failures, 404s, and
//! offline mode silently no-op; the resolver re-fetches via its own
//! retry path. No PM in the npm-CM-space ships this — npm/pnpm/yarn/bun
//! all wait until the resolver pops its first task before issuing any
//! packument GET.
//!
//! `AUBE_DISABLE_PREFETCH=1` skips the prefetch entirely.
//! `AUBE_DISABLE_SPECULATIVE_PREFETCH=1` keeps the direct-dep walk
//! but skips the primer-transitive expansion.

use std::collections::BTreeSet;
use std::sync::Arc;

/// Upper bound on the speculative primer-transitive expansion.
/// Caps runaway speculative work on huge dep trees and keeps the
/// request fan-out comfortably below npmjs's per-connection HTTP/2
/// concurrent-stream limit (typically 128). Only the transitive
/// expansion is capped — direct deps are always included since the
/// resolver definitely needs them.
const MAX_PREFETCH: usize = 128;

const REGISTRY_LIKE_PREFIXES: &[&str] = &["npm:"];
const NON_REGISTRY_SPEC_MARKERS: &[&str] = &[
    "workspace:",
    "file:",
    "link:",
    "github:",
    "git+",
    "git:",
    "http://",
    "https://",
    "catalog:",
];

/// Returns true when prefetch is disabled.
#[inline]
pub fn is_disabled() -> bool {
    std::env::var_os("AUBE_DISABLE_PREFETCH").is_some()
}

/// Returns true when primer-transitive expansion is disabled.
/// Direct-dep prefetch is unaffected.
#[inline]
fn speculative_is_disabled() -> bool {
    std::env::var_os("AUBE_DISABLE_SPECULATIVE_PREFETCH").is_some()
}

/// Spawn a fire-and-forget packument GET for every registry-shaped
/// direct dep in the manifest. Returns immediately; results are
/// discarded — the win is the warm reqwest pool + warm on-disk
/// packument cache the resolver hits next.
///
/// Honors `AUBE_DISABLE_PREFETCH=1`. No-op when offline OR when any
/// lockfile is present in `cwd` (the resolver will hit its
/// integrity-keyed lockfile fast path and never request packuments,
/// so prefetch is pure bandwidth waste). Spec parsing skips
/// workspace/file/link/git/http/catalog protocols and `npm:` aliases.
pub fn spawn_direct_dep_prefetch(
    manifest: &aube_manifest::PackageJson,
    cwd: &std::path::Path,
    network_mode: aube_registry::NetworkMode,
) {
    if is_disabled() {
        return;
    }
    if matches!(network_mode, aube_registry::NetworkMode::Offline) {
        return;
    }
    if has_lockfile(cwd) {
        return;
    }
    let direct_names = collect_registry_dep_names(manifest);
    if direct_names.is_empty() {
        return;
    }
    let names = expand_with_primer_transitives(&direct_names);
    let client = Arc::new(super::super::make_client(cwd).with_network_mode(network_mode));
    let cache_dir = super::super::packument_cache_dir();
    let total = names.len();
    let direct_count = direct_names.len();
    tracing::debug!(
        "prefetch: spawning {total} packument GETs ({direct_count} direct + {} primer-transitive)",
        total.saturating_sub(direct_count)
    );

    for name in names {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        tokio::spawn(async move {
            if let Err(e) = client.fetch_packument_cached(&name, &cache_dir).await {
                tracing::debug!(name = %name, error = %e, "prefetch fetch failed");
            }
        });
    }
}

/// Union direct deps with their one-hop primer-covered transitives.
/// Direct deps are always included unconditionally — they're
/// known-needed work, not speculation. The [`MAX_PREFETCH`] cap only
/// limits the primer-transitive expansion, so a manifest with
/// hundreds of direct deps simply skips speculative work rather than
/// dropping packuments the resolver will definitely ask for. Honors
/// `AUBE_DISABLE_SPECULATIVE_PREFETCH=1` for users that want the
/// pre-expansion behavior back.
fn expand_with_primer_transitives(direct_names: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = direct_names.iter().cloned().collect();
    if speculative_is_disabled() || out.len() >= MAX_PREFETCH {
        return out.into_iter().collect();
    }
    for name in direct_names {
        if out.len() >= MAX_PREFETCH {
            break;
        }
        let Some(transitives) = aube_resolver::primer_one_hop_deps(name) else {
            continue;
        };
        for t in transitives {
            if out.len() >= MAX_PREFETCH {
                break;
            }
            out.insert(t);
        }
    }
    out.into_iter().collect()
}

fn has_lockfile(cwd: &std::path::Path) -> bool {
    const LOCKFILES: &[&str] = &[
        "aube-lock.yaml",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "yarn.lock",
        "package-lock.json",
        "npm-shrinkwrap.json",
    ];
    LOCKFILES.iter().any(|name| cwd.join(name).exists())
}

fn collect_registry_dep_names(manifest: &aube_manifest::PackageJson) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let sections = [
        &manifest.dependencies,
        &manifest.dev_dependencies,
        &manifest.optional_dependencies,
        &manifest.peer_dependencies,
    ];
    for section in sections {
        for (name, spec) in section.iter() {
            if !is_registry_spec(spec) {
                continue;
            }
            names.push(name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

fn is_registry_spec(spec: &str) -> bool {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return false;
    }
    for marker in NON_REGISTRY_SPEC_MARKERS {
        if trimmed.starts_with(marker) {
            return false;
        }
    }
    // `npm:alias@target` registry-aliased entries route through the
    // resolver's alias rewrite. The alias target prefetch happens
    // there, so skip the alias key itself.
    for prefix in REGISTRY_LIKE_PREFIXES {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_specs_pass() {
        assert!(is_registry_spec("^1.2.3"));
        assert!(is_registry_spec("1.2.3"));
        assert!(is_registry_spec("~1.0"));
        assert!(is_registry_spec("*"));
        assert!(is_registry_spec("latest"));
        assert!(is_registry_spec(">=1.0 <2.0"));
    }

    #[test]
    fn expand_preserves_all_direct_names_under_cap() {
        let direct = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
        let expanded = expand_with_primer_transitives(&direct);
        for name in &direct {
            assert!(expanded.contains(name), "missing direct dep {name}");
        }
    }

    #[test]
    fn expand_preserves_all_direct_names_even_above_cap() {
        // Direct deps are always included unconditionally — the cap
        // only limits speculative primer-transitive expansion.
        let n = MAX_PREFETCH + 50;
        let direct: Vec<String> = (0..n).map(|i| format!("pkg-{i:04}")).collect();
        let expanded = expand_with_primer_transitives(&direct);
        assert_eq!(
            expanded.len(),
            n,
            "every direct dep must survive the expansion"
        );
        for name in &direct {
            assert!(expanded.contains(name));
        }
    }

    #[test]
    fn non_registry_specs_filtered() {
        assert!(!is_registry_spec("workspace:*"));
        assert!(!is_registry_spec("workspace:^1.0"));
        assert!(!is_registry_spec("file:./local"));
        assert!(!is_registry_spec("link:../sibling"));
        assert!(!is_registry_spec("github:user/repo"));
        assert!(!is_registry_spec("git+https://example.com/r.git"));
        assert!(!is_registry_spec("https://example.com/pkg.tgz"));
        assert!(!is_registry_spec("catalog:default"));
        assert!(!is_registry_spec("npm:foo@1.0.0"));
        assert!(!is_registry_spec(""));
    }
}
