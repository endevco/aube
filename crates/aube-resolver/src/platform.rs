//! Platform filtering for `os` / `cpu` / `libc` package metadata.
//!
//! npm-style packages can declare the platforms they support via the
//! `os`, `cpu`, and `libc` arrays in `package.json`. Each entry is
//! either a positive match (`"linux"`, `"x64"`, `"glibc"`) or a
//! negation prefixed with `!` (`"!win32"`). pnpm's rule:
//!
//!   - empty array        → unconstrained (installable everywhere)
//!   - any negation hit   → reject
//!   - at least one pos   → accept only if one positive matches
//!   - negations only     → accept if no negation matched
//!
//! pnpm lets the user widen the match set beyond the host via
//! `pnpm.supportedArchitectures` — an object with `os`/`cpu`/`libc`
//! arrays, each entry either a concrete value or the literal `"current"`
//! which expands to the host triple. The package passes if ANY of the
//! (os, cpu, libc) combinations in the supported set is installable.
//!
//! This module stays intentionally small: no reading of config, no
//! serde, just the matcher and host detection. Configuration lives on
//! the `Resolver`, which calls [`is_supported`] during filtering.

/// User-declared override for the host triple used when filtering
/// optional dependencies. Missing arrays fall back to the host; the
/// literal `"current"` inside any array expands to the same host value
/// so users can write `["current", "linux"]` to keep their native
/// platform *and* also resolve optionals for Linux.
///
/// `explicit_combinations` sidesteps the cartesian expansion entirely —
/// [`aube_lock_default`] uses it to emit a hand-picked matrix that
/// drops rare combinations (darwin-x64) without shrinking the OS / CPU
/// lists a user might inspect.
#[derive(Debug, Clone, Default)]
pub struct SupportedArchitectures {
    pub os: Vec<String>,
    pub cpu: Vec<String>,
    pub libc: Vec<String>,
    /// When `Some`, `combinations()` returns these triples verbatim
    /// instead of computing `os` × `cpu` × `libc`. Lets a preset prune
    /// individual (os, cpu, libc) combinations the cartesian product
    /// would otherwise include.
    pub explicit_combinations: Option<Vec<(String, String, String)>>,
}

impl SupportedArchitectures {
    /// Wide default used when resolving into `aube-lock.yaml` with no
    /// user-declared `pnpm.supportedArchitectures`. Covers the common
    /// npm OS / CPU / libc combinations so optional native deps for
    /// every major platform land in the lockfile on a first resolve —
    /// a project resolved on macOS installs correctly on Linux CI
    /// without the user having to hand-edit their manifest. pnpm-lock
    /// / yarn / npm outputs stay host-only (pnpm parity) so we don't
    /// silently change the shape of a non-native lockfile.
    ///
    /// darwin-x64 is not in the baseline matrix: Apple Silicon is the
    /// shipping Mac platform, and several major native package
    /// ecosystems (sharp, swc) have already dropped Intel Mac binaries,
    /// so an Apple Silicon developer's lockfile doesn't need to bake in
    /// Intel Mac natives for other contributors. The host's own triple
    /// is always added to the set, though — an Intel Mac user installing
    /// on that same machine gets darwin-x64 natives because the host
    /// triple joins the matrix here. Exotic hosts (freebsd, ppc64, …)
    /// get the same treatment: their native deps still install for the
    /// user, and the wide matrix covers everyone else.
    pub fn aube_lock_default() -> Self {
        let mut combos = vec![
            ("darwin".to_string(), "arm64".to_string(), String::new()),
            ("linux".to_string(), "x64".to_string(), "glibc".to_string()),
            ("linux".to_string(), "x64".to_string(), "musl".to_string()),
            (
                "linux".to_string(),
                "arm64".to_string(),
                "glibc".to_string(),
            ),
            ("linux".to_string(), "arm64".to_string(), "musl".to_string()),
            ("win32".to_string(), "x64".to_string(), String::new()),
            ("win32".to_string(), "arm64".to_string(), String::new()),
        ];
        // Ensure the host's own triple is always included. Without this
        // step an Intel Mac user (darwin-x64) would silently lose their
        // own native optional deps — the cross-platform widening dropped
        // them from the matrix, and the post-resolve `filter_graph` can't
        // bring back packages that never entered the graph in the first
        // place.
        let host = host_triple();
        let host_triple_owned = (host.0.to_string(), host.1.to_string(), host.2.to_string());
        if !combos.contains(&host_triple_owned) {
            combos.push(host_triple_owned);
        }
        Self {
            os: Vec::new(),
            cpu: Vec::new(),
            libc: Vec::new(),
            explicit_combinations: Some(combos),
        }
    }

