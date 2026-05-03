//! Stable identifiers for every error and warning that aube emits.
//!
//! The crate is dependency-free on purpose: every other aube crate may
//! depend on it. Codes are exposed as `pub const &str` so they can be
//! used unmodified in `tracing::warn!(code = aube_codes::warnings::X, ...)`,
//! `#[diagnostic(code = aube_codes::errors::Y)]`, and ndjson-emitting
//! reporters without needing to call `.as_str()` or do any conversion.
//!
//! Naming convention:
//! - `ERR_AUBE_*` for errors (anything that returns `Err` to the caller
//!   or aborts with a non-zero exit).
//! - `WARN_AUBE_*` for warnings (`tracing::warn!`) and non-fatal
//!   `tracing::error!` sites that don't change exit status.
//!
//! aube does not emit `ERR_PNPM_*` codes itself. Where a code maps
//! cleanly onto a pnpm concept (lockfile, peer-deps, tarball, etc.) we
//! reuse pnpm's *suffix* under the `ERR_AUBE_` prefix so the code reads
//! the same to anyone familiar with pnpm — but the published code is
//! always `ERR_AUBE_*`.
//!
//! Codes are stable: once published, a code's identifier and meaning
//! must not change. Adding new codes is fine; removing or repurposing
//! one is a breaking change.

#![forbid(unsafe_code)]

pub mod errors;
pub mod exit;
pub mod warnings;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_const_value_matches_its_name() {
        // The `pub const ERR_AUBE_X: &str = "ERR_AUBE_X"` shape is
        // load-bearing — typos between the const name and the value
        // would silently emit the wrong code. The list below is the
        // exhaustive set of error codes aube publishes; new codes
        // must be added here.
        for (name, value) in errors::ALL {
            assert_eq!(*name, *value, "error code value must match its name");
            assert!(
                value.starts_with("ERR_AUBE_"),
                "error codes must use the ERR_AUBE_ prefix: {value}"
            );
        }
    }

    #[test]
    fn every_warning_const_value_matches_its_name() {
        for (name, value) in warnings::ALL {
            assert_eq!(*name, *value, "warning code value must match its name");
            assert!(
                value.starts_with("WARN_AUBE_"),
                "warning codes must use the WARN_AUBE_ prefix: {value}"
            );
        }
    }

    #[test]
    fn no_duplicate_codes() {
        use std::collections::HashSet;
        let all: Vec<&str> = errors::ALL
            .iter()
            .chain(warnings::ALL.iter())
            .map(|(_, v)| *v)
            .collect();
        let unique: HashSet<&str> = all.iter().copied().collect();
        assert_eq!(
            all.len(),
            unique.len(),
            "duplicate code identifier across errors/warnings"
        );
    }
}
