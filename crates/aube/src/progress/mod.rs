//! Install-time progress UI built on top of `clx::progress`.
//!
//! Two modes live behind one API so call sites in `install::run` stay the same:
//!
//! * **TTY** — an animated clx bar: a root row with overall counts plus
//!   transient child rows for in-flight tarball fetches. Children auto-hide
//!   on completion via `ProgressJobDoneBehavior::Hide`, so the display stays
//!   bounded even though the pipeline resolves → fetches → links concurrently.
//! * **CI** — append-only lines safe for GitHub Actions / plain pipes: a
//!   single repeating pnpm-style `Progress:` line emitted on a ~2s
//!   heartbeat, showing `resolved` / `reused` / `downloaded` plus the
//!   byte total for the downloaded set. The heartbeat only prints when
//!   something actually advanced, so a fast install stays quiet and a
//!   slow one shows exactly *why* it's slow (network-bound vs
//!   linker-bound). No phase noise, no child rows, no redraws.
//!
//! `try_new` picks the mode: TTY on an interactive stderr, CI on a pipe,
//! or CI when `is_ci::cached()` detects a known CI environment (Buildkite,
//! GitHub Actions, etc.) even if stderr looks like a TTY — those systems
//! allocate a PTY so tools emit colors, but their log capturers strip
//! cursor-control escapes and each animation frame lands as its own log
//! line. CI mode's ~2s heartbeat is the right shape for that.
//! It returns `None` only when clx has been forced into text mode
//! (`--silent`, `-v`, `--reporter=append-only|ndjson`) — those modes own
//! their own output and we stay out of the way.

mod ci;
mod render;

use ci::{CiState, format_duration};
use clx::progress::{
    ProgressJob, ProgressJobBuilder, ProgressJobDoneBehavior, ProgressOutput, ProgressStatus,
};
use clx::style;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// Cap on the number of simultaneously-visible per-package fetch rows
/// in TTY mode. Bursts above this are collapsed into a single overflow
/// row labeled "N more packages…" so the animated display stays
/// bounded on installs that fan out hundreds of tarball fetches at
/// once.
const TTY_MAX_VISIBLE_FETCH_ROWS: usize = 5;

fn overflow_fetch_label(count: usize) -> String {
    let word = pluralizer::pluralize("package", count as isize, false);
    format!("{count} more {word}…")
}

/// Build the standard `aube VERSION by en.dev · <msg>` one-line
/// header used by the no-op and fast-mode summaries. Centralizes the
/// header shape so the install-finished, already-up-to-date, and
/// fast-mode-summary paths all read consistently.
fn aube_prefix_line(msg: &str) -> String {
    format!(
        "{} {} {} {} {msg}",
        style::emagenta("aube").bold(),
        style::edim(crate::version::VERSION.as_str()),
        style::edim("by en.dev"),
        style::edim("·"),
    )
}

/// Install-time progress UI. Cheap to clone (internally `Arc`).
pub struct InstallProgress {
    mode: Mode,
}

