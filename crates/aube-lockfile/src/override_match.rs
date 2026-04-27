//! Minimal override-key matcher for the importer-level drift check.
//!
//! pnpm rewrites an importer's recorded `specifier` when an override
//! fires on a direct dep — so a manifest that reads `"plist": "^3.0.4"`
//! with override `"plist@<3.0.5": ">=3.0.5"` produces a lockfile that
//! records `specifier: ">=3.0.5"`. `--frozen-lockfile` must apply the
//! same override to the manifest spec before comparing, otherwise
//! every pnpm-written lockfile with overrides reads stale on the next
//! frozen install.
//!
//! The full pnpm/yarn override grammar (parent chains `foo>bar`, yarn
//! wildcards `**/foo`) lives in `aube-resolver::override_rule`. Direct
//! deps of an importer have no ancestor chain by construction, so this
//! matcher only handles the two key shapes that can fire here:
//!
//! - bare name: `lodash`, `@babel/core`
//! - name + version range: `lodash@<4.17.21`, `@scope/pkg@^1`
//!
//! Keys with parent-chain syntax are ignored — they can't match a
//! direct-dep override application.
//!
//! Kept inside aube-lockfile (rather than reaching into aube-resolver)
//! to avoid a cross-crate dep cycle: aube-resolver already depends on
//! aube-lockfile.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct DirectOverrideRule {
    pub name: String,
    pub version_req: Option<String>,
    pub replacement: String,
}

/// Parse and compile a raw `name → replacement` map into rules. Keys
/// with parent-chain selectors (containing `>` or `/`, except for the
/// scope `/`) are dropped — they only match transitive deps.
pub(crate) fn compile(raw: &BTreeMap<String, String>) -> Vec<DirectOverrideRule> {
    raw.iter()
        .filter_map(|(k, v)| {
            parse_key(k).map(|(n, r)| DirectOverrideRule {
                name: n,
                version_req: r,
                replacement: v.clone(),
            })
        })
        .collect()
}

/// Find the first rule whose target matches `(name, spec)` and return
/// its replacement spec. A rule matches when (a) the target name is
/// equal and (b) either the rule has no version req, or the manifest
/// spec's lower-bound version satisfies the rule's req — same probe
/// `aube-resolver::override_rule` uses.
pub(crate) fn apply<'a>(
    rules: &'a [DirectOverrideRule],
    name: &str,
    spec: &str,
) -> Option<&'a str> {
    rules.iter().find_map(|rule| {
        if rule.name != name {
            return None;
        }
        match rule.version_req.as_deref() {
            None => Some(rule.replacement.as_str()),
            Some(req) if range_could_satisfy(spec, req) => Some(rule.replacement.as_str()),
            _ => None,
        }
    })
}

fn parse_key(key: &str) -> Option<(String, Option<String>)> {
    if key.is_empty() || key.contains('>') {
        return None;
    }
    if let Some(after_at) = key.strip_prefix('@') {
        let slash = after_at.find('/')?;
        let scope = &after_at[..slash];
        let rest = &after_at[slash + 1..];
        if rest.is_empty() || rest.contains('/') {
            return None;
        }
        match rest.find('@') {
            Some(0) => None,
            Some(i) => {
                let pkg_tail = &rest[..i];
                let req = &rest[i + 1..];
                if pkg_tail.is_empty() || req.is_empty() {
                    return None;
                }
                Some((format!("@{scope}/{pkg_tail}"), Some(req.to_string())))
            }
            None => Some((format!("@{scope}/{rest}"), None)),
        }
    } else if key.contains('/') {
        None
    } else if let Some(at) = key.find('@') {
        if at == 0 {
            return None;
        }
        let name = &key[..at];
        let req = &key[at + 1..];
        if name.is_empty() || req.is_empty() {
            return None;
        }
        Some((name.to_string(), Some(req.to_string())))
    } else {
        Some((key.to_string(), None))
    }
}

/// Lower-bound probe. Mirrors `aube-resolver::override_rule::range_could_satisfy`
/// without the cross-crate dep. A range whose extractable lower bound
/// satisfies the req counts as a hit. Ranges we can't parse fall through
/// to "probably matches" so a user override is never silently dropped.
fn range_could_satisfy(task_range: &str, req: &str) -> bool {
    let Ok(r) = node_semver::Range::parse(req) else {
        return true;
    };
    if let Ok(v) = node_semver::Version::parse(task_range)
        && v.satisfies(&r)
    {
        return true;
    }
    if let Some(candidate) = lower_bound_version(task_range)
        && let Ok(v) = node_semver::Version::parse(&candidate)
    {
        return v.satisfies(&r);
    }
    true
}

fn lower_bound_version(range: &str) -> Option<String> {
    let s = range
        .trim()
        .trim_start_matches(['^', '~', '=', '>', 'v', ' ']);
    let end = s.find([' ', ',', '<', '|', '>']).unwrap_or(s.len());
    let v = &s[..end];
    if v.is_empty() || !v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn bare_name_matches_any_spec() {
        let rules = compile(&map(&[("lodash", "4.17.21")]));
        assert_eq!(apply(&rules, "lodash", "^4.17.0"), Some("4.17.21"));
        assert_eq!(apply(&rules, "lodash", "*"), Some("4.17.21"));
        assert_eq!(apply(&rules, "other", "^1"), None);
    }

    #[test]
    fn scoped_bare_name() {
        let rules = compile(&map(&[("@babel/core", "7.20.0")]));
        assert_eq!(apply(&rules, "@babel/core", "^7"), Some("7.20.0"));
    }

    #[test]
    fn version_qualified_filters_by_range() {
        let rules = compile(&map(&[("plist@<3.0.5", ">=3.0.5")]));
        assert_eq!(apply(&rules, "plist", "^3.0.4"), Some(">=3.0.5"));
        assert_eq!(apply(&rules, "plist", "^4.0.0"), None);
    }

    #[test]
    fn scoped_with_range() {
        let rules = compile(&map(&[("@scope/pkg@^1", "1.5.0")]));
        assert_eq!(apply(&rules, "@scope/pkg", "^1.0.0"), Some("1.5.0"));
        assert_eq!(apply(&rules, "@scope/pkg", "^2.0.0"), None);
    }

    #[test]
    fn parent_chain_keys_dropped() {
        let rules = compile(&map(&[
            ("foo>bar", "1.0.0"),
            ("**/foo", "1.0.0"),
            ("parent/foo", "1.0.0"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn empty_or_malformed_keys_dropped() {
        let rules = compile(&map(&[
            ("", "1"),
            ("@scope", "1"),
            ("foo@", "1"),
            ("@", "1"),
        ]));
        assert!(rules.is_empty());
    }

    #[test]
    fn first_matching_rule_wins() {
        let rules = compile(&map(&[("plist", "9.9.9"), ("plist@<3", "2.0.0")]));
        assert_eq!(apply(&rules, "plist", "^3"), Some("9.9.9"));
    }
}
