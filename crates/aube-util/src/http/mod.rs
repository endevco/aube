//! HTTP client helpers reused across aube crates.
//!
//! The npm registry path is dominated by cold TCP+TLS handshakes,
//! per-origin DNS lookups, and per-request priority noise. Each helper
//! here addresses one of those costs without owning a `reqwest::Client`
//! itself — call sites keep their builders and pass them in.
//!
//! Killswitch convention follows aube-util: every optimization that
//! defaults ON ships an `AUBE_DISABLE_*` env var. Each killswitch is
//! named in the doc comment of the function reading it so cargo doc
//! enumerates them.

pub mod prewarm;
pub mod priority;
pub mod race;
pub mod resolve;
pub mod ticket_cache;
