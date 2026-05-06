//! Crate-shared test helper for serializing env-mutating tests.
//!
//! Rust 2024's `unsafe { std::env::set_var }` reflects the libc
//! reality: setenv/getenv are not thread-safe across each other
//! because setenv can realloc the environ pointer. Tests in this
//! crate that touch the process environment all acquire `ENV_LOCK`
//! to serialize against the parallel test runner.

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
