//! Bun-style pluggable security scanner.
//!
//! When `securityScanner` is set, `aube add` spawns the configured
//! executable, pipes the package list in as JSON on stdin, and reads
//! advisories back on stdout. A `fatal`-level advisory blocks the
//! add with `ERR_AUBE_SECURITY_SCANNER_FATAL`; `warn`-level emits
//! `WARN_AUBE_SECURITY_SCANNER_FINDING` and continues. Any failure
//! mode in between (missing binary, non-zero exit, timeout,
//! unparseable JSON) emits `WARN_AUBE_SECURITY_SCANNER_FAILED` and
//! falls through — a broken scanner shouldn't be able to block
//! every add.
//!
//! Contract is modeled on [Bun's Security Scanner
//! API](https://bun.sh/docs/install/security-scanner). Bun's
//! scanner is an in-process JS plugin; aube's is a subprocess
//! because aube is Rust and doesn't host a JS runtime. The
//! semantic shape — `{packages} → {advisories}` with levels
//! `fatal | warn` — is identical, so the same logical scanner
//! (Socket, Snyk, custom org policies) can ship to both runtimes
//! behind a thin wrapper.

use aube_codes::errors::ERR_AUBE_SECURITY_SCANNER_FATAL;
use aube_codes::warnings::{WARN_AUBE_SECURITY_SCANNER_FAILED, WARN_AUBE_SECURITY_SCANNER_FINDING};
use miette::miette;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Hard upper bound on how long the scanner may run. A scanner
/// that hangs forever shouldn't be able to wedge `aube add`.
/// 30s mirrors what npm and bun use for similar install-time hooks.
const SCANNER_TIMEOUT: Duration = Duration::from_secs(30);

/// Stdin payload format version. Bumped only when we change the
/// shape in a backwards-incompatible way; scanners that support
/// multiple versions should branch on this field.
const PROTOCOL_VERSION: u32 = 1;

/// One package about to be added, as the scanner sees it.
/// `spec` is whatever the user typed after `@` (e.g. `^4.17.21`,
/// `latest`, a JSR/npm spec) — passed verbatim so the scanner
/// can apply version-range policy if it wants to.
#[derive(Debug, Clone, Serialize)]
pub struct ScannerPackage {
    pub name: String,
    pub spec: String,
}

#[derive(Debug, Serialize)]
struct ScannerRequest<'a> {
    version: u32,
    packages: &'a [ScannerPackage],
}

#[derive(Debug, Deserialize, Default)]
struct ScannerResponse {
    #[serde(default)]
    advisories: Vec<Advisory>,
}

#[derive(Debug, Deserialize)]
struct Advisory {
    package: String,
    level: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    url: Option<String>,
}

/// Outcome categories used when classifying scanner advisories.
/// `Fatal` blocks the add; `Warn` emits a warning and continues;
/// `Other` is logged at debug level and otherwise ignored —
/// future-proof for additional levels (e.g. `info`) without
/// changing the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Fatal,
    Warn,
    Other,
}

fn classify(level: &str) -> Severity {
    match level.to_ascii_lowercase().as_str() {
        "fatal" => Severity::Fatal,
        "warn" | "warning" => Severity::Warn,
        _ => Severity::Other,
    }
}

