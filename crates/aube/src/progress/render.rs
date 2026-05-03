//! Shared rendering primitives for the install progress UI.
//!
//! CI mode and TTY mode both want the same label content (cur/total
//! pkgs, downloaded / estimated bytes, transfer rate, ETA, phase
//! word) — only the bar and frame differ. The label assembly lives
//! here so a tweak to the segment order or styling lands in both
//! modes without drift.

use super::ci::Snap;
use clx::style;

/// Rough gzip compression ratio for npm tarballs. `dist.unpackedSize`
/// is what aube installs to disk, not what crosses the wire — typical
/// JS/TS code minified through gzip lands around 0.20-0.35×, with a
/// long tail. 0.30 is a middle-of-the-distribution constant that
/// keeps the estimate within ~30% on most installs without per-package
/// content-type tuning. Used solely for the `~13.8 MB` display
/// segment, never persisted to lockfiles or the store.
const TARBALL_COMPRESSION_RATIO: f64 = 0.30;

/// Build the full `<bar> <label>` line for one heartbeat tick.
/// Returns an empty string when the snapshot has nothing meaningful
/// to show — the heartbeat skips empty lines so a phase=0 snapshot
/// stays quiet instead of printing a blank.
pub(super) fn progress_line(snap: Snap, term_width: usize, bar_width: usize) -> String {
    if snap.phase == 0 {
        return String::new();
    }
    let label = label_for(snap);
    if label.is_empty() {
        return String::new();
    }
    let bar = bar_only(snap, bar_width);
    let _ = term_width; // reserved for future right-align/truncate logic
    format!("{bar} {label}")
}

/// The fixed-width left-aligned bar. Filled portion is green, empty
/// portion is dim. During resolving the bar is empty (the work hasn't
/// started yet); during linking it's effectively full.
pub(super) fn bar_only(snap: Snap, width: usize) -> String {
    let (numerator, denominator) = if snap.phase == 1 {
        (0, 1)
    } else {
        let completed = clamped_completed(snap);
        let denom = snap.resolved.max(1);
        (completed, denom)
    };
    let filled = numerator
        .checked_mul(width)
        .and_then(|v| v.checked_div(denominator))
        .unwrap_or(0)
        .min(width);
    let empty = width - filled;
    let fill = "█".repeat(filled);
    let empty = "░".repeat(empty);
    format!("{}{}", style::egreen(fill), style::edim(empty))
}

/// Phase-specific label content. Format:
///
/// * resolving: `N pkgs · resolving · ETA …`
/// * fetching:  `cur/total pkgs · 4.2 MB / ~13.8 MB · 1.4 MB/s · ETA 5s`
/// * linking:   `cur/total pkgs · 13.8 MB · linking`
fn label_for(snap: Snap) -> String {
    match snap.phase {
        1 => {
            // No bar to fill yet — show the running resolved count and
            // a placeholder ETA. `…` reads as "still figuring it out"
            // instead of blank.
            format!(
                "{} pkgs · {} · {}",
                style::ebold(snap.resolved),
                style::eyellow("resolving"),
                style::edim("ETA …"),
            )
        }
        2 => {
            let completed = clamped_completed(snap);
            let mut parts = Vec::with_capacity(4);
            parts.push(format!(
                "{}/{} pkgs",
                style::ebold(completed),
                style::ebold(snap.resolved),
            ));
            parts.push(bytes_segment(snap));
            if let Some(rate) = transfer_rate(snap) {
                parts.push(style::edim(format!("{}/s", format_bytes(rate))).to_string());
            }
            parts.push(eta_segment(snap, completed));
            parts.join(&format!(" {} ", style::edim("·")))
        }
        3 => {
            let completed = clamped_completed(snap);
            // Suppress the bytes segment when nothing was downloaded
            // (fully warm cache) — `0 B` would be visual noise.
            let mut parts = vec![format!(
                "{}/{} pkgs",
                style::ebold(completed),
                style::ebold(snap.resolved),
            )];
            if snap.bytes > 0 {
                parts.push(style::edim(format_bytes(snap.bytes)).to_string());
            }
            parts.push(style::ecyan("linking").to_string());
            parts.join(&format!(" {} ", style::edim("·")))
        }
        _ => String::new(),
    }
}

/// `4.2 MB` running, optionally `4.2 MB / ~13.8 MB` when the
/// estimated total is known. The estimate is `unpackedSize ×
/// TARBALL_COMPRESSION_RATIO` so it lands in the same units as the
/// running download counter. Drops the estimate suffix once the
/// running total has caught up — at that point we know the actual
/// total and the estimate is just noise.
fn bytes_segment(snap: Snap) -> String {
    let estimated_download = estimated_download_bytes(snap.estimated);
    if estimated_download > snap.bytes && snap.bytes > 0 {
        format!(
            "{} / ~{}",
            style::ebold(format_bytes(snap.bytes)),
            style::edim(format_bytes(estimated_download)),
        )
    } else if snap.bytes > 0 {
        style::ebold(format_bytes(snap.bytes)).to_string()
    } else if estimated_download > 0 {
        // Fetching just started but no bytes have landed — show the
        // estimated size so the user has a sense of total scope.
        format!("~{}", style::edim(format_bytes(estimated_download)),)
    } else {
        // No bytes, no estimate. Avoid emitting a stray `0 B` segment
        // that would just be visual noise.
        String::new()
    }
}

/// Convert a sum of `unpackedSize` values to an estimated tarball
/// (download) byte count. Pure helper so the call sites that build
/// the segment, and the eventual ETA-based-on-bytes calculation,
/// stay aligned on the same conversion.
fn estimated_download_bytes(unpacked: u64) -> u64 {
    if unpacked == 0 {
        return 0;
    }
    (unpacked as f64 * TARBALL_COMPRESSION_RATIO) as u64
}

