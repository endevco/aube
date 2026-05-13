//! Supply-chain gates that run at the top of `aube add`.
//!
//! Two checks, layered by signal strength:
//!
//! 1. **OSV `MAL-*` advisory check** — hard block via
//!    `ERR_AUBE_MALICIOUS_PACKAGE`. Confirmed malicious advisories
//!    aren't a judgement call. Default fails open on a fetch error
//!    (so offline workflows still install); `advisoryCheck=required`
//!    flips that to fail closed for hardened CI.
//!
//! 2. **Weekly-downloads floor** — interactive confirm prompt below
//!    the threshold, hard refusal in non-interactive contexts unless
//!    `--allow-low-downloads` is passed. Catches typosquats and
//!    impersonations that haven't been reported to OSV yet. The
//!    `allowedUnpopularPackages` setting (glob patterns) bypasses
//!    this gate for opted-in names, leaving the OSV check intact.
//!
//! The gate fires only on the names the user typed for *registry*
//! packages — git/local/workspace/jsr/aliased specs all skip both
//! checks because the public-registry signal doesn't apply. Names
//! whose resolved registry isn't `registry.npmjs.org` (per
//! `NpmConfig::is_public_npmjs`) are filtered out upstream in
//! `registry_bound_names_for_supply_chain`.

use aube_codes::errors::{
    ERR_AUBE_ADVISORY_CHECK_FAILED, ERR_AUBE_LOW_DOWNLOAD_PACKAGE, ERR_AUBE_MALICIOUS_PACKAGE,
};
use aube_codes::warnings::{
    WARN_AUBE_ADVISORY_CHECK_FAILED, WARN_AUBE_LOW_DOWNLOAD_PACKAGE,
    WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
};
use aube_registry::osv_mirror::OsvMirror;
use aube_registry::supply_chain::{
    DownloadCount, advisory_url, fetch_malicious_advisories, fetch_weekly_downloads_with,
};
use aube_settings::resolved::{AdvisoryCheck, AdvisoryCheckOnInstall};
use miette::miette;
use std::io::{BufRead, IsTerminal, Write};

/// Run both supply-chain gates against the registry-bound names the
/// user passed to `aube add`. `names` should already be filtered to
/// names that resolve via the public npm registry — workspace, git,
/// and local specs are not in scope.
///
/// `allow_low_downloads` is the per-invocation `--allow-low-downloads`
/// override; when `true` the download gate is skipped entirely (the
/// advisory check still runs).
///
/// `allowed_unpopular_globs` are the `allowedUnpopularPackages`
/// setting entries: full-name globs that exempt matching names from
/// the downloads gate only. The advisory check still runs against
/// every name regardless — exempting confirmed-malicious advisories
/// is not what this list is for.
pub async fn run_gates(
    names: &[String],
    advisory_check: AdvisoryCheck,
    low_download_threshold: u64,
    allow_low_downloads: bool,
    allowed_unpopular_globs: &[String],
) -> miette::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    // One client shared across both gates and every per-package
    // probe so the OSV POST and the (potentially parallel) downloads
    // GETs all reuse the same connection pool + TLS session.
    //
    // Builder failure (TLS init, no root certs, etc.) routes through
    // the same `advisoryCheck` policy `osv_gate` applies to HTTP
    // failures: under `Required` it's a hard fail with
    // `ERR_AUBE_ADVISORY_CHECK_FAILED`, otherwise it warns and skips
    // both gates. `Off` short-circuits before even surfacing the
    // warning — the user opted out of OSV entirely, so a probe-
    // client init failure is no longer their concern.
    let client = match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => c,
        Err(e) => {
            if matches!(advisory_check, AdvisoryCheck::Off) {
                tracing::debug!(
                    "supply-chain probe client init failed; OSV is off, skipping all gates: {e}"
                );
                return Ok(());
            }
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "supply-chain probe client init failed: {e}"
            );
            if matches!(advisory_check, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "supply-chain probe client could not be initialised and `advisoryCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    osv_gate(&client, names, advisory_check).await?;
    if !allow_low_downloads && low_download_threshold > 0 {
        let patterns = compile_allowed_unpopular(allowed_unpopular_globs);
        let gated: Vec<String> = names
            .iter()
            .filter(|n| !patterns.iter().any(|p| p.matches(n)))
            .cloned()
            .collect();
        if !gated.is_empty() {
            downloads_gate(&client, &gated, low_download_threshold).await?;
        }
    }
    Ok(())
}

