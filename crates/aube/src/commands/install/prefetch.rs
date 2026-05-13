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
use std::path::Path;
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
///
/// Direct-dep GETs are fired synchronously (no per-name decode cost).
/// The speculative primer-transitive expansion runs in a separate
/// background task so the zstd decode of primer entries doesn't sit
/// on install startup; the registry client's single-flight gate keeps
/// the deferred prefetches from racing the resolver into duplicate
/// GETs.
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
    let client = Arc::new(super::super::make_client(cwd).with_network_mode(network_mode));
    let cache_dir = super::super::packument_cache_dir();
    let direct_count = direct_names.len();
    tracing::debug!("prefetch: spawning {direct_count} direct-dep packument GETs");

    for name in &direct_names {
        spawn_one(&client, &cache_dir, name.clone());
    }

    if speculative_is_disabled() {
        return;
    }
    // Defer the primer-transitive expansion: each `primer::get` decodes
    // a zstd-compressed rkyv entry, which adds up across N direct deps.
    // Running the walk in a background task keeps that cost off the
    // critical path; the single-flight gate in the registry client
    // handles any race with the resolver popping the same name.
    tokio::spawn(async move {
        let mut budget = MAX_PREFETCH;
        let direct_set: BTreeSet<&str> = direct_names.iter().map(String::as_str).collect();
        let mut fired: BTreeSet<String> = BTreeSet::new();
        let mut transitive_count = 0usize;
        for name in &direct_names {
            if budget == 0 {
                break;
            }
            let Some(transitives) = aube_resolver::primer_one_hop_deps(name) else {
                continue;
            };
            for t in transitives {
                if budget == 0 {
                    break;
                }
                if direct_set.contains(t.as_str()) || !fired.insert(t.clone()) {
                    continue;
                }
                spawn_one(&client, &cache_dir, t);
                budget -= 1;
                transitive_count += 1;
            }
        }
        tracing::debug!("prefetch: spawned {transitive_count} primer-transitive packument GETs");
    });
}

fn spawn_one(client: &Arc<aube_registry::client::RegistryClient>, cache_dir: &Path, name: String) {
    let client = client.clone();
    let cache_dir = cache_dir.to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = client.fetch_packument_cached(&name, &cache_dir).await {
            tracing::debug!(name = %name, error = %e, "prefetch fetch failed");
        }
    });
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
