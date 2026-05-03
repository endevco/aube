//! Emit `docs/error-codes.data.json` from `aube_codes::errors::ALL`
//! and `warnings::ALL`. Run via
//! `cargo run -p aube-codes --bin generate-error-codes-docs`
//! (wired into `mise run render`).
//!
//! VitePress imports the JSON via `docs/error-codes.data.ts`
//! (build-time data loader) and renders it through the
//! `<ErrorCodesTable>` Vue component on the `/error-codes` page.
//! Same shape as the benchmarks pipeline (`benchmarks/results.json`
//! → `docs/benchmarks.data.ts` → `<BenchChart>`).
//!
//! The JSON is the data contract — every code's identifier,
//! category, description, and (optional) bespoke exit code lives
//! next to its `pub const` declaration in
//! `crates/aube-codes/src/{errors,warnings}.rs`. Hand-edits to the
//! JSON or the markdown will be clobbered on the next
//! `mise run render`. Update the registry instead.

use aube_codes::{CodeMeta, errors, warnings};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let root = workspace_root();
    let out_path = root.join("docs/error-codes.data.json");
    let json = render_json();
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("failed to create {}: {e}", parent.display()));
    }
    fs::write(&out_path, json)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out_path.display()));
    println!(
        "generated {}",
        out_path.strip_prefix(&root).unwrap().display()
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Render both registries as a JSON document. Hand-rolled emitter so
/// `aube-codes` can stay dep-free — every other crate depends on it,
/// and dragging `serde_json` in here would propagate. The output is
/// stable per registry order, so diffs land cleanly when codes are
/// added or descriptions tweaked.
///
/// Schema (consumed by `docs/error-codes.data.ts`):
///
/// ```jsonc
/// {
///   "errors": [
///     { "name": "ERR_AUBE_NO_LOCKFILE", "category": "Lockfile",
///       "description": "...", "exit_code": 10 },
///     ...
///   ],
///   "warnings": [
///     { "name": "WARN_AUBE_IGNORED_BUILD_SCRIPTS", "category": "Install lifecycle",
///       "description": "...", "exit_code": null },
///     ...
///   ],
///   "categories": {
///     "errors":   ["Lockfile", "Resolver", ...],
///     "warnings": ["pnpmfile / hooks", "Install lifecycle", ...]
///   }
/// }
/// ```
///
/// `categories` lists each kind's category labels in the order they
/// first appear in `ALL`, so the Vue component can render filter
/// chips without re-deriving the order from individual entries.
fn render_json() -> String {
    let mut out = String::from("{\n");
    out.push_str("  \"errors\": ");
    push_codes_array(&mut out, errors::ALL, 2);
    out.push_str(",\n  \"warnings\": ");
    push_codes_array(&mut out, warnings::ALL, 2);
    out.push_str(",\n  \"categories\": {\n");
    out.push_str("    \"errors\": ");
    push_categories_array(&mut out, errors::ALL);
    out.push_str(",\n    \"warnings\": ");
    push_categories_array(&mut out, warnings::ALL);
    out.push_str("\n  }\n}\n");
    out
}

fn push_codes_array(out: &mut String, all: &[CodeMeta], indent: usize) {
    let pad = " ".repeat(indent);
    let inner_pad = " ".repeat(indent + 2);
    out.push_str("[\n");
    for (i, meta) in all.iter().enumerate() {
        write!(out, "{inner_pad}{{").unwrap();
        write!(out, "\"name\": \"{}\", ", json_escape(meta.name)).unwrap();
        write!(out, "\"category\": \"{}\", ", json_escape(meta.category)).unwrap();
        write!(
            out,
            "\"description\": \"{}\", ",
            json_escape(meta.description)
        )
        .unwrap();
        match meta.exit_code {
            Some(code) => write!(out, "\"exit_code\": {code}").unwrap(),
            None => write!(out, "\"exit_code\": null").unwrap(),
        }
        out.push('}');
        if i + 1 < all.len() {
            out.push(',');
        }
        out.push('\n');
    }
    write!(out, "{pad}]").unwrap();
}

fn push_categories_array(out: &mut String, all: &[CodeMeta]) {
    // First-seen order preserved via BTreeMap-keyed insertion check
    // against a Vec — same trick the markdown generator used before.
    let mut seen: BTreeMap<&'static str, ()> = BTreeMap::new();
    let mut ordered: Vec<&'static str> = Vec::new();
    for meta in all {
        if seen.insert(meta.category, ()).is_none() {
            ordered.push(meta.category);
        }
    }
    out.push('[');
    for (i, cat) in ordered.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "\"{}\"", json_escape(cat)).unwrap();
    }
    out.push(']');
}

/// Hand-rolled JSON string escape covering the chars that can appear
/// in code descriptions: backslash, double quote, and control chars.
/// Backticks and Unicode (em-dash, ≥, etc.) pass through unchanged
/// because JSON is UTF-8 and they're not control characters.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).unwrap(),
            c => out.push(c),
        }
    }
    out
}