/// Single entry point for the post-resolve OSV `MAL-*` routing
/// `install::run` uses from both lockfile branches.
///
/// Pre-PR the routing block was inline in the no-lockfile (Err)
/// match arm only, so `aube ci` and any `aube install` that
/// matched the lockfile cleanly silently skipped OSV entirely —
/// `advisoryCheckEveryInstall = true` and `advisoryCheckOnInstall`
/// were designed for exactly that path. Extracted here so both
/// arms call the same helper and the routing table actually
/// applies to every install entry point.
///
/// Decision table:
/// - `fresh_resolution || osv_transitive_check || advisory_check_every_install`
///   → live OSV API (`run_transitive_osv_gate`)
/// - otherwise, when `advisory_check_on_install != Off`
///   → local mirror (`run_transitive_osv_gate_via_mirror`)
/// - otherwise → no OSV check
///
/// `advisory_check` is the caller's already-upgraded policy
/// (paranoid → `Required`). Both gates internally short-circuit
/// on `Off`, but skip the call entirely so an empty graph
/// doesn't get a useless `transitive_registry_names` walk.
pub async fn run_post_resolve_osv_routing(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    fresh_resolution: bool,
    osv_transitive_check: bool,
    advisory_check: AdvisoryCheck,
    advisory_check_on_install: AdvisoryCheckOnInstall,
    advisory_check_every_install: bool,
) -> miette::Result<()> {
    let needs_live_api = osv_transitive_check || advisory_check_every_install || fresh_resolution;
    if needs_live_api {
        if !matches!(advisory_check, AdvisoryCheck::Off) {
            run_transitive_osv_gate(cwd, graph, advisory_check).await?;
        }
    } else if !matches!(advisory_check_on_install, AdvisoryCheckOnInstall::Off) {
        run_transitive_osv_gate_via_mirror(cwd, graph, advisory_check_on_install).await?;
    }
    Ok(())
}

/// Live-API transitive OSV `MAL-*` check.
///
/// Runs against the full post-resolve transitive set, batch-querying
/// `api.osv.dev`. Used by the fresh-resolution install paths —
/// `aube add`, `aube update`, missing-lockfile installs, and any
/// install where the resolver picked a `(name, version)` the
/// lockfile didn't already pin. The companion
/// [`run_transitive_osv_gate_via_mirror`] is the local-mirror
/// fallback for plain reinstalls where the lockfile was authoritative.
///
/// Policy mapping (same shape as the existing CLI-name gate):
/// - `Off` → no-op.
/// - `On` → OSV fetch failures degrade to
///   `WARN_AUBE_ADVISORY_CHECK_FAILED` and install proceeds.
/// - `Required` → fetch failures map to
///   `ERR_AUBE_ADVISORY_CHECK_FAILED`. Hits map to
///   `ERR_AUBE_MALICIOUS_PACKAGE` under both `On` and `Required`.
pub async fn run_transitive_osv_gate(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: AdvisoryCheck,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    let names = transitive_registry_names(cwd, graph);
    if names.is_empty() {
        return Ok(());
    }
    let client = match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "supply-chain probe client init failed: {e}"
            );
            if matches!(policy, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "supply-chain probe client could not be initialised and `advisoryCheck = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    osv_gate(&client, &names, policy).await
}

