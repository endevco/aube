//! CI-mode progress: append-only `Progress:` line on a ~2s heartbeat,
//! plus a framed header and final summary. No spinners, no child rows,
//! no redraws — shape safe for GitHub Actions / plain pipes, where
//! cursor-control escapes get stripped and each animation frame would
//! otherwise land as its own log line.
//!
//! `CiState` owns the heartbeat thread; callers in `super` poke atomic
//! counters (`resolved`, `reused`, `downloaded`, `downloaded_bytes`) and
//! the heartbeat renders from those snapshots. See `super::InstallProgress`
//! for how TTY vs CI is selected and how these counters are updated.

use clx::style;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How often the CI heartbeat thread wakes to check whether to print a
/// progress line. Kept long enough that a 142-package fetch produces a
/// handful of lines, not a flood.
const CI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// Fallback width used when the terminal size can't be detected and
/// `$COLUMNS` isn't set. 80 is the historical terminal default and
/// renders cleanly even when the CI log viewer clips long lines.
const DEFAULT_BAR_WIDTH: usize = 80;

/// Hard floor on the bar width. Below this the label text won't fit
/// inside the bar and we'd start losing data.
const MIN_BAR_WIDTH: usize = 40;

/// Hard ceiling so a ridiculously wide terminal doesn't produce a
/// 200-column bar that the CI log viewer wraps awkwardly.
const MAX_BAR_WIDTH: usize = 120;

/// CI-mode shared state. Owns the heartbeat thread.
///
/// The status line has three moving parts: a phase counter (`[N/3]`
/// where 1=resolving, 2=fetching, 3=linking), the byte total for
/// downloaded tarballs, and an ASCII bar for `completed / resolved`
/// (where completed = reused + downloaded). One line, same shape each
/// time — reprinted only when something actually changed since the
/// previous line.
pub(super) struct CiState {
    phase: AtomicUsize,
    pub(super) resolved: AtomicUsize,
    pub(super) reused: AtomicUsize,
    pub(super) downloaded: AtomicUsize,
    pub(super) downloaded_bytes: AtomicU64,
    start: Instant,
    /// The last rendered line we actually wrote. Dedup on the rendered
    /// string (not the raw counter tuple) so changes that round to the
    /// same display — e.g. a byte delta that stays in the same MB
    /// bucket, or a phase change when phase isn't in the render — stay
    /// quiet instead of reprinting an identical line.
    last_printed: Mutex<String>,
    /// Whether the heartbeat has ever emitted the header + a progress
    /// line. Stays `false` for fast installs that finish before the
    /// first 2s tick — those stay completely silent, including in the
    /// final summary.
    shown: AtomicBool,
    done: AtomicBool,
    /// Live `InstallProgress` clone count. Incremented in `Clone`,
    /// decremented in `Drop`. When it hits zero the last clone is gone
    /// and we tear down. We can't use `Arc::strong_count` for this
    /// because the heartbeat thread owns its own strong `Arc<CiState>`
    /// for the entire run.
    pub(super) alive: AtomicUsize,
    /// Signals the heartbeat thread to wake early (phase change / stop).
    wake: Condvar,
    wake_lock: Mutex<()>,
    /// The heartbeat thread's join handle, taken by `stop()` so the
    /// thread is guaranteed to have exited before the final summary
    /// line is written — no stray tick can appear after `Done in …`.
    heartbeat: Mutex<Option<thread::JoinHandle<()>>>,
}

/// Detect the current terminal width for rendering the progress bar.
/// Prefers the `$COLUMNS` env var (set by most shells and honored by
/// GitHub Actions), then falls back to `console::Term::stderr().size()`
/// (works when stderr is a TTY), then a sensible 80-column default.
/// Clamped into `[MIN_BAR_WIDTH, MAX_BAR_WIDTH]`.
fn term_width() -> usize {
    let raw = std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .or_else(|| {
            let (_rows, cols) = console::Term::stderr().size();
            // `size()` returns (24, 80) as a hardcoded fallback when stderr
            // isn't a TTY — treat that as "unknown" and fall through.
            if cols == 0 { None } else { Some(cols as usize) }
        })
        .unwrap_or(DEFAULT_BAR_WIDTH);
    raw.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)
}

impl CiState {
    pub(super) fn new() -> Self {
        Self {
            phase: AtomicUsize::new(0),
            resolved: AtomicUsize::new(0),
            reused: AtomicUsize::new(0),
            downloaded: AtomicUsize::new(0),
            downloaded_bytes: AtomicU64::new(0),
            start: Instant::now(),
            last_printed: Mutex::new(String::new()),
            shown: AtomicBool::new(false),
            done: AtomicBool::new(false),
            alive: AtomicUsize::new(1),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
            heartbeat: Mutex::new(None),
        }
    }