#[derive(Clone)]
enum Mode {
    Tty {
        root: Arc<ProgressJob>,
        /// Our own mirror of the denominator so `inc_total` can atomically
        /// fetch-add without racing a concurrent reader/writer through clx's
        /// separate `overall_progress()` / `progress_total()` calls.
        total: Arc<AtomicUsize>,
        /// Mirror of cumulative reused-package count so the TTY bar can
        /// recompute the live numerator without taking a round-trip
        /// through clx's progress accessors.
        reused: Arc<AtomicUsize>,
        /// Mirror of cumulative downloaded-package count for the same
        /// reason.
        downloaded: Arc<AtomicUsize>,
        /// Phase number: 0=init, 1=resolving, 2=fetching, 3=linking. Used
        /// by the rate / ETA props to gate display to the fetching
        /// window and switch to the `linking` label in phase 3.
        phase_num: Arc<AtomicUsize>,
        /// Cumulative downloaded bytes. Fed into the transfer-rate
        /// calculation displayed in the TTY bar's `rate` prop.
        downloaded_bytes: Arc<AtomicU64>,
        /// Running sum of `dist.unpackedSize` from packuments seen
        /// during the streaming resolve. `0` on the lockfile fast path.
        /// The bar's `bytes` prop renders `4.2 MB / ~13.8 MB` when this
        /// is set; otherwise just `4.2 MB`.
        estimated_bytes: Arc<AtomicU64>,
        /// Captured the first time `set_phase("fetching")` is called.
        /// Used as the rate denominator so the displayed throughput
        /// measures the fetch window only, not `bytes / (resolve_time +
        /// fetch_time)`.
        fetch_start: Arc<OnceLock<Instant>>,
        /// Wall-clock start of the install, used by `eta` recomputation
        /// to derive elapsed time without re-querying clx.
        install_start: Arc<Instant>,
        /// Bounded visible-fetch-row bookkeeping. `visible` is the count
        /// of live per-package child rows (capped at
        /// `TTY_MAX_VISIBLE_FETCH_ROWS`); `overflow` is the count of
        /// in-flight fetches folded into the single overflow row. The
        /// overflow row itself is lazily added on first overspill and
        /// retained for the rest of the install.
        fetch_state: Arc<Mutex<FetchState>>,
    },
    Ci(Arc<CiState>),
}

struct FetchState {
    visible: usize,
    overflow: usize,
    overflow_row: Option<Arc<ProgressJob>>,
}

impl Clone for InstallProgress {
    /// CI mode tracks its own "alive clones" refcount instead of relying on
    /// `Arc::strong_count`, because the heartbeat thread owns an `Arc<CiState>`
    /// for the entire run and would otherwise pin `strong_count ≥ 2` — defeating
    /// the `== 1` shutdown check in `Drop`.
    fn clone(&self) -> Self {
        if let Mode::Ci(s) = &self.mode {
            s.alive.fetch_add(1, Ordering::Relaxed);
        }
        Self {
            mode: self.mode.clone(),
        }
    }
}

impl InstallProgress {
    /// Construct a new install progress UI, or `None` if progress should be
    /// disabled (clx text mode — i.e. `--silent`, `-v`, or a line-oriented
    /// reporter that owns its own output).
    pub fn try_new() -> Option<Self> {
        if clx::progress::output() == ProgressOutput::Text {
            return None;
        }
        // Prefer CI mode whenever we're in a known CI environment
        // (`is_ci` checks `CI`, `BUILDKITE`, `GITHUB_ACTIONS`, and friends),
        // even when stderr looks like a TTY. Most CI runners allocate a
        // PTY so child processes emit colors, which makes
        // `is_terminal()` return true — but the log capturer then strips
        // cursor-control escapes and each animation frame becomes its
        // own log line, flooding the build log with thousands of
        // near-duplicate spinner rows. CI mode's 2s heartbeat is the
        // right shape there.
        if std::io::stderr().is_terminal() && !is_ci::cached() {
            Some(Self::new_tty())
        } else {
            Some(Self::new_ci())
        }
    }

    fn new_tty() -> Self {
        // Colored header: magenta bold "aube", dim version, dim "by en.dev".
        // Mirrors the `mise VERSION by @jdx` / `hk VERSION by @jdx` convention
        // for visual parity across the trio.
        let header = format!(
            "{} {} {}",
            style::emagenta("aube").bold(),
            style::edim(crate::version::VERSION.as_str()),
            style::edim("by en.dev"),
        );
        // Layout: header, animated bar, cur/total, optional bytes
        // segment (running download, with `/ ~estimated` when
        // available), phase-gated rate, ETA. Mirrors the CI-mode
        // label segment-for-segment so both modes show the same
        // information.
        let root = ProgressJobBuilder::new()
            .body(
                "{{aube}}{{phase}}  {{progress_bar(flex=true)}} {{cur}}/{{total}}{{bytes}}{{rate}}{{eta}}",
            )
            .body_text(Some(
                "{{aube}}{{phase}} {{cur}}/{{total}}{{bytes}}{{rate}}{{eta}}",
            ))
            .prop("aube", &header)
            .prop("phase", "")
            .prop("bytes", "")
            .prop("rate", "")
            .prop("eta", "")
            .progress_current(0)
            .progress_total(0)
            .on_done(ProgressJobDoneBehavior::Hide)
            .start();
        Self {
            mode: Mode::Tty {
                root,
                total: Arc::new(AtomicUsize::new(0)),
                reused: Arc::new(AtomicUsize::new(0)),
                downloaded: Arc::new(AtomicUsize::new(0)),
                phase_num: Arc::new(AtomicUsize::new(0)),
                downloaded_bytes: Arc::new(AtomicU64::new(0)),
                estimated_bytes: Arc::new(AtomicU64::new(0)),
                fetch_start: Arc::new(OnceLock::new()),
                install_start: Arc::new(Instant::now()),
                fetch_state: Arc::new(Mutex::new(FetchState {
                    visible: 0,
                    overflow: 0,
                    overflow_row: None,
                })),
            },
        }
    }

