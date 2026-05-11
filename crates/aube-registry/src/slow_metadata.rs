//! Debounced, grouped warnings for slow registry metadata fetches.
//!
//! `fetchWarnTimeoutMs` is an observability knob: when a packument
//! takes longer than the threshold, aube wants to surface that slowness
//! so operators can spot registry latency without enabling debug
//! tracing. Emitting one warning *per* slow packument floods the
//! install output — a single throttled run can produce dozens of
//! near-identical lines. Waiting until end-of-resolve to summarize
//! goes too far the other way: the user sees nothing for tens of
//! seconds while a slow registry stalls the install.
//!
//! This module sits in the middle: a tumbling window opens on the
//! first slow event and stays open for `FLUSH_WINDOW`. Every event in
//! that window is accumulated into one group, and the window's expiry
//! flushes the group as a single `tracing::warn!`. If more events
//! arrive after the flush, a fresh window opens. The install pipeline
//! still calls [`flush_summary`] at end-of-resolve to drain any trailing
//! group whose window hasn't expired.
//!
//! The result: groups roughly the size of "events that arrived close
//! together in time," refreshed at the cadence of the window. The
//! `code = WARN_AUBE_SLOW_METADATA` field on each emission is
//! unchanged — CI scripts and ndjson reporters that branch on the
//! code stay working.
//!
//! Per-event detail (label + elapsed) is *not* logged at any level by
//! default. `--loglevel debug` floods unrelated DEBUG sites and isn't
//! a usable escape hatch; if structured per-package telemetry is ever
//! needed, ndjson is the right vehicle.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Window during which slow-fetch events are coalesced into one group.
/// Long enough that bursts (~18 events within a few seconds) emit a
/// single warning; short enough that streaming slowness surfaces in
/// near-real-time rather than waiting for end-of-resolve. The
/// threshold itself is typically 10s, so a sub-threshold window keeps
/// groups distinguishable: at most one group per window, multiple
/// groups per install when latency persists.
const FLUSH_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
struct Record {
    label: String,
    elapsed_ms: u64,
}

#[derive(Default)]
struct State {
    records: Vec<Record>,
    /// Whether a background timer is currently armed to flush the
    /// current window. First event in a window arms it; the timer
    /// drains the group on expiry and clears the flag so the next
    /// event opens a fresh window.
    timer_armed: bool,
    /// Most recent `fetchWarnTimeoutMs` seen at a [`record`] call.
    /// Carried into the timer-driven summary so the grouped warning
    /// can name the threshold without the timer task re-reading
    /// settings. Invariant across a single install run.
    threshold_ms: u64,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// Record that `label` took `elapsed_ms` and exceeded `threshold_ms`
/// (`fetchWarnTimeoutMs`). Called by the registry client's metadata
/// fetch path in place of a per-event `tracing::warn!`.
///
/// The first event in a window also arms a background tokio timer
/// that drains the group after [`FLUSH_WINDOW`]. Subsequent events
/// inside that window simply accumulate. If no tokio runtime is
/// available (unit tests outside `#[tokio::test]`), no timer is
/// armed and the group only drains via [`flush_summary`] — that's
/// the documented test-time behavior.
pub fn record(label: &str, elapsed_ms: u64, threshold_ms: u64) {
    let needs_arm = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        g.records.push(Record {
            label: label.to_string(),
            elapsed_ms,
        });
        g.threshold_ms = threshold_ms;
        if g.timer_armed {
            false
        } else {
            g.timer_armed = true;
            true
        }
    };
    if needs_arm {
        arm_flush_timer();
    }
}

/// Spawn the once-per-window flush task. Best-effort: if no tokio
/// runtime is current (unit tests outside `#[tokio::test]`), we leave
/// `timer_armed = true` and rely on [`flush_summary`] to drain at
/// end-of-resolve. Production call sites are always inside a running
/// runtime (the install pipeline drives the registry client through
/// `tokio::spawn`/`JoinSet`), so this fallback is purely a test path.
fn arm_flush_timer() {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime: leave the flag armed; the next `record` call
        // sees `timer_armed = true` and won't spawn a second; the
        // end-of-resolve `flush_summary` drains the group instead.
        return;
    };
    handle.spawn(async {
        tokio::time::sleep(FLUSH_WINDOW).await;
        drain_window();
    });
}

