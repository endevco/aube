//! Bespoke Unix exit codes per error code.
//!
//! Most aube errors exit with the generic [`EXIT_GENERIC`] (`1`). A
//! curated subset — the ones a CI script or shell pipeline most often
//! wants to branch on — gets its own exit code so callers can react
//! without parsing stderr.
//!
//! The 8-bit exit-code space is lean (POSIX reserves several values
//! 126–165 for shell signals), so codes are allocated in 10-wide
//! ranges by category, with room to grow:
//!
//! | range  | category                                       |
//! | ------ | ---------------------------------------------- |
//! | 1      | generic / unknown error                        |
//! | 2      | CLI usage error                                |
//! | 10–19  | lockfile                                       |
//! | 20–29  | resolver                                       |
//! | 30–39  | tarball / store                                |
//! | 40–49  | registry / network                             |
//! | 50–59  | scripts / build                                |
//! | 60–69  | linker                                         |
//! | 70–79  | manifest / workspace                           |
//! | 80–89  | engine / cli surface                           |
//! | 90–99  | misc / safety                                  |
//!
//! Only error codes that appear in [`EXIT_TABLE`] get a bespoke exit;
//! everything else falls back to [`EXIT_GENERIC`]. The mapping is
//! stable — once an error code is assigned an exit code, neither
//! changes — but new codes may be added to the table at any time.
//!
//! Tooling consumers should branch on the *exit code* rather than the
//! exit category, since the categories are documentation, not API.

use crate::errors::*;

/// Generic catch-all. Anything not explicitly listed in
/// [`EXIT_TABLE`] resolves to this exit code.
pub const EXIT_GENERIC: i32 = 1;

/// CLI usage error — bad flags, conflicting options, missing required
/// arguments. Reserved as a convention, not currently emitted by aube
/// itself (clap exits with this code on its own).
pub const EXIT_CLI_USAGE: i32 = 2;

// Lockfile (10–19)
pub const EXIT_NO_LOCKFILE: i32 = 10;
pub const EXIT_LOCKFILE_PARSE: i32 = 11;
pub const EXIT_LOCKFILE_UNSUPPORTED_FORMAT: i32 = 12;

// Resolver (20–29)
pub const EXIT_NO_MATCHING_VERSION: i32 = 20;
pub const EXIT_NO_MATURE_MATCHING_VERSION: i32 = 21;
pub const EXIT_BLOCKED_EXOTIC_SUBDEP: i32 = 22;
pub const EXIT_TRUST_DOWNGRADE: i32 = 23;
pub const EXIT_TRUST_MISSING_TIME: i32 = 24;
pub const EXIT_UNKNOWN_CATALOG: i32 = 25;
pub const EXIT_UNKNOWN_CATALOG_ENTRY: i32 = 26;
pub const EXIT_PEER_CONTEXT_NOT_CONVERGED: i32 = 27;

// Tarball / store (30–39)
pub const EXIT_TARBALL_INTEGRITY: i32 = 30;
pub const EXIT_TARBALL_EXTRACT: i32 = 31;
pub const EXIT_PKG_CONTENT_MISMATCH: i32 = 32;
pub const EXIT_GIT_ERROR: i32 = 33;

// Registry / network (40–49)
pub const EXIT_PACKAGE_NOT_FOUND: i32 = 40;
pub const EXIT_VERSION_NOT_FOUND: i32 = 41;
pub const EXIT_UNAUTHORIZED: i32 = 42;
pub const EXIT_OFFLINE: i32 = 43;
pub const EXIT_INVALID_PACKAGE_NAME: i32 = 44;
pub const EXIT_REGISTRY_WRITE_REJECTED: i32 = 45;

// Scripts / build (50–59)
pub const EXIT_SCRIPT_NON_ZERO_EXIT: i32 = 50;
pub const EXIT_SCRIPT_SPAWN: i32 = 51;

// Linker (60–69)
pub const EXIT_PATCH_FAILED: i32 = 60;
pub const EXIT_LINK_FAILED: i32 = 61;
pub const EXIT_MISSING_PACKAGE_INDEX: i32 = 62;
pub const EXIT_MISSING_STORE_FILE: i32 = 63;

// Manifest / workspace (70–79)
pub const EXIT_MANIFEST_PARSE: i32 = 70;
pub const EXIT_WORKSPACE_PARSE: i32 = 71;

// Engine / cli surface (80–89)
pub const EXIT_UNSUPPORTED_ENGINE: i32 = 80;
pub const EXIT_UNKNOWN_COMMAND: i32 = 81;
pub const EXIT_NPM_ONLY_COMMAND: i32 = 82;

// Misc / safety (90–99)
pub const EXIT_UNSAFE_INDEX_KEY: i32 = 90;
pub const EXIT_UNSAFE_SHEBANG_INTERPRETER: i32 = 91;