/// Run `scanner` against the candidate `packages`. Empty `scanner`
/// or empty `packages` short-circuits to `Ok(())` without spawning
/// anything — the caller already filtered to registry-bound names,
/// and there's nothing useful to scan beyond that.
pub async fn run_scanner(
    scanner: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> miette::Result<()> {
    if scanner.is_empty() || packages.is_empty() {
        return Ok(());
    }
    let response = match invoke(scanner, cwd, packages).await {
        Ok(r) => r,
        Err(e) => {
            // Fail open: a misconfigured scanner shouldn't break
            // every `aube add` in the project. The operator sees
            // the warning and can fix their setup; the install
            // continues using whatever other gates are configured
            // (OSV check, minimum-release-age, etc.).
            tracing::warn!(
                code = WARN_AUBE_SECURITY_SCANNER_FAILED,
                "securityScanner `{scanner}` failed: {e}"
            );
            return Ok(());
        }
    };
    apply_advisories(scanner, &response.advisories)
}

async fn invoke(
    scanner: &str,
    cwd: &Path,
    packages: &[ScannerPackage],
) -> Result<ScannerResponse, String> {
    let request = ScannerRequest {
        version: PROTOCOL_VERSION,
        packages,
    };
    let body = serde_json::to_vec(&request)
        .map_err(|e| format!("failed to encode scanner request: {e}"))?;

    let mut cmd = tokio::process::Command::new(scanner);
    cmd.current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The scanner runs against unresolved package specs — it
        // has no business with npm auth tokens or registry
        // credentials. Scrubbing them keeps a hostile or buggy
        // scanner from exfiltrating them as a side effect.
        .env_remove("NPM_TOKEN")
        .env_remove("NODE_AUTH_TOKEN")
        .env_remove("GITHUB_TOKEN");

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn scanner executable: {e}"))?;

    // Take stdin out of the child so we can write to it. The
    // child must close stdin before producing output so the
    // scanner sees EOF on its read loop; that's what
    // `drop(stdin)` after writing achieves.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "internal pipe error: stdin not available".to_string())?;
    use tokio::io::AsyncWriteExt;
    stdin
        .write_all(&body)
        .await
        .map_err(|e| format!("failed to write request to scanner stdin: {e}"))?;
    drop(stdin);

    let wait = child.wait_with_output();
    let output = tokio::time::timeout(SCANNER_TIMEOUT, wait)
        .await
        .map_err(|_| {
            format!(
                "scanner exceeded {} second timeout",
                SCANNER_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("failed to wait for scanner subprocess: {e}"))?;

    if !output.status.success() {
        // Surface stderr (truncated) so the operator can diagnose
        // what's wrong with their scanner. Don't surface stdout —
        // it might be a partial JSON document and pasting it
        // into the warning is noisy.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        let snippet = if trimmed.len() > 500 {
            format!("{}…", &trimmed[..500])
        } else {
            trimmed.to_string()
        };
        return Err(format!(
            "scanner exited with status {:?}; stderr: {snippet}",
            output.status.code()
        ));
    }

    serde_json::from_slice::<ScannerResponse>(&output.stdout)
        .map_err(|e| format!("scanner stdout was not valid JSON: {e}"))
}

fn apply_advisories(scanner: &str, advisories: &[Advisory]) -> miette::Result<()> {
    let mut fatal: Vec<&Advisory> = Vec::new();
    for adv in advisories {
        match classify(&adv.level) {
            Severity::Fatal => fatal.push(adv),
            Severity::Warn => {
                let url_suffix = adv
                    .url
                    .as_deref()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                tracing::warn!(
                    code = WARN_AUBE_SECURITY_SCANNER_FINDING,
                    "{}: {}{}",
                    adv.package,
                    if adv.description.is_empty() {
                        "flagged by securityScanner"
                    } else {
                        adv.description.as_str()
                    },
                    url_suffix
                );
            }
            Severity::Other => {
                tracing::debug!(
                    "securityScanner reported level={} for {}: {}",
                    adv.level,
                    adv.package,
                    adv.description
                );
            }
        }
    }
    if fatal.is_empty() {
        return Ok(());
    }
    let mut lines = vec![format!(
        "refusing to add package(s) flagged by `securityScanner = {scanner}`:"
    )];
    for adv in &fatal {
        let url_suffix = adv
            .url
            .as_deref()
            .map(|u| format!(" — {u}"))
            .unwrap_or_default();
        let body = if adv.description.is_empty() {
            "(no description)".to_string()
        } else {
            adv.description.clone()
        };
        lines.push(format!("  - {}: {}{url_suffix}", adv.package, body));
    }
    Err(miette!(
        code = ERR_AUBE_SECURITY_SCANNER_FATAL,
        "{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adv(package: &str, level: &str) -> Advisory {
        Advisory {
            package: package.to_string(),
            level: level.to_string(),
            description: String::new(),
            url: None,
        }
    }

    #[test]
    fn classify_is_case_insensitive() {
        // Scanner authors are inconsistent about case — make sure
        // we accept "FATAL", "Warning", "warn" interchangeably so
        // a scanner that ships for Bun's case-sensitive parser
        // also works under aube.
        assert_eq!(classify("FATAL"), Severity::Fatal);
        assert_eq!(classify("fatal"), Severity::Fatal);
        assert_eq!(classify("Warning"), Severity::Warn);
        assert_eq!(classify("warn"), Severity::Warn);
        assert_eq!(classify("info"), Severity::Other);
        assert_eq!(classify(""), Severity::Other);
    }

    #[test]
    fn apply_advisories_empty_is_ok() {
        // No advisories ⇒ no block, no warning emitted. This is
        // the dominant path on a clean install — should never error.
        assert!(apply_advisories("/bin/true", &[]).is_ok());
    }

    #[test]
    fn apply_advisories_warn_only_does_not_block() {
        // A scanner that reports `warn` levels should let the add
        // through. The warning is surfaced via tracing; the
        // miette error path is reserved for `fatal`.
        let advs = vec![adv("pkg-a", "warn"), adv("pkg-b", "warning")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    #[test]
    fn apply_advisories_fatal_blocks() {
        // One fatal advisory is enough — install refused.
        let advs = vec![adv("pkg-a", "warn"), adv("evil", "fatal")];
        let err = apply_advisories("scanner", &advs).unwrap_err();
        let msg = format!("{err:?}");
        // Both package and scanner identity should be in the
        // error message so the user knows what blocked and why.
        assert!(msg.contains("evil"), "missing package name: {msg}");
        assert!(msg.contains("scanner"), "missing scanner ref: {msg}");
    }

    #[test]
    fn unknown_severity_falls_through() {
        // A future-dated scanner emitting `level: "info"` should
        // not block, not warn (since we don't know if it's a
        // structural issue or just chatter), and not crash.
        let advs = vec![adv("pkg-a", "info"), adv("pkg-b", "trace")];
        assert!(apply_advisories("scanner", &advs).is_ok());
    }

    /// End-to-end test: spawn a real `sh` subprocess that mimics a
    /// scanner, verify aube reads its stdout, parses the JSON, and
    /// applies the verdict. Unit tests cover the policy layer
    /// (`classify`, `apply_advisories`); this is the only test
    /// that exercises stdin piping, `wait_with_output`, and
    /// `serde_json::from_slice(stdout)` together.
    ///
    /// Unix-only because the inline script uses POSIX `sh`. The
    /// subprocess plumbing has no Windows-specific code, so the
    /// platform gate is purely about the test harness.
    #[cfg(unix)]
    #[tokio::test]
    async fn end_to_end_blocks_on_fatal_advisory() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scanner.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        // The scanner discards stdin (we don't care what aube sent
        // for the purposes of this test — that's covered by the
        // policy unit tests) and emits a fatal advisory.
        writeln!(
            f,
            "#!/bin/sh\ncat >/dev/null\necho '{{\"advisories\":[{{\"package\":\"evil\",\"level\":\"fatal\",\"description\":\"test\"}}]}}'"
        ).unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let pkgs = vec![ScannerPackage {
            name: "evil".to_string(),
            spec: "latest".to_string(),
        }];
        let err = run_scanner(path.to_str().unwrap(), tmp.path(), &pkgs)
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("evil"), "missing pkg in error: {msg}");
        assert!(msg.contains("test"), "missing description in error: {msg}");
    }

    /// Companion to the e2e block test: a scanner emitting only
    /// `warn` advisories should let the add through. Catches a
    /// regression where the fatal/warn branch wired up wrong.
    #[cfg(unix)]
    #[tokio::test]
    async fn end_to_end_passes_on_warn_only() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("scanner.sh");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "#!/bin/sh\ncat >/dev/null\necho '{{\"advisories\":[{{\"package\":\"meh\",\"level\":\"warn\",\"description\":\"minor\"}}]}}'"
        ).unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let pkgs = vec![ScannerPackage {
            name: "meh".to_string(),
            spec: "1.0.0".to_string(),
        }];
        assert!(
            run_scanner(path.to_str().unwrap(), tmp.path(), &pkgs)
                .await
                .is_ok()
        );
    }

    /// A scanner that doesn't exist on disk must surface as
    /// `WARN_AUBE_SECURITY_SCANNER_FAILED` and let the add
    /// proceed — a broken scanner shouldn't gate every add.
    /// `run_scanner` therefore returns `Ok(())` even on spawn
    /// failure; the operator sees the warning via tracing.
    #[tokio::test]
    async fn missing_scanner_fails_open() {
        let pkgs = vec![ScannerPackage {
            name: "lodash".to_string(),
            spec: "^4".to_string(),
        }];
        let result = run_scanner(
            "/definitely/not/a/real/path/to/a/scanner",
            std::path::Path::new("."),
            &pkgs,
        )
        .await;
        assert!(result.is_ok(), "fail-open contract broken: {result:?}");
    }
}