/// Mirror-backed transitive OSV `MAL-*` check for plain reinstalls.
///
/// Counterpart to [`run_transitive_osv_gate`]. Fires when none of
/// the fresh-resolution triggers apply (no `aube add` / `aube
/// update`, no `advisoryCheckEveryInstall`, no lockfile drift) — so
/// the live-API gate is dormant and the mirror picks up the slack
/// against the post-resolve graph without an `api.osv.dev`
/// round-trip on every reinstall. Off by default.
///
/// Policy mapping (mirrors the live-API gate's shape so CI configs
/// that have `advisoryCheck = required` can mirror that bit onto
/// `advisoryCheckOnInstall = required` without surprise):
/// - `Off` → no-op.
/// - `On` → mirror refresh failures degrade to `WARN_AUBE_OSV_MIRROR_REFRESH_FAILED`
///   and a `tracing::warn!`; install continues against the prior
///   (possibly empty) on-disk index.
/// - `Required` → mirror refresh failures map to
///   `ERR_AUBE_ADVISORY_CHECK_FAILED`. Hits map to
///   `ERR_AUBE_MALICIOUS_PACKAGE` under both `On` and `Required`,
///   same as the live-API gate.
pub async fn run_transitive_osv_gate_via_mirror(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    policy: AdvisoryCheckOnInstall,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheckOnInstall::Off) {
        return Ok(());
    }
    let names = transitive_registry_names(cwd, graph);
    if names.is_empty() {
        return Ok(());
    }
    let Some(cache_dir) = aube_store::dirs::cache_dir() else {
        // `$HOME` (or platform equivalent) is unset, so we can't
        // open the mirror. Same policy split as a refresh failure
        // — `Required` is a hard stop, `On` is a warning.
        tracing::warn!(
            code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
            "OSV mirror cache dir unavailable (HOME/XDG_CACHE_HOME unset); skipping install-time advisory check"
        );
        if matches!(policy, AdvisoryCheckOnInstall::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV mirror cache dir unavailable and `advisoryCheckOnInstall = required` is set"
            ));
        }
        return Ok(());
    };
    let mirror = OsvMirror::open(&cache_dir);
    let client = match OsvMirror::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
                "OSV mirror probe client init failed: {e}"
            );
            if matches!(policy, AdvisoryCheckOnInstall::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV mirror probe client could not be initialised and `advisoryCheckOnInstall = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if let Err(e) = mirror.refresh_if_stale_default(&client).await {
        tracing::warn!(
            code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
            "OSV mirror refresh failed: {e}"
        );
        if matches!(policy, AdvisoryCheckOnInstall::Required) {
            return Err(miette!(
                code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                "OSV mirror refresh failed and `advisoryCheckOnInstall = required` is set: {e}"
            ));
        }
        // Fall through under `On`: `refresh_if_stale` already
        // seeded the in-memory cache with whatever the on-disk
        // index held going in, so `lookup_advisories` below
        // checks against the previously cached data. When the
        // mirror has never been synced successfully the prior
        // data is empty and lookup is a no-op — the warning is
        // the only user-visible signal in that case.
    }
    let hits = match mirror.lookup_advisories(&names) {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_OSV_MIRROR_REFRESH_FAILED,
                "OSV mirror lookup failed: {e}"
            );
            if matches!(policy, AdvisoryCheckOnInstall::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV mirror lookup failed and `advisoryCheckOnInstall = required` is set: {e}"
                ));
            }
            return Ok(());
        }
    };
    if hits.is_empty() {
        return Ok(());
    }
    let mut lines = vec!["refusing to install malicious package(s):".to_string()];
    for hit in &hits {
        lines.push(format!(
            "  - {} ({}: {})",
            hit.package,
            hit.advisory_id,
            advisory_url(&hit.advisory_id),
        ));
    }
    lines.push(String::new());
    lines.push("Set `advisoryCheckOnInstall = off` to bypass (not recommended).".to_string());
    Err(miette!(
        code = ERR_AUBE_MALICIOUS_PACKAGE,
        "{}",
        lines.join("\n")
    ))
}

/// True when the resolved graph contains at least one
/// `(registry_name, version)` pair the pre-existing lockfile
/// didn't already pin — meaning the resolver did fresh work and
/// the result deserves a live-API OSV pass rather than the
/// mirror-backed fallback. A missing pre-existing lockfile
/// (`None`) is treated as drift by definition: nothing on disk
/// vouched for what just got resolved.
///
/// Filtered to public-npmjs registry names so private / workspace
/// / git / file deps don't get classified as "new" just because
/// they aren't in a public-npmjs comparison set.
pub fn lockfile_has_new_picks(
    cwd: &std::path::Path,
    prior: Option<&aube_lockfile::LockfileGraph>,
    resolved: &aube_lockfile::LockfileGraph,
) -> bool {
    let Some(prior) = prior else {
        // No lockfile before resolve: every resolved entry is a
        // fresh pick. Skip the loop and short-circuit.
        return !resolved.packages.is_empty();
    };
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    use std::collections::HashSet;
    let prior_pairs: HashSet<(&str, &str)> = prior
        .packages
        .values()
        .filter(|p| p.local_source.is_none())
        .map(|p| (p.registry_name(), p.version.as_str()))
        .collect();
    resolved
        .packages
        .values()
        .filter(|p| p.local_source.is_none())
        .filter(|p| npm_config.is_public_npmjs(p.registry_name()))
        .any(|p| !prior_pairs.contains(&(p.registry_name(), p.version.as_str())))
}

/// Distinct public-npmjs registry names in `graph`, filtered to
/// match the CLI-name gate's `registry_bound_names_for_supply_chain`
/// shape so a scoped registry override (`@myorg:registry=...`) or a
/// swapped default registry doesn't ship internal package names to
/// OSV. Workspace / `link:` / `file:` entries drop out via
/// `LockedPackage::local_source.is_none()`. Sorted + deduped so
/// aliased entries (`{"my-alias": "npm:lodash@^4"}`) collapse onto
/// their real registry name.
fn transitive_registry_names(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
) -> Vec<String> {
    let npm_config = aube_registry::config::NpmConfig::load(cwd);
    let mut names: Vec<String> = graph
        .packages
        .values()
        .filter(|pkg| pkg.local_source.is_none())
        .map(|pkg| pkg.registry_name().to_string())
        .filter(|name| npm_config.is_public_npmjs(name))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Parse `allowedUnpopularPackages` entries into compiled
/// `glob::Pattern`s. Invalid entries are logged and dropped — we'd
/// rather miss an exemption (and prompt the user) than fail the
/// whole `aube add` over a typo in a user-defined glob.
fn compile_allowed_unpopular(raw: &[String]) -> Vec<glob::Pattern> {
    raw.iter()
        .filter_map(|p| match glob::Pattern::new(p) {
            Ok(pat) => Some(pat),
            Err(e) => {
                tracing::warn!("ignoring malformed allowedUnpopularPackages entry `{p}`: {e}");
                None
            }
        })
        .collect()
}

async fn osv_gate(
    client: &reqwest::Client,
    names: &[String],
    policy: AdvisoryCheck,
) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    match fetch_malicious_advisories(client, names).await {
        Ok(hits) if hits.is_empty() => Ok(()),
        Ok(hits) => {
            // First hit drives the error message; subsequent hits are
            // chained in for visibility. Confirmed-malicious is a hard
            // block — we don't care whether the user is interactive.
            let mut lines = vec!["refusing to add malicious package(s):".to_string()];
            for hit in &hits {
                lines.push(format!(
                    "  - {} ({}: {})",
                    hit.package,
                    hit.advisory_id,
                    advisory_url(&hit.advisory_id),
                ));
            }
            lines.push(String::new());
            lines.push("Set `advisoryCheck = off` to bypass (not recommended).".to_string());
            Err(miette!(
                code = ERR_AUBE_MALICIOUS_PACKAGE,
                "{}",
                lines.join("\n")
            ))
        }
        Err(e) => {
            tracing::warn!(
                code = WARN_AUBE_ADVISORY_CHECK_FAILED,
                "OSV advisory check failed: {e}"
            );
            // `AdvisoryCheck::Off` short-circuits at the top of
            // `osv_gate` and never reaches this branch — only the
            // `On` / `Required` split needs handling here.
            if matches!(policy, AdvisoryCheck::Required) {
                return Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV advisory check failed and `advisoryCheck = required` is set: {e}"
                ));
            }
            Ok(())
        }
    }
}

async fn downloads_gate(
    client: &reqwest::Client,
    names: &[String],
    threshold: u64,
) -> miette::Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let mut set: tokio::task::JoinSet<(String, Result<DownloadCount, _>)> =
        tokio::task::JoinSet::new();
    for name in names {
        let client = client.clone();
        let name = name.clone();
        set.spawn(async move {
            let result = fetch_weekly_downloads_with(&client, &name).await;
            (name, result)
        });
    }
    // Preserve input order so the warning / prompt sequence is
    // deterministic regardless of which probe returns first.
    let mut by_name: std::collections::HashMap<String, _> =
        std::collections::HashMap::with_capacity(names.len());
    while let Some(joined) = set.join_next().await {
        // `join_next` only errors on panic / cancellation — those are
        // bugs in this call site rather than expected probe failures,
        // so propagate via tracing and skip the slot. The OSV gate
        // above is still the harder line.
        let (name, result) = match joined {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("downloads probe task join failed: {e}");
                continue;
            }
        };
        by_name.insert(name, result);
    }
    for name in names {
        let Some(result) = by_name.remove(name) else {
            continue;
        };
        let count = match result {
            Ok(c) => c,
            Err(e) => {
                // Treat a downloads-API fetch error as "no signal" —
                // we'd rather let a sketchy install through than break
                // every add when api.npmjs.org has a hiccup.
                tracing::debug!("downloads probe failed for {name}: {e}");
                continue;
            }
        };
        let DownloadCount::Known(weekly) = count else {
            // Scoped packages, brand-new names with no published
            // history, or registry mirrors that don't proxy
            // `api.npmjs.org` all fall here. No signal → no gate.
            continue;
        };
        if weekly >= threshold {
            continue;
        }
        tracing::warn!(
            code = WARN_AUBE_LOW_DOWNLOAD_PACKAGE,
            "{name}: {weekly} weekly downloads (threshold: {threshold})"
        );
        if !interactive {
            return Err(miette!(
                code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
                "refusing to add {name}: only {weekly} weekly downloads (threshold: {threshold}). Pass --allow-low-downloads to bypass, or set `lowDownloadThreshold = 0`."
            ));
        }
        if !prompt_continue(name, weekly, threshold)? {
            return Err(miette!(
                code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
                "user aborted `aube add {name}`"
            ));
        }
    }
    Ok(())
}

fn prompt_continue(name: &str, weekly: u64, threshold: u64) -> miette::Result<bool> {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "  ⚠ {name} looks suspicious:").ok();
    writeln!(
        stderr,
        "    • {weekly} downloads last week (threshold: {threshold})"
    )
    .ok();
    write!(stderr, "  Continue adding {name}? [y/N] ").ok();
    stderr.flush().ok();
    drop(stderr);

    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| {
        miette!(
            code = ERR_AUBE_LOW_DOWNLOAD_PACKAGE,
            "failed to read confirmation: {e}"
        )
    })?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn osv_gate_off_skips_network() {
        // `Off` short-circuits before any HTTP — important so users
        // who set `advisoryCheck = off` for an air-gapped registry
        // don't see spurious timeouts on add. The dummy client is
        // never touched on this code path; we still have to
        // construct one to satisfy the type signature.
        let client = aube_registry::supply_chain::build_probe_client()
            .expect("probe client builder shouldn't fail in tests");
        let names = vec!["lodash".to_string()];
        assert!(osv_gate(&client, &names, AdvisoryCheck::Off).await.is_ok());
    }

    #[tokio::test]
    async fn run_gates_no_op_on_empty() {
        // Workspace/git/local-only invocations end up with an empty
        // registry-name list. The function must be a no-op in that
        // case (no network, no error) so those code paths stay free.
        assert!(
            run_gates(&[], AdvisoryCheck::Required, 1000, false, &[])
                .await
                .is_ok()
        );
    }

    #[test]
    fn compile_allowed_unpopular_drops_invalid_patterns() {
        // `[` is a malformed range — we keep the well-formed entries
        // and drop the broken one so a single typo doesn't disable
        // every exemption.
        let pats = compile_allowed_unpopular(&[
            "@myorg/*".to_string(),
            "[unterminated".to_string(),
            "internal-*".to_string(),
        ]);
        assert_eq!(pats.len(), 2);
        assert!(pats.iter().any(|p| p.matches("@myorg/foo")));
        assert!(pats.iter().any(|p| p.matches("internal-thing")));
        assert!(!pats.iter().any(|p| p.matches("public-pkg")));
    }

    #[test]
    fn compile_allowed_unpopular_scope_glob_matches_only_in_scope() {
        // `@myorg/*` should match every name in the `@myorg` scope
        // but not a same-named unscoped package, and not a different
        // scope. Catches the regression where a too-greedy pattern
        // (e.g. plain `myorg*`) would skip arbitrary names.
        let pats = compile_allowed_unpopular(&["@myorg/*".to_string()]);
        assert!(pats[0].matches("@myorg/utils"));
        assert!(pats[0].matches("@myorg/nested-name"));
        assert!(!pats[0].matches("@otherorg/utils"));
        assert!(!pats[0].matches("myorg-utils"));
    }

    fn registry_pkg(name: &str, version: &str) -> aube_lockfile::LockedPackage {
        aube_lockfile::LockedPackage {
            name: name.to_string(),
            version: version.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn transitive_registry_names_skips_local_source_entries() {
        // `file:` / `link:` / workspace edges resolve outside the
        // public registry — OSV has nothing to say about them, and
        // forwarding the workspace package name to OSV could leak
        // an internal name to a public API.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let mut linked = registry_pkg("@workspace/util", "1.0.0");
        linked.local_source = Some(aube_lockfile::LocalSource::Link("../util".into()));
        packages.insert("@workspace/util@1.0.0".to_string(), linked);
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let names = transitive_registry_names(tmp.path(), &graph);
        assert_eq!(names, vec!["lodash".to_string()]);
    }

    #[test]
    fn transitive_registry_names_dedups_by_registry_name() {
        // Alias entries (`{"my-alias": "npm:lodash@^4"}`) and the
        // real package both report under `registry_name() = "lodash"`.
        // The mirror lookup shouldn't see duplicates — and shouldn't
        // surface the alias name to the public API either.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let mut aliased = registry_pkg("my-alias", "4.17.21");
        aliased.alias_of = Some("lodash".to_string());
        packages.insert("my-alias@4.17.21".to_string(), aliased);
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let names = transitive_registry_names(tmp.path(), &graph);
        assert_eq!(names, vec!["lodash".to_string()]);
    }

    #[test]
    fn lockfile_drift_no_prior_lockfile_is_drift_when_resolved_has_entries() {
        // No on-disk lockfile + non-empty resolve = fresh
        // resolution by definition. The router needs this to flip
        // to the live API even though the user typed plain
        // `aube install`.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let resolved = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(lockfile_has_new_picks(tmp.path(), None, &resolved));
    }

    #[test]
    fn lockfile_drift_empty_resolve_and_no_prior_is_not_drift() {
        // No lockfile + nothing resolved (e.g. a workspace with no
        // deps) shouldn't flip the live-API gate on — there's
        // nothing to check anyway.
        let resolved = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), None, &resolved));
    }

    #[test]
    fn lockfile_drift_fully_pinned_is_not_drift() {
        // Prior and resolved hold the same (registry_name, version)
        // pair: the lockfile was authoritative, fall through to the
        // mirror path.
        use std::collections::BTreeMap;
        let mut prior_packages = BTreeMap::new();
        prior_packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let prior = aube_lockfile::LockfileGraph {
            packages: prior_packages.clone(),
            ..Default::default()
        };
        let resolved = aube_lockfile::LockfileGraph {
            packages: prior_packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[test]
    fn lockfile_drift_new_version_is_drift() {
        // Resolver picked a version the lockfile didn't pin — the
        // canonical fresh-resolution signal. Same name, different
        // version, both public-npmjs.
        use std::collections::BTreeMap;
        let mut prior_packages = BTreeMap::new();
        prior_packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let prior = aube_lockfile::LockfileGraph {
            packages: prior_packages,
            ..Default::default()
        };
        let mut resolved_packages = BTreeMap::new();
        resolved_packages.insert(
            "lodash@4.17.22".to_string(),
            registry_pkg("lodash", "4.17.22"),
        );
        let resolved = aube_lockfile::LockfileGraph {
            packages: resolved_packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[test]
    fn lockfile_drift_ignores_local_source_entries() {
        // Workspace / link: / file: entries shouldn't trigger
        // drift detection even when they aren't in the prior
        // lockfile — they don't resolve through the public
        // registry, so OSV has no signal on them.
        use std::collections::BTreeMap;
        let mut resolved_packages = BTreeMap::new();
        let mut linked = registry_pkg("@workspace/util", "1.0.0");
        linked.local_source = Some(aube_lockfile::LocalSource::Link("../util".into()));
        resolved_packages.insert("@workspace/util@1.0.0".to_string(), linked);
        let resolved = aube_lockfile::LockfileGraph {
            packages: resolved_packages,
            ..Default::default()
        };
        let prior = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!lockfile_has_new_picks(tmp.path(), Some(&prior), &resolved));
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_off_skips_network() {
        // Mirror of the live-API gate's off-policy test: `Off`
        // must short-circuit before any client construction or
        // network access.
        let graph = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate(tmp.path(), &graph, AdvisoryCheck::Off)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_via_mirror_off_short_circuits() {
        // `advisoryCheckOnInstall = off` is the default for every
        // user that hasn't opted in. A `LockfileGraph` with real
        // entries must not refresh the on-disk mirror or hit the
        // network — that would defeat the "no per-install network
        // cost" promise of the install-time gate.
        use std::collections::BTreeMap;
        let mut packages = BTreeMap::new();
        packages.insert(
            "lodash@4.17.21".to_string(),
            registry_pkg("lodash", "4.17.21"),
        );
        let graph = aube_lockfile::LockfileGraph {
            packages,
            ..Default::default()
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate_via_mirror(tmp.path(), &graph, AdvisoryCheckOnInstall::Off,)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_transitive_osv_gate_via_mirror_empty_graph_is_noop() {
        // No public-npmjs entries → nothing to check. The mirror
        // should not even be opened, much less refreshed.
        let graph = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate_via_mirror(tmp.path(), &graph, AdvisoryCheckOnInstall::On,)
                .await
                .is_ok()
        );
    }
}