    fn snapshot(&self) -> (usize, usize, usize, usize, u64) {
        (
            self.phase.load(Ordering::Relaxed),
            self.resolved.load(Ordering::Relaxed),
            self.reused.load(Ordering::Relaxed),
            self.downloaded.load(Ordering::Relaxed),
            self.downloaded_bytes.load(Ordering::Relaxed),
        )
    }

    fn render(snap: (usize, usize, usize, usize, u64)) -> String {
        let (phase, resolved, reused, downloaded, bytes) = snap;
        let completed = reused + downloaded;
        let phase_str = if phase > 0 {
            format!(" [{phase}/3]")
        } else {
            String::new()
        };
        let label = format!(
            "{completed}/{resolved} pkgs{phase_str} · {}",
            format_bytes(bytes)
        );
        render_bar_with_label(completed, resolved, term_width(), &label)
    }

    /// Framed header line. Emitted once, on the first heartbeat tick
    /// where there's something to show — so the CI log only grows an
    /// aube banner when an install is actually happening. Styling
    /// routes through `style::e*`, so `NO_COLOR` / `--no-color` drops
    /// the ANSI escapes automatically.
    fn render_header() -> String {
        let header_text = format!(
            "{} {} {}",
            style::emagenta("aube").bold(),
            style::edim(env!("CARGO_PKG_VERSION")),
            style::edim("by en.dev"),
        );
        render_centered_line(&header_text, term_width())
    }

    pub(super) fn spawn_heartbeat(state: &Arc<Self>) {
        let thread_state = state.clone();
        let handle = thread::spawn(move || {
            let state = thread_state;
            loop {
                let guard = state.wake_lock.lock().unwrap();
                // Re-check `done` *before* sleeping. `stop()` sets `done`
                // and then `notify_all()`s without holding `wake_lock`, so
                // a notification that races with the tick body would
                // otherwise be lost and the thread would sleep a full
                // `CI_HEARTBEAT_INTERVAL` before noticing shutdown.
                if state.done.load(Ordering::Relaxed) {
                    break;
                }
                let (guard, _timeout) = state
                    .wake
                    .wait_timeout(guard, CI_HEARTBEAT_INTERVAL)
                    .unwrap();
                drop(guard);
                if state.done.load(Ordering::Relaxed) {
                    break;
                }
                let snap = state.snapshot();
                // Don't make noise until an install is actually underway.
                // Until then there's nothing to bar-graph and no reason to
                // print the aube header — a no-op install should remain
                // completely silent.
                if snap.1 == 0 {
                    continue;
                }
                let line = Self::render(snap);
                let mut last = state.last_printed.lock().unwrap();
                if *last == line {
                    // Same rendered line as before — stay quiet.
                    continue;
                }
                *last = line.clone();
                drop(last);
                // First time we actually print, emit the framed header
                // above the bar so the CI log shows the aube banner.
                if !state.shown.swap(true, Ordering::Relaxed) {
                    let _ = writeln!(std::io::stderr(), "{}", Self::render_header());
                }
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        });
        *state.heartbeat.lock().unwrap() = Some(handle);
    }

    pub(super) fn set_phase(&self, phase: &str) {
        // Map the free-form phase label from `install::run` onto the fixed
        // `[N/3]` counter. Unknown labels leave the counter alone.
        let n = match phase {
            "resolving" => 1,
            "fetching" => 2,
            "linking" => 3,
            _ => return,
        };
        if self.phase.swap(n, Ordering::Relaxed) != n {
            self.wake.notify_all();
        }
    }

    /// Stop the heartbeat and (optionally) write the final summary.
    ///
    /// Crucially, we `join()` the heartbeat thread *before* writing the
    /// `Done in …` line so there's no race where a heartbeat tick lands
    /// after the summary. Idempotent via `done.swap`: the second caller
    /// (Drop after explicit `finish()`, etc.) finds `done == true` and
    /// returns without doing anything.
    pub(super) fn stop(&self, print_summary: bool) {
        if self.done.swap(true, Ordering::Relaxed) {
            return;
        }
        self.wake.notify_all();
        if let Some(handle) = self.heartbeat.lock().unwrap().take() {
            let _ = handle.join();
        }
        if !print_summary {
            return;
        }
        // If the heartbeat never printed anything (fast install, no-op,
        // or error before the first tick), stay completely silent — no
        // header, no final bar, no summary.
        if !self.shown.load(Ordering::Relaxed) {
            return;
        }
        // One snapshot for both the final bar and the summary stats —
        // taking two separate snapshots would let a concurrent
        // `FetchRow::drop` land between them and desync the numbers.
        let snap = self.snapshot();
        // Emit one final bar so CI logs end on a complete snapshot even
        // if the last heartbeat was skipped (fast install, or the last
        // tarball landed between ticks).
        let line = Self::render(snap);
        let mut last = self.last_printed.lock().unwrap();
        if *last != line {
            *last = line.clone();
            drop(last);
            let _ = writeln!(std::io::stderr(), "{line}");
        }
        // Final stats line: elapsed time plus the full resolve / reuse /
        // download breakdown, framed in the same `[ ]` block as the
        // header and the progress bar so the three lines read as one
        // coherent unit. Each segment is labeled so the numbers are
        // self-describing in a CI log weeks later without needing
        // context about aube's vocabulary.
        let (_phase, resolved, reused, downloaded, bytes) = snap;
        let elapsed = self.start.elapsed();
        let summary = format!(
            "{} {} · resolved {} · reused {} · downloaded {} ({})",
            style::egreen("✓"),
            style::edim(format_duration(elapsed)),
            resolved,
            reused,
            downloaded,
            format_bytes(bytes),
        );
        let _ = writeln!(
            std::io::stderr(),
            "{}",
            render_centered_line(&summary, term_width()),
        );
    }
}

/// Format an elapsed duration compactly: sub-second → `240ms`,
/// sub-minute → `4.0s`, otherwise `3m12s`. Matches how most package
/// managers render install time in their summary lines.
pub(super) fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        let total = d.as_secs();
        format!("{}m{:02}s", total / 60, total % 60)
    }
}

