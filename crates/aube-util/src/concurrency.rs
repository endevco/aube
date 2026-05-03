//! Concurrency configuration helpers shared across aube crates.
//!
//! Today this module exposes one thing: the
//! `AUBE_CONCURRENCY=<N>` env override that lets users pin the
//! tarball-fetch fan-out when the default 128 in-flight requests
//! trigger 429/503 throttling on slow private registries
//! (Artifactory, Nexus). The override is a knob, not a probe — when
//! AIMD ramping lands it will live alongside the semaphore in
//! `aube-registry::concurrency` (the layer that owns retry signals).
//!
//! Range-clamped to `[CONCURRENCY_FLOOR, CONCURRENCY_CEILING]` so a
//! hostile or typo'd value can't exhaust file descriptors on Windows
//! (default ulimit 8192).

/// Lower bound on `AUBE_CONCURRENCY`. A degenerate slow link still
/// makes progress with 8 in-flight fetches.
pub const CONCURRENCY_FLOOR: u32 = 8;

/// Upper bound on `AUBE_CONCURRENCY`. Picked so a pathological env
/// value cannot exhaust the Windows default fd ulimit.
pub const CONCURRENCY_CEILING: u32 = 256;

/// Read `AUBE_CONCURRENCY` as a clamped integer override.
/// Returns `None` when the variable is unset, missing, or outside
/// the range — callers fall back to the default (npmrc / setting /
/// hard-coded). Out-of-range and non-numeric values warn.
pub fn parse_concurrency_env() -> Option<u32> {
    let raw = std::env::var_os("AUBE_CONCURRENCY")?;
    if let Some(s) = raw.to_str()
        && let Ok(n) = s.parse::<u32>()
        && (CONCURRENCY_FLOOR..=CONCURRENCY_CEILING).contains(&n)
    {
        return Some(n);
    }
    tracing::warn!(
        code = aube_codes::warnings::WARN_AUBE_CONCURRENCY_ENV_INVALID,
        value = ?raw,
        floor = CONCURRENCY_FLOOR,
        ceiling = CONCURRENCY_CEILING,
        "AUBE_CONCURRENCY ignored: must be an integer in [floor, ceiling]; using default"
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The env var is process-global. Serialize via a static mutex so
    // these tests don't race the parallel test runner.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<F: FnOnce()>(value: Option<&str>, f: F) {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("AUBE_CONCURRENCY");
        // SAFETY: tests serialized via ENV_LOCK; no other thread
        // touches this var concurrently.
        unsafe {
            match value {
                Some(v) => std::env::set_var("AUBE_CONCURRENCY", v),
                None => std::env::remove_var("AUBE_CONCURRENCY"),
            }
        }
        f();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("AUBE_CONCURRENCY", v),
                None => std::env::remove_var("AUBE_CONCURRENCY"),
            }
        }
    }

    #[test]
    fn unset_returns_none() {
        with_env(None, || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn in_range_returns_value() {
        with_env(Some("64"), || {
            assert_eq!(parse_concurrency_env(), Some(64));
        });
    }

    #[test]
    fn below_floor_warns_and_returns_none() {
        with_env(Some("1"), || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn above_ceiling_warns_and_returns_none() {
        with_env(Some("99999"), || {
            assert_eq!(parse_concurrency_env(), None);
        });
    }

    #[test]
    fn non_numeric_warns_and_returns_none() {
        with_env(Some("garbage"), || {
            assert_eq!(parse_concurrency_env(), None);
        });
    }

    #[test]
    fn empty_warns_and_returns_none() {
        with_env(Some(""), || assert_eq!(parse_concurrency_env(), None));
    }

    #[test]
    fn floor_and_ceiling_inclusive() {
        with_env(Some("8"), || {
            assert_eq!(parse_concurrency_env(), Some(CONCURRENCY_FLOOR));
        });
        with_env(Some("256"), || {
            assert_eq!(parse_concurrency_env(), Some(CONCURRENCY_CEILING));
        });
    }
}