    fn new_ci() -> Self {
        // Header + first progress line are deferred to the first heartbeat
        // tick (see `CiState::spawn_heartbeat`). A fast install that
        // finishes before the 2s heartbeat interval therefore prints
        // nothing at all — no header, no bar, no summary — which is what
        // we want for the no-op and near-no-op cases.
        let state = Arc::new(CiState::new());
        CiState::spawn_heartbeat(&state);
        Self {
            mode: Mode::Ci(state),
        }
    }

    /// Set the total (`resolved`) package count. Safe to call repeatedly.
    pub fn set_total(&self, total: usize) {
        match &self.mode {
            Mode::Tty { root, total: t, .. } => {
                t.store(total, Ordering::Relaxed);
                root.progress_total(total);
                self.refresh_eta();
            }
            Mode::Ci(s) => {
                s.resolved.store(total, Ordering::Relaxed);
            }
        }
    }

    /// Atomically bump the total (`resolved`) by `n` packages.
    pub fn inc_total(&self, n: usize) {
        match &self.mode {
            Mode::Tty { root, total, .. } => {
                let new_total = total.fetch_add(n, Ordering::Relaxed) + n;
                root.progress_total(new_total);
                self.refresh_eta();
            }
            Mode::Ci(s) => {
                s.resolved.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    /// Add `n` bytes to the running estimated-total-download counter.
    /// Fed from `dist.unpackedSize` as resolver streams in packuments.
    /// Surfaces in the bar's `bytes` segment as the `/ ~13.8 MB`
    /// suffix so users have a sense of total install scope before the
    /// fetch finishes. No-op when the packument lacks the field.
    pub fn inc_estimated_bytes(&self, n: u64) {
        match &self.mode {
            Mode::Tty {
                estimated_bytes, ..
            } => {
                estimated_bytes.fetch_add(n, Ordering::Relaxed);
                self.refresh_bytes_segment();
            }
            Mode::Ci(s) => {
                s.estimated_bytes.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    /// Set the phase label shown to the right of the header (e.g. "resolving",
    /// "fetching", "linking"). Empty string clears it.
    pub fn set_phase(&self, phase: &str) {
        match &self.mode {
            Mode::Tty {
                root,
                phase_num,
                fetch_start,
                ..
            } => {
                if phase.is_empty() {
                    root.prop("phase", "");
                } else {
                    let colored_phase = match phase {
                        "resolving" => style::eyellow(phase).to_string(),
                        "linking" => style::ecyan(phase).to_string(),
                        _ => style::edim(phase).to_string(),
                    };
                    root.prop("phase", &format!(" {} {}", style::edim("—"), colored_phase));
                }
                let n = match phase {
                    "resolving" => 1,
                    "fetching" => 2,
                    "linking" => 3,
                    _ => 0,
                };
                phase_num.store(n, Ordering::Relaxed);
                if n == 2 {
                    // Seed the rate denominator on the fetching transition.
                    // First-writer-wins; repeated calls are no-ops.
                    let _ = fetch_start.set(Instant::now());
                } else if n == 3 {
                    // Linking phase: rate / ETA aren't meaningful — the
                    // network's done, the linker work is dominated by
                    // filesystem ops on a fixed package count. Clear
                    // both props so the "linking" word reads cleanly.
                    root.prop("rate", "");
                    root.prop("eta", "");
                }
                self.refresh_bytes_segment();
                self.refresh_rate();
                self.refresh_eta();
            }
            Mode::Ci(s) => s.set_phase(phase),
        }
    }

    /// Credit `n` packages to the `reused` bucket: served from the global
    /// content-addressed store (cache hit) or materialized from a local
    /// `file:` / `link:` source — anything that didn't touch the network.
    pub fn inc_reused(&self, n: usize) {
        match &self.mode {
            Mode::Tty { root, reused, .. } => {
                reused.fetch_add(n, Ordering::Relaxed);
                root.increment(n);
                self.refresh_eta();
            }
            Mode::Ci(s) => {
                s.reused.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    /// Credit `bytes` to the downloaded-bytes total. Called once per
    /// tarball after the registry fetch completes, on top of the per-package
    /// increment that `FetchRow::drop` contributes to the downloaded count.
    ///
    /// In TTY mode this refreshes the bytes / rate / ETA props on the
    /// animated bar. In CI mode the heartbeat re-renders from the
    /// cumulative byte counter on each tick; here we just bump that
    /// counter.
    pub fn inc_downloaded_bytes(&self, bytes: u64) {
        match &self.mode {
            Mode::Tty {
                downloaded_bytes, ..
            } => {
                downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
                self.refresh_bytes_segment();
                self.refresh_rate();
            }
            Mode::Ci(s) => {
                s.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
        }
    }

    /// TTY-only: rebuild the `bytes` prop from the current downloaded /
    /// estimated counters. Picks shape based on what we know:
    ///   `4.2 MB / ~13.8 MB` when both are available, `4.2 MB` when
    ///   only the running total is, `~13.8 MB` when only the estimate
    ///   is, empty otherwise. CI mode does this inside the heartbeat
    ///   render — no per-call refresh needed there.
    fn refresh_bytes_segment(&self) {
        let Mode::Tty {
            root,
            downloaded_bytes,
            estimated_bytes,
            phase_num,
            ..
        } = &self.mode
        else {
            return;
        };
        let bytes = downloaded_bytes.load(Ordering::Relaxed);
        let estimated = estimated_bytes.load(Ordering::Relaxed);
        let phase = phase_num.load(Ordering::Relaxed);
        // The bytes segment only carries useful information from
        // fetching onward. Hide it in resolving — there's nothing
        // downloaded yet — and during phase 0 (pre-progress).
        if phase < 2 || (bytes == 0 && estimated == 0) {
            root.prop("bytes", "");
            return;
        }
        let segment = if estimated > bytes && bytes > 0 {
            format!(
                " · {} {} {}",
                style::ebold(render::format_bytes(bytes)),
                style::edim("/"),
                style::edim(format!("~{}", render::format_bytes(estimated))),
            )
        } else if bytes > 0 {
            format!(" · {}", style::ebold(render::format_bytes(bytes)))
        } else {
            // bytes == 0, estimated > 0
            format!(
                " · {}",
                style::edim(format!("~{}", render::format_bytes(estimated)))
            )
        };
        root.prop("bytes", &segment);
    }

    /// TTY-only: rebuild the `rate` prop. Active during fetching only;
    /// cleared in resolving (no data) and linking (network done).
    fn refresh_rate(&self) {
        let Mode::Tty {
            root,
            phase_num,
            downloaded_bytes,
            fetch_start,
            ..
        } = &self.mode
        else {
            return;
        };
        if phase_num.load(Ordering::Relaxed) != 2 {
            root.prop("rate", "");
            return;
        }
        let bytes = downloaded_bytes.load(Ordering::Relaxed);
        let Some(start) = fetch_start.get() else {
            return;
        };
        let elapsed_ms = start.elapsed().as_millis() as u64;
        if bytes == 0 || elapsed_ms == 0 {
            root.prop("rate", "");
            return;
        }
        let rate = bytes.saturating_mul(1000) / elapsed_ms;
        root.prop(
            "rate",
            &format!(
                " · {}",
                style::edim(format!("{}/s", render::format_bytes(rate)))
            ),
        );
    }

    /// TTY-only: rebuild the `eta` prop. `ETA …` while we don't have
    /// enough data to extrapolate; `ETA Xs` once we do.
    fn refresh_eta(&self) {
        let Mode::Tty {
            root,
            total,
            reused,
            downloaded,
            phase_num,
            install_start,
            ..
        } = &self.mode
        else {
            return;
        };
        let phase = phase_num.load(Ordering::Relaxed);
        // Only show ETA in resolving + fetching. Linking is fast and
        // bounded — adding an ETA there would just flap around 0s.
        if phase == 0 || phase == 3 {
            root.prop("eta", "");
            return;
        }
        let total_n = total.load(Ordering::Relaxed);
        let completed =
            (reused.load(Ordering::Relaxed) + downloaded.load(Ordering::Relaxed)).min(total_n);
        let elapsed_ms = install_start.elapsed().as_millis() as u64;
        if completed == 0 || completed >= total_n || total_n == 0 || elapsed_ms == 0 {
            root.prop("eta", &format!(" · {}", style::edim("ETA …")));
            return;
        }
        let remaining = total_n - completed;
        let eta_ms = elapsed_ms.saturating_mul(remaining as u64) / completed as u64;
        let eta_str = format_duration(Duration::from_millis(eta_ms));
        root.prop(
            "eta",
            &format!(" · {}", style::edim(format!("ETA {eta_str}"))),
        );
    }

    /// Add a transient child row for an in-flight tarball fetch. Drop the
    /// returned `FetchRow` when the fetch completes to remove the row and
    /// bump the `downloaded` bucket.
    ///
    /// In CI mode this creates no child row — the returned value just
    /// increments the `downloaded` counter on drop so the heartbeat advances.
    pub fn start_fetch(&self, name: &str, version: &str) -> FetchRow {
        match &self.mode {
            Mode::Tty {
                root,
                fetch_state,
                downloaded,
                ..
            } => {
                let mut st = fetch_state.lock().unwrap();
                if st.visible < TTY_MAX_VISIBLE_FETCH_ROWS {
                    st.visible += 1;
                    drop(st);
                    let child = ProgressJobBuilder::new()
                        .body("  {{spinner()}} {{label | flex}}")
                        .body_text(None::<String>)
                        .prop("label", &format!("{name}@{version}"))
                        .status(ProgressStatus::Running)
                        .on_done(ProgressJobDoneBehavior::Hide)
                        .build();
                    let child = root.add(child);
                    return FetchRow {
                        inner: FetchRowInner::Tty {
                            child,
                            root: Arc::downgrade(root),
                            fetch_state: Arc::downgrade(fetch_state),
                            downloaded: Arc::downgrade(downloaded),
                            visible: true,
                        },
                        completed: false,
                    };
                }
                // Over the visible-row cap: fold this fetch into the
                // single "N more packages…" overflow row. Lazily
                // create the row on first overspill; it persists for
                // the rest of the install (no promotion back to
                // visible — avoids row churn on flappy fetch queues).
                st.overflow += 1;
                if st.overflow_row.is_none() {
                    let row = ProgressJobBuilder::new()
                        .body("  {{spinner()}} {{label | flex}}")
                        .body_text(None::<String>)
                        .prop("label", &overflow_fetch_label(st.overflow))
                        .status(ProgressStatus::Running)
                        .on_done(ProgressJobDoneBehavior::Hide)
                        .build();
                    st.overflow_row = Some(root.add(row));
                } else if let Some(row) = &st.overflow_row {
                    row.prop("label", &overflow_fetch_label(st.overflow));
                }
                FetchRow {
                    inner: FetchRowInner::Tty {
                        child: st.overflow_row.as_ref().unwrap().clone(),
                        root: Arc::downgrade(root),
                        fetch_state: Arc::downgrade(fetch_state),
                        downloaded: Arc::downgrade(downloaded),
                        visible: false,
                    },
                    completed: false,
                }
            }
            Mode::Ci(s) => FetchRow {
                inner: FetchRowInner::Ci(Arc::downgrade(s)),
                completed: false,
            },
        }
    }

    /// Finalize and clear the progress display. TTY mode leaves no output
    /// behind. CI mode blocks until the heartbeat thread has actually
    /// stopped so no stray tick can appear after this returns, and
    /// optionally writes the final framed `[ ✓ … ]` status line.
    /// Idempotent.
    ///
    /// `print_ci_summary`: set to `false` when a later call site will
    /// print its own end-of-install line (so the main install path
    /// doesn't double up with [`print_install_summary`]). Set to `true`
    /// for early-return paths (`--lockfile-only`, drift check) that
    /// want the framed summary to remain the end of CI log output.
    pub fn finish(&self, print_ci_summary: bool) {
        match &self.mode {
            Mode::Tty { root, .. } => {
                root.set_status(ProgressStatus::Done);
                clx::progress::stop_clear();
            }
            Mode::Ci(s) => s.stop(print_ci_summary),
        }
    }

    /// Emit the post-install summary line after the progress display has
    /// been torn down. Two shapes:
    ///
    /// * `linked > 0` — `aube VERSION by en.dev · ✓ installed N packages
    ///   in Xs`, TTY-only (CI mode prints its own framed `✓` summary
    ///   from the heartbeat's final tick).
    /// * `linked == 0 && top_level_linked == 0` — `Already up to date`
    ///   (matches pnpm), printed in both TTY and CI modes so cache-only
    ///   runs confirm nothing needed doing. Stays silent in reporter
    ///   modes where `prog_ref` is `None`.
    ///
    /// The `top_level_linked` guard distinguishes a true no-op from the
    /// `rm -rf node_modules && aube install` case where the global store
    /// was warm (so `packages_linked` is 0) but every top-level symlink
    /// had to be recreated — that's not "up to date" from the user's
    /// perspective.
    ///
    /// **Safety:** must be called *after* [`InstallProgress::finish`]. The
    /// write goes straight to stderr without routing through
    /// `PausingWriter` or `with_terminal_lock`, which is only safe once
    /// `finish()` has synchronously stopped the render loop via
    /// `stop_clear()`. A new call site placed before `finish()` would
    /// silently race the animated display.
    pub fn print_install_summary(
        &self,
        linked: usize,
        top_level_linked: usize,
        total_packages: usize,
        elapsed: Duration,
    ) {
        if linked == 0 && top_level_linked == 0 {
            let msg = if total_packages == 0 {
                "Already up to date".to_string()
            } else {
                format!(
                    "Already up to date ({})",
                    pluralizer::pluralize("package", total_packages as isize, true)
                )
            };
            // Same single-line `aube VERSION by en.dev · ✓ msg` shape
            // for both TTY and CI modes. CI mode's heartbeat may have
            // emitted intermediate progress lines above this; the
            // self-identifying summary still reads fine as the closing
            // line of a multi-line CI block.
            let line = aube_prefix_line(&style::egreen(&msg).bold().to_string());
            let _ = writeln!(std::io::stderr(), "{line}");
            return;
        }
        if linked == 0 {
            return;
        }
        // CI mode prints its own multi-segment summary from the
        // heartbeat's final tick (resolve / reused / downloaded
        // breakdown). For fast installs that never hit the heartbeat,
        // print the single-line summary here so the user still sees
        // a confirmation. TTY mode always prints here.
        let needs_summary = match &self.mode {
            Mode::Tty { .. } => true,
            Mode::Ci(s) => !s.shown.load(Ordering::Relaxed),
        };
        if !needs_summary {
            return;
        }
        let msg = format!(
            "✓ installed {} in {}",
            pluralizer::pluralize("package", linked as isize, true),
            format_duration(elapsed)
        );
        let line = aube_prefix_line(&style::egreen(msg).bold().to_string());
        let _ = writeln!(std::io::stderr(), "{line}");
    }
}

impl Drop for InstallProgress {
    /// Safety net: if `install::run` bails through `?` without reaching
    /// `finish()` (flaky network, lockfile parse error, linker failure, …)
    /// the renderer would otherwise be left running. We only tear down
    /// when *this* instance is the last live clone, not when an earlier
    /// clone (e.g. the one handed to the fresh-resolve fetch coordinator)
    /// drops while the install is still in flight.
    ///
    /// CI mode can't use `Arc::strong_count` for this check because the
    /// heartbeat thread holds its own clone of `Arc<CiState>` for the
    /// entire run. Instead, it tracks the live-clone count in a separate
    /// `CiState::alive` atomic, incremented in `Clone` and decremented
    /// here. Error paths drop without printing the `Done in Xs` summary
    /// — the heartbeat still gets joined so no stray tick escapes.
    fn drop(&mut self) {
        match &self.mode {
            Mode::Tty { root, .. } => {
                if Arc::strong_count(root) == 1 {
                    root.set_status(ProgressStatus::Done);
                    clx::progress::stop_clear();
                }
            }
            Mode::Ci(s) => {
                if s.alive.fetch_sub(1, Ordering::Relaxed) == 1 {
                    s.stop(false);
                }
            }
        }
    }
}

/// A single in-flight fetch row. Dropping completes it (hide + bump the
/// download counter in TTY mode; download-counter-only in CI mode).
pub struct FetchRow {
    inner: FetchRowInner,
    completed: bool,
}

enum FetchRowInner {
    Tty {
        child: Arc<ProgressJob>,
        /// Weak ref so orphaned rows (e.g. spawned fetch tasks still in flight
        /// after an error short-circuits the install) don't hold the root job
        /// alive and block `InstallProgress::Drop` from clearing the display.
        root: Weak<ProgressJob>,
        /// Weak ref to the shared fetch bookkeeping so drop can
        /// decrement visible/overflow counters and refresh the
        /// overflow row label without pinning it alive.
        fetch_state: Weak<Mutex<FetchState>>,
        /// Weak ref to the TTY mirror of the downloaded counter so
        /// drop can bump it without holding the root alive. Mirrors
        /// CI mode's `downloaded` counter — used by the ETA / clamp
        /// computations in the bar render.
        downloaded: Weak<AtomicUsize>,
        /// Whether this row occupies one of the `TTY_MAX_VISIBLE_FETCH_ROWS`
        /// visible slots. Overflow rows share a single child job; they
        /// only bump the overflow counter and the label on drop.
        visible: bool,
    },
    /// Matches the TTY variant's weak-ref discipline: orphaned CI fetch
    /// rows shouldn't prevent `CiState` from being dropped after the
    /// last `InstallProgress` clone is gone.
    Ci(Weak<CiState>),
}

impl FetchRow {
    fn finish_inner(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        match &self.inner {
            FetchRowInner::Tty {
                child,
                root,
                fetch_state,
                downloaded,
                visible,
            } => {
                if let Some(root) = root.upgrade() {
                    root.increment(1);
                }
                if let Some(d) = downloaded.upgrade() {
                    d.fetch_add(1, Ordering::Relaxed);
                }
                if *visible {
                    child.set_status(ProgressStatus::Done);
                    if let Some(st) = fetch_state.upgrade() {
                        let mut st = st.lock().unwrap();
                        if st.visible > 0 {
                            st.visible -= 1;
                        }
                    }
                } else if let Some(st) = fetch_state.upgrade() {
                    let mut st = st.lock().unwrap();
                    if st.overflow > 0 {
                        st.overflow -= 1;
                    }
                    if st.overflow == 0 {
                        if let Some(row) = st.overflow_row.take() {
                            row.set_status(ProgressStatus::Done);
                        }
                    } else if let Some(row) = &st.overflow_row {
                        row.prop("label", &overflow_fetch_label(st.overflow));
                    }
                }
            }
            FetchRowInner::Ci(weak) => {
                if let Some(s) = weak.upgrade() {
                    s.downloaded.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

impl Drop for FetchRow {
    fn drop(&mut self) {
        self.finish_inner();
    }
}

/// A `tracing_subscriber` writer that coordinates with clx so log
/// events don't get overwritten by the animated progress display.
///
/// Default `std::io::stderr` writes race the render loop: a `warn!`
/// emitted mid-frame lands in the middle of a redraw, leaving the bar
/// fragments smeared across the log line (and the log line smeared
/// across the bar) until the next tick repaints over it.
///
/// `PausingWriter` fixes this by buffering each event in-memory and
/// flushing the whole buffer atomically at the end of the event:
///
///   1. `make_writer` returns a fresh buffered guard — one per event.
///   2. The fmt layer writes the formatted record (level prefix,
///      message, fields, trailing newline) into the guard's buffer.
///   3. On drop, the guard takes clx's terminal lock, pauses the
///      render loop, writes the whole buffer in a single `write_all`,
///      then resumes.
///
/// Holding the terminal lock across the pause/write/resume window
/// serializes against `ProgressJob::println` and the render thread,
/// so neither can interleave half a frame mid-event. In text mode
/// (`-v`, `--silent`, append-only, ndjson) the progress display
/// isn't running; pause/resume become benign no-ops and the event
/// still flushes cleanly.
/// Print a message to stderr safely while the install progress bar
/// may be active. Direct `eprintln!` during an active bar smears
/// output across frames (bar paints over half the message, next tick
/// repaints over what remains). Use this for warnings that need to
/// surface mid-install like peer-dep errors, allowBuilds policy
/// warnings, retry notifications, etc. If no bar is up, degenerates
/// to a plain stderr write. Trailing newline is appended. Call sites
/// that already hold a bar handle can use ProgressJob::println
/// instead, but this works without one.
pub fn safe_eprintln(msg: &str) {
    use std::io::Write;
    let was_paused = clx::progress::is_paused();
    if !was_paused {
        clx::progress::pause();
    }
    let _: () = clx::progress::with_terminal_lock(|| {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{msg}");
        let _ = stderr.flush();
    });
    if !was_paused {
        clx::progress::resume();
    }
}

#[derive(Clone, Copy, Default)]
pub struct PausingWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for PausingWriter {
    type Writer = PausingWriterGuard;

    fn make_writer(&'a self) -> Self::Writer {
        PausingWriterGuard { buf: Vec::new() }
    }
}

/// Per-event writer guard returned by [`PausingWriter::make_writer`].
/// Accumulates into `buf` and flushes once on drop. See `PausingWriter`
/// for the full pause/write/resume protocol.
pub struct PausingWriterGuard {
    buf: Vec<u8>,
}

impl Write for PausingWriterGuard {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for PausingWriterGuard {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.buf);
        // Pause *before* taking `TERM_LOCK`: `pause()` internally
        // calls `clear()`, which also grabs `TERM_LOCK`, and
        // `std::sync::Mutex` isn't reentrant — taking the lock first
        // would deadlock. Same ordering `ProgressJob::println` uses.
        //
        // The `is_paused()` → `pause()` check is intentionally not
        // atomic. Two guards dropping concurrently can both observe
        // `was_paused = false`, and the first `resume()` can restart
        // the render loop before the second thread's write lands.
        // That's a benign visual artifact (the progress bar may
        // briefly redraw between the two log lines), not a correctness
        // hazard: byte-level atomicity comes from `with_terminal_lock`
        // below, which serializes every writer — render thread,
        // `ProgressJob::println`, and other `PausingWriterGuard`
        // drops. `pause`/`resume` are best-effort visual guards on
        // top of that hard serialization.
        let was_paused = clx::progress::is_paused();
        if !was_paused {
            clx::progress::pause();
        }
        // Hold `TERM_LOCK` across the actual write so the render
        // thread (which also takes it before `write_frame`) and any
        // concurrent `ProgressJob::println` can't interleave between
        // our bytes. `with_terminal_lock` returns `()` here; the
        // explicit annotation silences its `#[must_use]`.
        let _: () = clx::progress::with_terminal_lock(|| {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(&buf);
            let _ = stderr.flush();
        });
        if !was_paused {
            clx::progress::resume();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_fetch_label_pluralizes_count() {
        assert_eq!(overflow_fetch_label(1), "1 more package…");
        assert_eq!(overflow_fetch_label(2), "2 more packages…");
    }
}