    /// Expand any `"current"` entries to the host triple and default
    /// empty arrays to `[host]`. The result is a non-empty list of
    /// (os, cpu, libc) combinations the caller can test against.
    fn combinations(&self) -> Vec<(String, String, String)> {
        if let Some(ref explicit) = self.explicit_combinations {
            return explicit.clone();
        }
        let host = host_triple();
        let expand = |field: &[String], host_val: &str| -> Vec<String> {
            if field.is_empty() {
                return vec![host_val.to_string()];
            }
            field
                .iter()
                .map(|v| {
                    if v == "current" {
                        host_val.to_string()
                    } else {
                        v.clone()
                    }
                })
                .collect()
        };
        let os = expand(&self.os, host.0);
        let cpu = expand(&self.cpu, host.1);
        let libc = expand(&self.libc, host.2);
        let mut out = Vec::with_capacity(os.len() * cpu.len() * libc.len());
        for o in &os {
            for c in &cpu {
                for l in &libc {
                    out.push((o.clone(), c.clone(), l.clone()));
                }
            }
        }
        out
    }
}

/// Return the host's (os, cpu, libc) triple using npm's vocabulary.
/// `libc` is `"glibc"` / `"musl"` on Linux and `""` elsewhere — npm
/// only sets `libc` on Linux packages, so non-Linux hosts treat libc
/// constraints as a no-op.
pub fn host_triple() -> (&'static str, &'static str, &'static str) {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let cpu = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "x86" => "ia32",
        "aarch64" => "arm64",
        "powerpc64" => "ppc64",
        other => other,
    };
    let libc = if cfg!(target_os = "linux") {
        if cfg!(target_env = "musl") {
            "musl"
        } else {
            "glibc"
        }
    } else {
        ""
    };
    (os, cpu, libc)
}

/// Apply npm's `os`/`cpu`/`libc` rules to a single (pkg_field, host)
/// pair. An empty pkg array is unconstrained; negations reject; at
/// least one positive entry means one must match.
fn field_matches(pkg_field: &[String], host: &str) -> bool {
    if pkg_field.is_empty() {
        return true;
    }
    let mut has_positive = false;
    let mut positive_matched = false;
    for entry in pkg_field {
        if let Some(neg) = entry.strip_prefix('!') {
            if neg == host {
                return false;
            }
        } else {
            has_positive = true;
            if entry == host {
                positive_matched = true;
            }
        }
    }
    !has_positive || positive_matched
}

/// Decide whether a package is installable on any of the (os, cpu,
/// libc) combinations expanded from `supported`. The `pkg_libc` check
/// is skipped when the host libc is empty (non-Linux) — npm doesn't
/// enforce libc off Linux.
pub fn is_supported(
    pkg_os: &[String],
    pkg_cpu: &[String],
    pkg_libc: &[String],
    supported: &SupportedArchitectures,
) -> bool {
    for (os, cpu, libc) in supported.combinations() {
        if !field_matches(pkg_os, &os) {
            continue;
        }
        if !field_matches(pkg_cpu, &cpu) {
            continue;
        }
        if !libc.is_empty() && !field_matches(pkg_libc, &libc) {
            continue;
        }
        return true;
    }
    false
}

