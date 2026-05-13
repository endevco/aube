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
//! The CLI-name gate fires only on the names the user typed for
//! *registry* packages — git/local/workspace/jsr/aliased specs all
//! skip both checks because the public-registry signal doesn't
//! apply. Names whose resolved registry isn't `registry.npmjs.org`
//! (per `NpmConfig::is_public_npmjs`) are filtered out upstream in
//! `registry_bound_names_for_supply_chain`.
//!
//! [`run_transitive_osv_gate`] is the post-resolve cousin of the
//! OSV check above: `aube add` flips
//! [`crate::commands::install::InstallOptions::osv_transitive_check`]
//! on, install::run calls this against the resolved graph just
//! before `securityScanner`, and a `MAL-*` hit anywhere in the
//! transitive closure fails the install. The downloads floor is
//! intentionally NOT extended to transitives — a legitimate
//! library can be low-traffic on its own merits.

use aube_codes::errors::{
    ERR_AUBE_ADVISORY_CHECK_FAILED, ERR_AUBE_LOW_DOWNLOAD_PACKAGE, ERR_AUBE_MALICIOUS_PACKAGE,
};
use aube_codes::warnings::{WARN_AUBE_ADVISORY_CHECK_FAILED, WARN_AUBE_LOW_DOWNLOAD_PACKAGE};
use aube_registry::supply_chain::{
    DownloadCount, advisory_url, fetch_malicious_advisories, fetch_weekly_downloads_with,
};
use aube_settings::resolved::AdvisoryCheck;
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
    let Some(client) = probe_client_for_policy(advisory_check)? else {
        return Ok(());
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

/// Post-resolve OSV `MAL-*` check against the full transitive set
/// of a resolved `LockfileGraph`. Called from `install::run` when
/// `aube add` flips `InstallOptions::osv_transitive_check` on. Same
/// `advisoryCheck` policy, same `ERR_AUBE_MALICIOUS_PACKAGE` exit
/// as the CLI-name gate — `paranoid` upgrades `On` to `Required`
/// upstream just like `run_gates` does, so callers don't repeat
/// that logic here.
///
/// Filters mirror `registry_bound_names_for_supply_chain` so a
/// scoped registry override (`@myorg:registry=...`) or a swapped
/// default registry doesn't ship internal package names to OSV.
/// Workspace / `link:` / `file:` entries also drop out via
/// `LockedPackage::local_source.is_none()`.
pub async fn run_transitive_osv_gate(
    cwd: &std::path::Path,
    graph: &aube_lockfile::LockfileGraph,
    advisory_check: AdvisoryCheck,
) -> miette::Result<()> {
    if matches!(advisory_check, AdvisoryCheck::Off) {
        return Ok(());
    }
    let names = transitive_registry_names(cwd, graph);
    if names.is_empty() {
        return Ok(());
    }
    let Some(client) = probe_client_for_policy(advisory_check)? else {
        return Ok(());
    };
    osv_gate(&client, &names, advisory_check).await
}

/// Distinct public-npmjs registry names in `graph`, filtered to
/// match the CLI-name gate. Sorted + deduped so the OSV batch is
/// deterministic; aliased entries report under their real
/// registry name via `LockedPackage::registry_name`.
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

/// Build the shared supply-chain probe client and translate any
/// init failure (TLS, missing root certs, …) into the configured
/// `advisoryCheck` policy:
/// - `Off` → debug-log and skip (`Ok(None)`).
/// - `On` → warn and skip (`Ok(None)`).
/// - `Required` → `Err(ERR_AUBE_ADVISORY_CHECK_FAILED)`.
///
/// Shared between the CLI-name gate and the transitive gate so
/// the two stay byte-identical on probe-init policy. Callers
/// pattern-match `Ok(None)` to short-circuit without an HTTP
/// round-trip.
fn probe_client_for_policy(
    advisory_check: AdvisoryCheck,
) -> miette::Result<Option<reqwest::Client>> {
    match aube_registry::supply_chain::build_probe_client() {
        Ok(c) => Ok(Some(c)),
        Err(e) => {
            if matches!(advisory_check, AdvisoryCheck::Off) {
                tracing::debug!(
                    "supply-chain probe client init failed; OSV is off, skipping all gates: {e}"
                );
                return Ok(None);
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
            Ok(None)
        }
    }
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
            // "install" (vs. "add") covers both the CLI-name gate and
            // the post-resolve transitive gate, where the malicious
            // name is a dep of something the user typed.
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
        // The OSV batch shouldn't see duplicates — and shouldn't
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

    #[tokio::test]
    async fn run_transitive_osv_gate_off_skips_network() {
        // `advisoryCheck = off` shouldn't hit OSV at all — important
        // for air-gapped CI runs that flip the setting precisely to
        // skip the network round-trip. An empty graph also shouldn't
        // emit a request; cover both in one call.
        let graph = aube_lockfile::LockfileGraph::default();
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            run_transitive_osv_gate(tmp.path(), &graph, AdvisoryCheck::Off)
                .await
                .is_ok()
        );
    }
}
