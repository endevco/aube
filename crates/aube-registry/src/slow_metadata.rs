//! Coalesced warning for slow registry metadata fetches.
//!
//! `fetchWarnTimeoutMs` is an observability knob: when a packument
//! takes longer than the threshold, aube emits a warning so operators
//! can spot registry latency without enabling debug tracing. Emitting
//! one warning *per* slow packument floods the install output — a
//! single throttled run can produce dozens of near-identical lines.
//!
//! This module accumulates each slow fetch into a process-global log
//! during the resolve phase. The install pipeline calls
//! [`flush_summary`] after resolve to emit a single `tracing::warn!`
//! carrying the count and the slowest example, then resets the log.
//!
//! Storage mirrors the [`crate::dep_chain`] (in the binary crate)
//! pattern: `OnceLock<Mutex<Vec<Record>>>`, set per-install, drained
//! at the phase boundary. The `code = WARN_AUBE_SLOW_METADATA` field
//! on the emitted warning is unchanged — CI scripts and ndjson
//! reporters that branch on the code stay working.
//!
//! Per-event detail (label + elapsed) is *not* logged at any level by
//! default. `--loglevel debug` floods unrelated DEBUG sites and isn't
//! a usable escape hatch; if structured per-package telemetry is ever
//! needed, ndjson is the right vehicle.

use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone)]
struct Record {
    label: String,
    elapsed_ms: u64,
}

fn log() -> &'static Mutex<Vec<Record>> {
    static LOG: OnceLock<Mutex<Vec<Record>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Record that `label` took `elapsed_ms` and exceeded the configured
/// `fetchWarnTimeoutMs`. Called by the registry client's metadata
/// fetch path in place of a per-event `tracing::warn!`.
pub fn record(label: &str, elapsed_ms: u64) {
    if let Ok(mut guard) = log().lock() {
        guard.push(Record {
            label: label.to_string(),
            elapsed_ms,
        });
    }
}

/// Drain the accumulator and emit one summary `tracing::warn!` if any
/// records are present. Called once at the end of the resolve phase.
/// No-op when the log is empty (no slow fetches → no warning).
///
/// `threshold_ms` is the active `fetchWarnTimeoutMs` at flush time;
/// it's invariant across a single install run.
pub fn flush_summary(threshold_ms: u64) {
    let records = match log().lock() {
        Ok(mut guard) => std::mem::take(&mut *guard),
        Err(_) => return,
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-global state plus parallel test execution means two
    /// tests touching `LOG` can race (test A records, test B drains,
    /// test A's assertions fail). Serialize the whole module under one
    /// test entry point so the cases run deterministically.
    #[test]
    fn record_drain_and_flush_lifecycle() {
        // Empty case: flush with nothing accumulated leaves the log
        // empty and emits no warning.
        let _ = log().lock().map(|mut g| g.clear());
        flush_summary(10_000);
        assert!(log().lock().unwrap().is_empty());

        // Populated case: record two entries, flush drains them.
        record("packument a", 11_000);
        record("packument b", 13_500);
        assert_eq!(log().lock().unwrap().len(), 2);
        flush_summary(10_000);
        assert!(
            log().lock().unwrap().is_empty(),
            "flush must drain the accumulator",
        );

        // Idempotent: a second flush with nothing new is a no-op.
        flush_summary(10_000);
    }
}