/// Remove optional dependencies that fail the platform check or appear in the
/// ignore list from a parsed `LockfileGraph`, then garbage-collect any packages
/// that become unreachable from the surviving importers.
///
/// Used by the install-from-lockfile path, where the resolver's inline
/// filter never runs: the lockfile carries os/cpu/libc per package so
/// aube can re-check on every platform without reparsing packuments.
///
/// Root and transitive optional edges are inspected directly. Any package that
/// becomes unreachable after optional-edge pruning is removed by the GC pass.
pub fn filter_graph(
    graph: &mut aube_lockfile::LockfileGraph,
    supported: &SupportedArchitectures,
    ignored: &std::collections::BTreeSet<String>,
) {
    use aube_lockfile::DepType;
    use rustc_hash::FxHashSet;

    let is_mismatched =
        |pkg: &aube_lockfile::LockedPackage| !is_supported(&pkg.os, &pkg.cpu, &pkg.libc, supported);

    // 1. Drop root optional deps by name or by platform.
    for deps in graph.importers.values_mut() {
        deps.retain(|dep| {
            if dep.dep_type != DepType::Optional {
                return true;
            }
            if ignored.contains(&dep.name) {
                return false;
            }
            !matches!(graph.packages.get(&dep.dep_path), Some(pkg) if is_mismatched(pkg))
        });
    }

    // 2. Drop transitive optional deps by name or platform. The pnpm parser
    // mirrors active optional edges into `dependencies`, so remove that edge
    // whenever the optional edge is filtered.
    let package_keys: FxHashSet<String> = graph.packages.keys().cloned().collect();
    let mismatched_packages: FxHashSet<String> = graph
        .packages
        .iter()
        .filter(|(_, pkg)| is_mismatched(pkg))
        .map(|(dep_path, _)| dep_path.clone())
        .collect();
    for pkg in graph.packages.values_mut() {
        let mut removed = Vec::new();
        pkg.optional_dependencies.retain(|name, tail| {
            let child_key = if package_keys.contains(tail) {
                tail.clone()
            } else {
                format!("{name}@{tail}")
            };
            let keep = !ignored.contains(name) && !mismatched_packages.contains(&child_key);
            if !keep {
                removed.push(name.clone());
            }
            keep
        });
        for name in removed {
            pkg.dependencies.remove(&name);
        }
    }

    // 3. Garbage-collect unreachable packages by walking from the
    //    surviving roots.
    let mut reachable: FxHashSet<String> = FxHashSet::default();
    let mut stack: Vec<String> = Vec::new();
    for deps in graph.importers.values() {
        for dep in deps {
            stack.push(dep.dep_path.clone());
        }
    }
    while let Some(dep_path) = stack.pop() {
        if !reachable.insert(dep_path.clone()) {
            continue;
        }
        if let Some(pkg) = graph.packages.get(&dep_path) {
            for (name, tail) in &pkg.dependencies {
                // Different lockfile readers use different conventions
                // for dependency values: the pnpm reader stores the
                // dep_path *tail* (`"1.2.3"`), while the npm/yarn/bun
                // readers store the full dep_path (`"foo@1.2.3"`).
                // Try the raw value first, then the pnpm-style
                // reconstruction.
                if graph.packages.contains_key(tail) {
                    stack.push(tail.clone());
                } else {
                    let child_key = format!("{name}@{tail}");
                    if graph.packages.contains_key(&child_key) {
                        stack.push(child_key);
                    }
                }
            }
        }
    }
    graph.packages.retain(|k, _| reachable.contains(k));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|x| (*x).to_string()).collect()
    }

    #[test]
    fn empty_fields_accept_any_host() {
        let sup = SupportedArchitectures::default();
        assert!(is_supported(&[], &[], &[], &sup));
    }

    #[test]
    fn positive_match_rules() {
        assert!(field_matches(&s(&["linux", "darwin"]), "linux"));
        assert!(!field_matches(&s(&["linux", "darwin"]), "win32"));
    }

    #[test]
    fn negation_rejects_match() {
        assert!(!field_matches(&s(&["!win32"]), "win32"));
        assert!(field_matches(&s(&["!win32"]), "linux"));
    }

    #[test]
    fn mixed_negation_and_positive() {
        // Negation takes precedence: even if a positive also matches,
        // hitting a negation rejects.
        assert!(!field_matches(&s(&["linux", "!linux"]), "linux"));
    }

    #[test]
    fn supported_architectures_widens_with_current() {
        // `["current", "linux"]` should accept the host *or* linux.
        let sup = SupportedArchitectures {
            os: s(&["current", "linux"]),
            ..Default::default()
        };
        // A linux-only package passes regardless of host.
        assert!(is_supported(&s(&["linux"]), &[], &[], &sup));
    }

    #[test]
    fn aube_lock_default_accepts_every_common_native() {
        // A first-time `aube install` run on macOS must still pull the
        // Linux / Windows native variants into `aube-lock.yaml` so the
        // committed lockfile installs cleanly on CI — that's the whole
        // point of the wide default.
        let sup = SupportedArchitectures::aube_lock_default();
        assert!(is_supported(&s(&["darwin"]), &s(&["arm64"]), &[], &sup));
        assert!(is_supported(
            &s(&["linux"]),
            &s(&["x64"]),
            &s(&["glibc"]),
            &sup
        ));
        assert!(is_supported(
            &s(&["linux"]),
            &s(&["arm64"]),
            &s(&["musl"]),
            &sup
        ));
        assert!(is_supported(&s(&["win32"]), &s(&["x64"]), &[], &sup));
        assert!(is_supported(&s(&["win32"]), &s(&["arm64"]), &[], &sup));
    }

    #[test]
    fn aube_lock_default_always_accepts_host_triple() {
        // The wide matrix excludes darwin-x64 by design (see
        // `aube_lock_default`'s doc), but the host's own triple is
        // unconditionally added to the set. An Intel Mac or a freebsd
        // box that runs `aube install` must still get its own native
        // optional deps, regardless of which combinations the baseline
        // matrix covers.
        let sup = SupportedArchitectures::aube_lock_default();
        let (os, cpu, libc) = host_triple();
        let pkg_libc = if libc.is_empty() { vec![] } else { s(&[libc]) };
        assert!(is_supported(&s(&[os]), &s(&[cpu]), &pkg_libc, &sup));
    }

    #[test]
    fn aube_lock_default_rejects_exotic_non_host_triples() {
        // Exotic triples the host isn't running as (openbsd, ppc64, …)
        // stay out of the default set — users targeting them still need
        // to opt in via `pnpm.supportedArchitectures`.
        let sup = SupportedArchitectures::aube_lock_default();
        let (os, _, _) = host_triple();
        if os != "openbsd" {
            assert!(!is_supported(&s(&["openbsd"]), &s(&["x64"]), &[], &sup));
        }
        if os != "aix" {
            assert!(!is_supported(&s(&["aix"]), &s(&["ppc64"]), &[], &sup));
        }
    }

    #[test]
    fn filter_graph_prunes_transitive_optional_platform_mismatches() {
        let supported = SupportedArchitectures {
            os: s(&["darwin"]),
            cpu: s(&["arm64"]),
            ..Default::default()
        };
        let mut graph = aube_lockfile::LockfileGraph::default();
        graph.importers.insert(
            ".".to_string(),
            vec![aube_lockfile::DirectDep {
                name: "host".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dep_type: aube_lockfile::DepType::Production,
                specifier: Some("1.0.0".to_string()),
            }],
        );
        graph.packages.insert(
            "host@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "host".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "host@1.0.0".to_string(),
                dependencies: [
                    ("native-darwin".to_string(), "1.0.0".to_string()),
                    ("native-linux".to_string(), "1.0.0".to_string()),
                ]
                .into(),
                optional_dependencies: [
                    ("native-darwin".to_string(), "1.0.0".to_string()),
                    ("native-linux".to_string(), "1.0.0".to_string()),
                ]
                .into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-darwin@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "native-darwin".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native-darwin@1.0.0".to_string(),
                os: s(&["darwin"]).into(),
                cpu: s(&["arm64"]).into(),
                ..Default::default()
            },
        );
        graph.packages.insert(
            "native-linux@1.0.0".to_string(),
            aube_lockfile::LockedPackage {
                name: "native-linux".to_string(),
                version: "1.0.0".to_string(),
                dep_path: "native-linux@1.0.0".to_string(),
                os: s(&["linux"]).into(),
                cpu: s(&["x64"]).into(),
                ..Default::default()
            },
        );

        filter_graph(&mut graph, &supported, &Default::default());

        let host = graph.packages.get("host@1.0.0").unwrap();
        assert!(host.dependencies.contains_key("native-darwin"));
        assert!(!host.dependencies.contains_key("native-linux"));
        assert!(graph.packages.contains_key("native-darwin@1.0.0"));
        assert!(!graph.packages.contains_key("native-linux@1.0.0"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn libc_ignored_off_linux() {
        // On a non-Linux host, a package that declares libc=musl
        // should still pass — npm only enforces libc on Linux.
        let sup = SupportedArchitectures::default();
        assert!(is_supported(&[], &[], &s(&["musl"]), &sup));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_glibc_host_rejects_musl_only_package() {
        // The mirror of `libc_ignored_off_linux`: on a glibc Linux
        // host, a package that declares libc=musl must not pass.
        // Skipped on musl Linux builds, since "current" expands to
        // musl there and the package would (correctly) match.
        if cfg!(target_env = "musl") {
            return;
        }
        let sup = SupportedArchitectures::default();
        assert!(!is_supported(&[], &[], &s(&["musl"]), &sup));
    }
}
