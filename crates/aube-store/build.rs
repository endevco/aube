use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let today = today_utc();
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    for (date, cfg) in remove_after_markers(Path::new(&manifest_dir).join("src").as_path()) {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
        if today >= date {
            println!("cargo::rustc-cfg={cfg}");
        }
    }
}

fn remove_after_markers(src: &Path) -> Vec<((i32, u32, u32), String)> {
    let mut out = Vec::new();
    collect_remove_after_markers(src, &mut out);
    out
}

fn collect_remove_after_markers(path: &Path, out: &mut Vec<((i32, u32, u32), String)>) {
    println!("cargo::rerun-if-changed={}", path.display());
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect_remove_after_markers(&entry.path(), out);
        }
        return;
    }
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return;
    }
    let Ok(source) = fs::read_to_string(path) else {
        return;
    };
    out.extend(parse_remove_after_markers(&source));
}

fn parse_remove_after_markers(source: &str) -> Vec<((i32, u32, u32), String)> {
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(offset) = rest.find("remove_after!(") {
        rest = &rest[offset + "remove_after!(".len()..];
        if let Some((date, cfg)) = parse_remove_after_marker(rest) {
            out.push((date, cfg));
        }
    }
    out
}

fn parse_remove_after_marker(source: &str) -> Option<((i32, u32, u32), String)> {
    let source = source.trim_start();
    let (date, source) = parse_string_literal(source)?;
    let source = source.trim_start().strip_prefix(',')?.trim_start();
    let cfg_len = source
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(source.len());
    let cfg = source[..cfg_len].to_string();
    Some((parse_date(&date)?, cfg))
}

fn parse_string_literal(source: &str) -> Option<(String, &str)> {
    let source = source.strip_prefix('"')?;
    let end = source.find('"')?;
    Some((source[..end].to_string(), &source[end + 1..]))
}

fn parse_date(date: &str) -> Option<(i32, u32, u32)> {
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    (parts.next().is_none()).then_some((year, month, day))
}

fn today_utc() -> (i32, u32, u32) {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    civil_from_days(days as i64)
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}