/// `ETA 5s` once we have enough data to extrapolate; `ETA …`
/// otherwise. The ETA assumes the per-package work rate stays
/// constant from now on, which is good enough for "is this 5 seconds
/// or 5 minutes" UX.
fn eta_segment(snap: Snap, completed: usize) -> String {
    if completed == 0 || completed >= snap.resolved || snap.elapsed_ms == 0 {
        return style::edim("ETA …").to_string();
    }
    let remaining = snap.resolved - completed;
    let eta_ms = snap.elapsed_ms.saturating_mul(remaining as u64) / completed as u64;
    style::edim(format!(
        "ETA {}",
        format_duration(std::time::Duration::from_millis(eta_ms))
    ))
    .to_string()
}

/// Bytes-per-second over the fetching window only. Returns `None`
/// when no bytes have landed or the fetch window hasn't opened yet —
/// the rate segment is then dropped from the label.
fn transfer_rate(snap: Snap) -> Option<u64> {
    if snap.bytes == 0 || snap.fetch_elapsed_ms == 0 {
        return None;
    }
    Some(snap.bytes.saturating_mul(1000) / snap.fetch_elapsed_ms)
}

/// Defensive clamp: numerator can never exceed denominator. The two
/// known sources of overrun (the catch-up bookkeeping bug and
/// streamed-then-pruned packages) are fixed at their roots, but if a
/// new code path regresses we want the display to stay sane and the
/// `WARN_AUBE_PROGRESS_OVERFLOW` warning to fire.
fn clamped_completed(snap: Snap) -> usize {
    let raw = snap.reused + snap.downloaded;
    if raw > snap.resolved && snap.resolved > 0 {
        // The warning fires on the rendering path so it's emitted at
        // most once per heartbeat, not once per increment. The
        // tracing layer already deduplicates identical events.
        tracing::warn!(
            code = aube_codes::warnings::WARN_AUBE_PROGRESS_OVERFLOW,
            raw_completed = raw,
            resolved = snap.resolved,
            "progress numerator exceeded resolved-package denominator; clamping display"
        );
    }
    raw.min(snap.resolved)
}

/// Format a byte count using the same SI units pnpm / npm show: `B`,
/// `kB`, `MB`, `GB`. Decimal (1000-based) because that's what every
/// package manager uses for on-the-wire sizes.
pub(super) fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1_000;
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} kB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format an elapsed duration compactly. Mirrors `ci::format_duration`
/// to avoid a cross-module call from the inline summary path; kept
/// as a single function so future tweaks land in one place.
pub(super) fn format_duration(d: std::time::Duration) -> String {
    super::ci::format_duration(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(phase: usize, resolved: usize, completed: usize, bytes: u64, estimated: u64) -> Snap {
        Snap {
            phase,
            resolved,
            reused: completed,
            downloaded: 0,
            bytes,
            estimated,
            elapsed_ms: 5_000,
            fetch_elapsed_ms: 3_000,
        }
    }

    fn strip_ansi(s: &str) -> String {
        // Strip simple SGR sequences for assertion stability (env-dependent
        // colors_enabled would otherwise break expected-string tests).
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for esc_c in chars.by_ref() {
                    if esc_c.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn resolving_phase_shows_count_and_eta_placeholder() {
        let line = strip_ansi(&progress_line(snap(1, 89, 0, 0, 0), 80, 15));
        assert!(line.contains("89 pkgs"), "got: {line}");
        assert!(line.contains("resolving"), "got: {line}");
        assert!(line.contains("ETA …"), "got: {line}");
    }

    #[test]
    fn fetching_phase_shows_bytes_and_estimate() {
        // Estimated unpacked = 46 MB → 0.30× = ~13.8 MB compressed,
        // which exceeds the 4.2 MB downloaded so far so the
        // `/ ~estimated` segment renders.
        let line = strip_ansi(&progress_line(
            snap(2, 142, 23, 4_200_000, 46_000_000),
            80,
            15,
        ));
        assert!(line.contains("23/142 pkgs"), "got: {line}");
        assert!(line.contains("4.2 MB"), "got: {line}");
        assert!(line.contains("~13.8 MB"), "got: {line}");
    }

    #[test]
    fn fetching_phase_drops_estimate_when_running_exceeds_it() {
        // Estimated unpacked × 0.30 (≈ 4.1 MB) is below the running
        // 4.2 MB, so the `/ ~estimated` segment is dropped — at that
        // point the running figure is the better number anyway.
        let line = strip_ansi(&progress_line(
            snap(2, 142, 23, 4_200_000, 13_800_000),
            80,
            15,
        ));
        assert!(line.contains("4.2 MB"), "got: {line}");
        assert!(!line.contains("~"), "estimate should drop: {line}");
    }

    #[test]
    fn linking_phase_drops_rate_and_eta() {
        let line = strip_ansi(&progress_line(
            snap(3, 142, 142, 13_800_000, 13_800_000),
            80,
            15,
        ));
        assert!(line.contains("142/142"), "got: {line}");
        assert!(line.contains("linking"), "got: {line}");
        assert!(!line.contains("MB/s"), "rate must drop in linking: {line}");
        assert!(!line.contains("ETA"), "eta must drop in linking: {line}");
    }

    #[test]
    fn clamps_overflow_to_resolved() {
        let mut s = snap(2, 5, 7, 0, 0);
        s.reused = 7;
        let line = strip_ansi(&progress_line(s, 80, 15));
        assert!(line.contains("5/5 pkgs"), "got: {line}");
    }
}
