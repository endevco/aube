//! Pre-resolver packument prefetch.
//!
//! aube fires packument GETs for every registry-shaped direct dep
//! *and* every transitively-reachable name from the bundled primer's
//! dep graph before the resolver constructs its first task. The
//! resolver then hits the warm on-disk packument cache + warm reqwest
//! pool when it formally requests them, hiding the lockfile parse /
//! pnpmfile setup / workspace-yaml / settings resolution cost behind
//! real network work, and collapsing the BFS-depth RTT chain (which
//! dominates cold installs against real npmjs) into a single fan-out.
//!
//! No PM in the npm-CM-space ships this — npm/pnpm/yarn/bun all wait
//! until the resolver pops its first task before issuing any
//! packument GET, and none of them speculatively prefetch
//! transitive deps from a bundled dep-graph snapshot.
//!
//! Primer-covered names are intentionally excluded from prefetch
//! because the resolver short-circuits the network for those (see
//! `packument_primer_hit` in `aube-resolver/src/resolve.rs`).
//!
//! The fetches are best-effort fire-and-forget. Failures, 404s, and
//! offline mode silently no-op; the resolver re-fetches via its own
//! retry path.
//!
//! `AUBE_DISABLE_PREFETCH=1` skips the prefetch.

use std::sync::Arc;

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

/// Spawn a fire-and-forget packument GET for every direct dep AND
/// every speculative transitive target the bundled primer can
/// identify. Returns immediately; results are discarded — the win
/// is the warm reqwest pool + warm on-disk packument cache the
/// resolver hits next.
///
/// `needs_time` controls cache selection: when true (the default
/// for npmjs + `minimumReleaseAge`/`trustPolicy=no-downgrade`), we
/// fetch the full packument so the resolver's full-packument cache
/// is warmed. When false, we use the cheaper corgi (abbreviated)
/// packument. Mismatched fetches land in the wrong cache and are
/// pure bandwidth waste — picking the right variant is the whole
/// point of taking `needs_time` here.
///
/// No-op when prefetch is disabled, when offline, when any lockfile
/// is present in `cwd` (the resolver hits its integrity-keyed
/// fast path and never requests packuments), or when the manifest
/// has no registry-shaped direct deps. Spec parsing skips
/// workspace/file/link/git/http/catalog protocols and `npm:` aliases.
pub fn spawn_packument_prefetch(
    manifest: &aube_manifest::PackageJson,
    cwd: &std::path::Path,
    network_mode: aube_registry::NetworkMode,
    needs_time: bool,
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
    let direct = collect_registry_dep_names(manifest);
    if direct.is_empty() {
        return;
    }
    // Walk the primer's bundled dep graph from these direct names to
    // find transitively-reachable packages NOT covered by the primer.
    // Those are the names the resolver would otherwise pay a real
    // network RTT for, gated by BFS depth — exactly the chain we want
    // to collapse. Primer-covered names are excluded by construction
    // (the resolver skips the network for them).
    let speculative = aube_resolver::collect_speculative_prefetch_targets(direct.iter().cloned());
    let mut all: Vec<String> = direct;
    all.extend(speculative);
    all.sort();
    all.dedup();

    let client = Arc::new(super::super::make_client(cwd).with_network_mode(network_mode));
    let cache_dir = if needs_time {
        super::super::packument_full_cache_dir()
    } else {
        super::super::packument_cache_dir()
    };
    let count = all.len();
    tracing::debug!("prefetch: spawning {count} packument GETs (needs_time={needs_time})");

    for name in all {
        let client = client.clone();
        let cache_dir = cache_dir.clone();
        tokio::spawn(async move {
            let result = if needs_time {
                client
                    .fetch_packument_with_time_cached(&name, &cache_dir)
                    .await
                    .map(|_| ())
            } else {
                client
                    .fetch_packument_cached(&name, &cache_dir)
                    .await
                    .map(|_| ())
            };
            if let Err(e) = result {
                tracing::debug!(name = %name, error = %e, "prefetch fetch failed");
            }
        });
    }
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
