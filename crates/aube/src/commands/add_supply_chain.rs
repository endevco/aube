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
//!    impersonations that haven't been reported to OSV yet.
//!
//! The gate fires only on the names the user typed for *registry*
//! packages — git/local/workspace/jsr/aliased specs all skip both
//! checks because the public-registry signal doesn't apply.

use aube_codes::errors::{
    ERR_AUBE_ADVISORY_CHECK_FAILED, ERR_AUBE_LOW_DOWNLOAD_PACKAGE, ERR_AUBE_MALICIOUS_PACKAGE,
};
use aube_codes::warnings::{WARN_AUBE_ADVISORY_CHECK_FAILED, WARN_AUBE_LOW_DOWNLOAD_PACKAGE};
use aube_registry::supply_chain::{
    DownloadCount, advisory_url, build_probe_client, fetch_malicious_advisories,
    fetch_weekly_downloads_with,
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
pub async fn run_gates(
    names: &[String],
    advisory_check: AdvisoryCheck,
    low_download_threshold: u64,
    allow_low_downloads: bool,
) -> miette::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    osv_gate(names, advisory_check).await?;
    if !allow_low_downloads && low_download_threshold > 0 {
        downloads_gate(names, low_download_threshold).await?;
    }
    Ok(())
}

async fn osv_gate(names: &[String], policy: AdvisoryCheck) -> miette::Result<()> {
    if matches!(policy, AdvisoryCheck::Off) {
        return Ok(());
    }
    match fetch_malicious_advisories(names).await {
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
            match policy {
                AdvisoryCheck::Required => Err(miette!(
                    code = ERR_AUBE_ADVISORY_CHECK_FAILED,
                    "OSV advisory check failed and `advisoryCheck = required` is set: {e}"
                )),
                AdvisoryCheck::On | AdvisoryCheck::Off => Ok(()),
            }
        }
    }
}

async fn downloads_gate(names: &[String], threshold: u64) -> miette::Result<()> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    // Single shared client across all probes — repeated `aube add a b c`
    // would otherwise build a fresh `reqwest::Client` and pay the
    // round-trip serially per package. Probe failures are
    // best-effort (no signal → skip), so a builder error degrades to
    // skipping the whole gate rather than aborting the install.
    let Ok(client) = build_probe_client() else {
        tracing::debug!("downloads probe client init failed; skipping low-download gate");
        return Ok(());
    };
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
        // don't see spurious timeouts on add.
        let names = vec!["lodash".to_string()];
        assert!(osv_gate(&names, AdvisoryCheck::Off).await.is_ok());
    }

    #[tokio::test]
    async fn run_gates_no_op_on_empty() {
        // Workspace/git/local-only invocations end up with an empty
        // registry-name list. The function must be a no-op in that
        // case (no network, no error) so those code paths stay free.
        assert!(
            run_gates(&[], AdvisoryCheck::Required, 1000, false)
                .await
                .is_ok()
        );
    }
}