/// Drain the currently-buffered window into a single `tracing::warn!`
/// and reset the timer flag so the next event opens a fresh window.
/// No-op when the buffer is empty (defensive; the timer task and
/// [`flush_summary`] can race at end-of-resolve and the loser sees an
/// empty buffer).
fn drain_window() {
    let (records, threshold_ms) = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        let records = std::mem::take(&mut g.records);
        g.timer_armed = false;
        (records, g.threshold_ms)
    };
    emit(records, threshold_ms);
}

fn emit(records: Vec<Record>, threshold_ms: u64) {
    let count = records.len();
    if count == 0 {
        return;
    }
    let slowest = records
        .iter()
        .max_by_key(|r| r.elapsed_ms)
        .expect("count > 0 implies a slowest record");
    tracing::warn!(
        count,
        threshold_ms,
        slowest_label = %slowest.label,
        slowest_ms = slowest.elapsed_ms,
        code = aube_codes::warnings::WARN_AUBE_SLOW_METADATA,
        "registry slow: {count} metadata fetches took longer than {threshold_ms}ms (slowest: {} at {}ms)",
        slowest.label,
        slowest.elapsed_ms,
    );
}

/// Drain any trailing group whose window hasn't fired yet. Called
/// once at the end of the resolve phase so the user sees the tail of
/// the slow-fetch activity before the fetch/link phases take over.
///
/// `threshold_ms` overrides whatever the last `record` call stashed —
/// callers in the install pipeline have the live `ResolveCtx` and
/// pass the authoritative value here, which matters when no events
/// fired and the cached threshold is still the `Default::default()`
/// zero. (No-op early-returns on empty drain, so the override only
/// matters when there are records to emit.)
pub fn flush_summary(threshold_ms: u64) {
    let records = {
        let Ok(mut g) = state().lock() else {
            return;
        };
        let records = std::mem::take(&mut g.records);
        g.timer_armed = false;
        records
    };
    emit(records, threshold_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-global state plus parallel test execution means two
    /// tests touching the accumulator can race. Serialize the whole
    /// module under one test entry point so the cases run
    /// deterministically.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn debounced_grouping_lifecycle() {
        // Reset state in case another test ran first.
        {
            let mut g = state().lock().unwrap();
            g.records.clear();
            g.timer_armed = false;
            g.threshold_ms = 0;
        }

        // Empty case: flush with nothing accumulated emits nothing
        // and leaves the buffer empty.
        flush_summary(10_000);
        assert!(state().lock().unwrap().records.is_empty());

        // First event arms the timer. Second event in the same window
        // joins the group without arming again.
        record("packument a", 11_000, 10_000);
        record("packument b", 13_500, 10_000);
        {
            let g = state().lock().unwrap();
            assert_eq!(g.records.len(), 2, "both events buffered into the group");
            assert!(g.timer_armed, "first event armed the flush timer");
        }

        // Advance virtual time past the window; the timer task drains
        // the group and clears the flag. Multiple yields cover the
        // case where the spawned task's wake takes more than one poll
        // cycle to flush on the current-thread runtime.
        tokio::time::sleep(FLUSH_WINDOW + Duration::from_millis(50)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        {
            let g = state().lock().unwrap();
            assert!(
                g.records.is_empty(),
                "timer must drain the window after FLUSH_WINDOW",
            );
            assert!(!g.timer_armed, "timer must clear the armed flag on drain",);
        }

        // New event after the drain opens a fresh window.
        record("packument c", 14_000, 10_000);
        assert!(state().lock().unwrap().timer_armed);

        // Explicit end-of-resolve flush drains the trailing group
        // even though the window hasn't expired.
        flush_summary(10_000);
        {
            let g = state().lock().unwrap();
            assert!(g.records.is_empty(), "flush_summary drains the tail");
            assert!(!g.timer_armed, "flush_summary clears the armed flag");
        }
    }
}