/// Render a plain-text line centered inside the same `[ ]` bracket
/// frame as the progress bar. Used for the header and the final
/// summary so all three lines share one consistent visual block.
///
/// `text` may contain ANSI escape sequences (for colored / dim /
/// bold styling); width is measured with `console::measure_text_width`
/// so escapes are excluded from the layout math. Text longer than the
/// inner width is returned as-is inside the brackets with no padding.
fn render_centered_line(text: &str, outer_width: usize) -> String {
    let outer_width = outer_width.max(MIN_BAR_WIDTH);
    let inner_width = outer_width.saturating_sub(2);
    let text_width = console::measure_text_width(text);
    if text_width >= inner_width {
        return format!("[{text}]");
    }
    let pad = inner_width - text_width;
    let left = pad / 2;
    let right = pad - left;
    format!("[{}{text}{}]", " ".repeat(left), " ".repeat(right))
}

/// Render a progress bar of `outer_width` characters with a label
/// centered inside it. The bar fills from the left with `#` up to
/// `current / total`, pads with `-`, and overlays `label` across the
/// middle positions — so the text stays visible whether the cursor is
/// in the filled or unfilled region.
///
/// Output shape (outer_width=60, 40% complete):
///   `[########################  183/239 pkgs · 13.8 MB  -----------]`
fn render_bar_with_label(current: usize, total: usize, outer_width: usize, label: &str) -> String {
    let outer_width = outer_width.max(MIN_BAR_WIDTH);
    // Two slots for the enclosing brackets.
    let inner_width = outer_width.saturating_sub(2);
    // Pad the label with a space on each side so it doesn't butt up
    // against the fill / empty characters — makes the text legible
    // inside a dense `#` run.
    let padded = format!(" {label} ");
    let padded_chars: Vec<char> = padded.chars().collect();
    let label_len = padded_chars.len().min(inner_width);
    let label_start = inner_width.saturating_sub(label_len) / 2;
    let label_end = label_start + label_len;

    let filled = current
        .checked_mul(inner_width)
        .and_then(|value| value.checked_div(total))
        .unwrap_or(0)
        .min(inner_width);

    let mut body = String::with_capacity(inner_width);
    for i in 0..inner_width {
        if i >= label_start && i < label_end {
            body.push(padded_chars[i - label_start]);
        } else if i < filled {
            body.push('#');
        } else {
            body.push('-');
        }
    }
    format!("[{body}]")
}

/// Format a byte count using the same SI units pnpm / npm show: `B`, `kB`,
/// `MB`, `GB`. Decimal (1000-based) because that's what every package
/// manager uses for on-the-wire sizes — closer to what the registry
/// `Content-Length` reports.
fn format_bytes(bytes: u64) -> String {
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