/// Mapping from error code identifier → bespoke exit code. Codes
/// absent from this table fall through to [`EXIT_GENERIC`]. Order is
/// not significant — the table is consumed by [`exit_code_for`] via
/// linear scan, which is fine for ~30 entries and avoids a HashMap
/// allocation in the failure path.
pub const EXIT_TABLE: &[(&str, i32)] = &[
    (ERR_AUBE_NO_LOCKFILE, EXIT_NO_LOCKFILE),
    (ERR_AUBE_LOCKFILE_PARSE, EXIT_LOCKFILE_PARSE),
    (
        ERR_AUBE_LOCKFILE_UNSUPPORTED_FORMAT,
        EXIT_LOCKFILE_UNSUPPORTED_FORMAT,
    ),
    (ERR_AUBE_NO_MATCHING_VERSION, EXIT_NO_MATCHING_VERSION),
    (
        ERR_AUBE_NO_MATURE_MATCHING_VERSION,
        EXIT_NO_MATURE_MATCHING_VERSION,
    ),
    (ERR_AUBE_BLOCKED_EXOTIC_SUBDEP, EXIT_BLOCKED_EXOTIC_SUBDEP),
    (ERR_AUBE_TRUST_DOWNGRADE, EXIT_TRUST_DOWNGRADE),
    (ERR_AUBE_TRUST_MISSING_TIME, EXIT_TRUST_MISSING_TIME),
    (ERR_AUBE_UNKNOWN_CATALOG, EXIT_UNKNOWN_CATALOG),
    (ERR_AUBE_UNKNOWN_CATALOG_ENTRY, EXIT_UNKNOWN_CATALOG_ENTRY),
    (
        ERR_AUBE_PEER_CONTEXT_NOT_CONVERGED,
        EXIT_PEER_CONTEXT_NOT_CONVERGED,
    ),
    (ERR_AUBE_TARBALL_INTEGRITY, EXIT_TARBALL_INTEGRITY),
    (ERR_AUBE_TARBALL_EXTRACT, EXIT_TARBALL_EXTRACT),
    (ERR_AUBE_PKG_CONTENT_MISMATCH, EXIT_PKG_CONTENT_MISMATCH),
    (ERR_AUBE_GIT_ERROR, EXIT_GIT_ERROR),
    (ERR_AUBE_PACKAGE_NOT_FOUND, EXIT_PACKAGE_NOT_FOUND),
    (ERR_AUBE_VERSION_NOT_FOUND, EXIT_VERSION_NOT_FOUND),
    (ERR_AUBE_UNAUTHORIZED, EXIT_UNAUTHORIZED),
    (ERR_AUBE_OFFLINE, EXIT_OFFLINE),
    (ERR_AUBE_INVALID_PACKAGE_NAME, EXIT_INVALID_PACKAGE_NAME),
    (
        ERR_AUBE_REGISTRY_WRITE_REJECTED,
        EXIT_REGISTRY_WRITE_REJECTED,
    ),
    (ERR_AUBE_SCRIPT_NON_ZERO_EXIT, EXIT_SCRIPT_NON_ZERO_EXIT),
    (ERR_AUBE_SCRIPT_SPAWN, EXIT_SCRIPT_SPAWN),
    (ERR_AUBE_PATCH_FAILED, EXIT_PATCH_FAILED),
    (ERR_AUBE_LINK_FAILED, EXIT_LINK_FAILED),
    (ERR_AUBE_MISSING_PACKAGE_INDEX, EXIT_MISSING_PACKAGE_INDEX),
    (ERR_AUBE_MISSING_STORE_FILE, EXIT_MISSING_STORE_FILE),
    (ERR_AUBE_MANIFEST_PARSE, EXIT_MANIFEST_PARSE),
    (ERR_AUBE_WORKSPACE_PARSE, EXIT_WORKSPACE_PARSE),
    (ERR_AUBE_UNSUPPORTED_ENGINE, EXIT_UNSUPPORTED_ENGINE),
    (ERR_AUBE_UNKNOWN_COMMAND, EXIT_UNKNOWN_COMMAND),
    (ERR_AUBE_NPM_ONLY_COMMAND, EXIT_NPM_ONLY_COMMAND),
    (ERR_AUBE_UNSAFE_INDEX_KEY, EXIT_UNSAFE_INDEX_KEY),
    (
        ERR_AUBE_UNSAFE_SHEBANG_INTERPRETER,
        EXIT_UNSAFE_SHEBANG_INTERPRETER,
    ),
];

/// Returns the bespoke exit code for `code`, or `None` if the code
/// has no bespoke entry (the caller should use [`EXIT_GENERIC`]).
///
/// Linear-scan lookup — fine for ~30 entries and avoids dragging in a
/// HashMap. The failure path is not hot.
pub fn exit_code_for(code: &str) -> Option<i32> {
    EXIT_TABLE
        .iter()
        .find_map(|(k, v)| if *k == code { Some(*v) } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn exit_codes_are_unique() {
        let mut seen = HashSet::new();
        for (_, exit) in EXIT_TABLE {
            assert!(
                seen.insert(*exit),
                "duplicate exit code {exit} in EXIT_TABLE"
            );
        }
    }

    #[test]
    fn exit_codes_are_in_valid_unix_range() {
        // POSIX exit codes are 0–255. Reserve <10 for the special
        // generic/usage entries; everything in EXIT_TABLE should fall
        // in [10, 125] to avoid colliding with shell signal codes
        // (126–165 are reserved by POSIX).
        for (code, exit) in EXIT_TABLE {
            assert!(
                (10..=125).contains(exit),
                "exit code {exit} for {code} is out of the [10, 125] range"
            );
        }
    }

    #[test]
    fn exit_lookup_round_trips() {
        for (code, expected) in EXIT_TABLE {
            assert_eq!(
                exit_code_for(code),
                Some(*expected),
                "round-trip failed for {code}"
            );
        }
    }

    #[test]
    fn unknown_code_returns_none() {
        assert_eq!(exit_code_for("ERR_AUBE_TOTALLY_MADE_UP"), None);
    }

    #[test]
    fn every_table_entry_references_a_real_error_code() {
        let known: HashSet<&str> = ALL.iter().map(|(_, v)| *v).collect();
        for (code, _) in EXIT_TABLE {
            assert!(
                known.contains(code),
                "EXIT_TABLE references unknown error code {code}"
            );
        }
    }
}
