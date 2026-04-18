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

use clx::progress::{
    ProgressJob, ProgressJobBuilder, ProgressJobDoneBehavior, ProgressOutput, ProgressStatus,
};
use clx::style;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

/// How often the CI heartbeat thread wakes to check whether to print a
/// progress line. Kept long enough that a 142-package fetch produces a
/// handful of lines, not a flood.
const CI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// Cap on the number of simultaneously-visible per-package fetch rows
/// in TTY mode. Bursts above this are collapsed into a single overflow
/// row labeled "N more packages…" so the animated display stays
/// bounded on installs that fan out hundreds of tarball fetches at
/// once.
const TTY_MAX_VISIBLE_FETCH_ROWS: usize = 5;

fn overflow_fetch_label(count: usize) -> String {
    let word = if count == 1 { "package" } else { "packages" };
    format!("{count} more {word}…")
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
            style::edim(env!("CARGO_PKG_VERSION")),
            style::edim("by en.dev"),
        );
        let root = ProgressJobBuilder::new()
            .body("{{aube}}{{phase}}  {{progress_bar(flex=true)}} {{cur}}/{{total}}")
            .body_text(Some("{{aube}}{{phase}} {{cur}}/{{total}}"))
            .prop("aube", &header)
            .prop("phase", "")
            .progress_current(0)
            .progress_total(0)
            .on_done(ProgressJobDoneBehavior::Hide)
            .start();
        Self {
            mode: Mode::Tty {
                root,
                total: Arc::new(AtomicUsize::new(0)),
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
            }
            Mode::Ci(s) => {
                s.resolved.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    /// Set the phase label shown to the right of the header (e.g. "resolving",
    /// "fetching", "linking"). Empty string clears it. In CI mode this
    /// bumps the `[N/3]` phase counter shown on the status line.
    pub fn set_phase(&self, phase: &str) {
        match &self.mode {
            Mode::Tty { root, .. } => {
                if phase.is_empty() {
                    root.prop("phase", "");
                } else {
                    root.prop("phase", &format!("{}", style::edim(format!(" — {phase}"))));
                }
            }
            Mode::Ci(s) => s.set_phase(phase),
        }
    }

    /// Credit `n` packages to the `reused` bucket: served from the global
    /// content-addressed store (cache hit) or materialized from a local
    /// `file:` / `link:` source — anything that didn't touch the network.
    pub fn inc_reused(&self, n: usize) {
        match &self.mode {
            Mode::Tty { root, .. } => root.increment(n),
            Mode::Ci(s) => {
                s.reused.fetch_add(n, Ordering::Relaxed);
            }
        }
    }

    /// Credit `bytes` to the CI-mode downloaded-bytes total. Called once per
    /// tarball after the registry fetch completes, on top of the per-package
    /// increment that `FetchRow::drop` contributes to the downloaded count.
    /// No-op in TTY mode (the animated bar has no room for a byte counter).
    pub fn inc_downloaded_bytes(&self, bytes: u64) {
        if let Mode::Ci(s) = &self.mode {
            s.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
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
                root, fetch_state, ..
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
    /// behind. CI mode prints a final status line + `Done in Xs` summary
    /// and blocks until the heartbeat thread has actually stopped, so no
    /// stray tick can appear after the summary. Idempotent.
    pub fn finish(&self) {
        match &self.mode {
            Mode::Tty { root, .. } => {
                root.set_status(ProgressStatus::Done);
                clx::progress::stop_clear();
            }
            Mode::Ci(s) => s.stop(true),
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
            let word = if total_packages == 1 {
                "package"
            } else {
                "packages"
            };
            let msg = if total_packages == 0 {
                "Already up to date".to_string()
            } else {
                format!("Already up to date ({total_packages} {word})")
            };
            // Same `aube VERSION by en.dev · …` shape in both TTY and
            // CI modes so the no-op line reads as part of the install
            // UI family. CI mode force-styles the ANSI escapes
            // (matching `render_header` / the CI final summary) —
            // GitHub Actions and the other supported CI runners render
            // them correctly.
            let line = match &self.mode {
                Mode::Tty { .. } => format!(
                    "{} {} {} {} {}",
                    style::emagenta("aube").bold(),
                    style::edim(env!("CARGO_PKG_VERSION")),
                    style::edim("by en.dev"),
                    style::edim("·"),
                    style::egreen(msg).bold(),
                ),
                Mode::Ci(_) => format!(
                    "{} {} {} {} {}",
                    forced(console::style("aube").magenta().bold()),
                    forced(console::style(env!("CARGO_PKG_VERSION")).dim()),
                    forced(console::style("by en.dev").dim()),
                    forced(console::style("·").dim()),
                    forced(console::style(msg).green().bright().bold()),
                ),
            };
            let _ = writeln!(std::io::stderr(), "{line}");
            return;
        }
        if linked == 0 {
            return;
        }
        if !matches!(self.mode, Mode::Tty { .. }) {
            return;
        }
        let word = if linked == 1 { "package" } else { "packages" };
        // Single line: `aube VERSION by en.dev · ✓ installed N packages in Xs`.
        // Same `aube VERSION by en.dev` shape the progress bar drew
        // while the install was running, joined to the green
        // success line by a middle dot so the whole thing reads as
        // one continuous status.
        let msg = format!(
            "✓ installed {linked} {word} in {}",
            format_duration(elapsed)
        );
        let line = format!(
            "{} {} {} {} {}",
            style::emagenta("aube").bold(),
            style::edim(env!("CARGO_PKG_VERSION")),
            style::edim("by en.dev"),
            style::edim("·"),
            style::egreen(msg).bold(),
        );
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
                visible,
            } => {
                if let Some(root) = root.upgrade() {
                    root.increment(1);
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

/// CI-mode shared state. Owns the heartbeat thread.
///
/// The status line has three moving parts: a phase counter (`[N/3]`
/// where 1=resolving, 2=fetching, 3=linking), the byte total for
/// downloaded tarballs, and an ASCII bar for `completed / resolved`
/// (where completed = reused + downloaded). One line, same shape each
/// time — reprinted only when something actually changed since the
/// previous line.
struct CiState {
    phase: AtomicUsize,
    resolved: AtomicUsize,
    reused: AtomicUsize,
    downloaded: AtomicUsize,
    downloaded_bytes: AtomicU64,
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
    alive: AtomicUsize,
    /// Signals the heartbeat thread to wake early (phase change / stop).
    wake: Condvar,
    wake_lock: Mutex<()>,
    /// The heartbeat thread's join handle, taken by `stop()` so the
    /// thread is guaranteed to have exited before the final summary
    /// line is written — no stray tick can appear after `Done in …`.
    heartbeat: Mutex<Option<thread::JoinHandle<()>>>,
}

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
    fn new() -> Self {
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

    /// Colored, framed header line. Emitted once, on the first heartbeat
    /// tick where there's something to show — so the CI log only grows
    /// an aube banner when an install is actually happening.
    fn render_header() -> String {
        let header_text = format!(
            "{} {} {}",
            forced(console::style("aube").magenta().bold()),
            forced(console::style(env!("CARGO_PKG_VERSION")).dim()),
            forced(console::style("by en.dev").dim()),
        );
        render_centered_line(&header_text, term_width())
    }

    fn spawn_heartbeat(state: &Arc<Self>) {
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

    fn set_phase(&self, phase: &str) {
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
    fn stop(&self, print_summary: bool) {
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
            forced(console::style("✓").green().bright()),
            forced(console::style(format_duration(elapsed)).dim()),
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
fn format_duration(d: Duration) -> String {
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

/// Force-enable ANSI styling on a `console::StyledObject` regardless
/// of TTY detection. clx/console both strip colors by default when
/// stderr isn't a terminal, but GitHub Actions (the primary target
/// for CI mode) renders ANSI just fine, and we want the framed
/// status block to look the same in CI as it does interactively.
fn forced<D>(s: console::StyledObject<D>) -> console::StyledObject<D> {
    s.force_styling(true)
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
