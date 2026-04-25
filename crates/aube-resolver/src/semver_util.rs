use aube_registry::Packument;

/// Outcome of [`pick_version`]. Distinguishes "nothing in the range
/// at all" from "the cutoff filtered every otherwise-satisfying
/// version" so the caller can surface a meaningful strict-mode error
/// instead of pretending the range itself was wrong.
#[derive(Debug)]
pub(crate) enum PickResult<'a> {
    Found(&'a aube_registry::VersionMetadata),
    NoMatch,
    /// Strict mode (or any caller treating the cutoff as a hard wall):
    /// at least one version satisfied the range, but all of them were
    /// filtered out by the cutoff.
    AgeGated,
}

#[cfg(test)]
impl<'a> PickResult<'a> {
    pub(crate) fn unwrap(self) -> &'a aube_registry::VersionMetadata {
        match self {
            PickResult::Found(m) => m,
            other => panic!("expected PickResult::Found, got {other:?}"),
        }
    }
}

/// Pick the best version from a packument that satisfies the given range.
///
/// `pick_lowest` flips the scan order — used by
/// `resolution-mode=time-based` for direct deps. `cutoff` filters out
/// versions whose registry publish time is later than the cutoff
/// (lexicographic compare on ISO-8601 UTC strings, which sort
/// correctly). When the packument has no `time` entry for a version
/// (e.g. abbreviated corgi payload in `Highest` mode), the cutoff is
/// ignored and the version stays eligible.
///
/// `strict` controls fallback when the cutoff filters out every
/// satisfying version: with `strict=true` we return `None` and the
/// caller errors out; with `strict=false` (the pnpm default) we make a
/// second pass that picks the *lowest* satisfying version ignoring the
/// cutoff. The lowest-satisfying fallback is pnpm's deliberate choice
/// — the oldest version in the range is least likely to be the freshly
/// pushed compromise that triggered the filter in the first place.
#[inline]
pub(crate) fn pick_version<'a>(
    packument: &'a Packument,
    range_str: &str,
    locked: Option<&str>,
    pick_lowest: bool,
    cutoff: Option<&str>,
    strict: bool,
) -> PickResult<'a> {
    // Handle dist-tag references. If the requested range is a tag
    // name and the packument has that tag, use the tagged version
    // as the effective range. Special case `latest`: some registries
    // serve packuments where dist-tags.latest is absent (fresh
    // publish race, all versions deprecated, private mirror bug).
    // Old code then tried to parse "latest" as a semver range,
    // failed, returned NoMatch. Caller could not tell whether the
    // range was genuinely unsatisfiable or the tag was just missing.
    // npm and pnpm fall back to the highest non-prerelease version.
    // Do the same so `aube install foo` does not silently fail on a
    // packument that just happens to lack the tag.
    let effective_range = if node_semver::Range::parse(normalize_range(range_str)).is_ok() {
        range_str.to_string()
    } else if let Some(tagged_version) = packument.dist_tags.get(range_str) {
        tagged_version.clone()
    } else if range_str == "latest" {
        match highest_stable_version(packument) {
            Some(v) => v,
            None => return PickResult::NoMatch,
        }
    } else {
        range_str.to_string()
    };

    let range = match node_semver::Range::parse(normalize_range(&effective_range)) {
        Ok(r) => r,
        Err(_) => return PickResult::NoMatch,
    };

    let passes_cutoff = |ver: &str| -> bool {
        let Some(c) = cutoff else { return true };
        match packument.time.get(ver) {
            Some(t) => t.as_str() <= c,
            // Missing time: keep it — we'd rather risk a slightly newer
            // transitive than fail to resolve the range entirely.
            None => true,
        }
    };

    // Prefer locked version if it satisfies and clears the cutoff.
    if let Some(locked_ver) = locked
        && let Ok(v) = node_semver::Version::parse(locked_ver)
        && v.satisfies(&range)
        && passes_cutoff(locked_ver)
        && let Some(meta) = packument.versions.get(locked_ver)
    {
        return PickResult::Found(meta);
    }

    // Track whether *any* version satisfied the range — if so but
    // every one was rejected by the cutoff, the failure is age-gate
    // related, not a real "no match in range".
    let mut had_satisfying_but_age_gated = false;

    let mut best: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> = None;
    let mut fallback_lowest: Option<(node_semver::Version, &'a aube_registry::VersionMetadata)> =
        None;

    for (ver_str, meta) in &packument.versions {
        let Ok(v) = node_semver::Version::parse(ver_str) else {
            continue;
        };
        if !v.satisfies(&range) {
            continue;
        }

        if fallback_lowest.as_ref().is_none_or(|(cur, _)| v < *cur) {
            fallback_lowest = Some((v.clone(), meta));
        }

        if passes_cutoff(ver_str) {
            let replace = best
                .as_ref()
                .is_none_or(|(cur, _)| if pick_lowest { v < *cur } else { v > *cur });
            if replace {
                best = Some((v, meta));
            }
        } else {
            had_satisfying_but_age_gated = true;
        }
    }

    if let Some((_, meta)) = best {
        return PickResult::Found(meta);
    }

    // Strict mode (or no cutoff active): give up. Distinguish age-gate
    // failures so the caller can surface a meaningful error instead of
    // pretending the range itself was wrong.
    if strict || cutoff.is_none() {
        return if had_satisfying_but_age_gated {
            PickResult::AgeGated
        } else {
            PickResult::NoMatch
        };
    }

    // Lenient fallback: pnpm's `pickPackageFromMetaUsingTime` ignores
    // the cutoff and picks the *lowest* satisfying version.
    if let Some((_, meta)) = fallback_lowest {
        return PickResult::Found(meta);
    }
    PickResult::NoMatch
}

/// Walk the packument's versions and return the highest non
/// prerelease version string. Used as the `latest` tag fallback
/// when the registry response lacks `dist-tags.latest`. Some
/// private mirrors and mid-publish races drop the tag briefly
/// and returning NoMatch there would break `aube install foo` for
/// no real reason. npm and pnpm both fall back to highest stable.
#[inline]
pub(crate) fn highest_stable_version(packument: &Packument) -> Option<String> {
    let mut best: Option<(node_semver::Version, String)> = None;
    for key in packument.versions.keys() {
        let Ok(v) = node_semver::Version::parse(key) else {
            continue;
        };
        // Skip prereleases so we match npm semantics. Registry
        // with only prereleases returns None and caller gets
        // NoMatch, same as before.
        if !v.pre_release.is_empty() {
            continue;
        }
        match &best {
            None => best = Some((v, key.clone())),
            Some((cur, _)) if v > *cur => best = Some((v, key.clone())),
            _ => {}
        }
    }
    best.map(|(_, k)| k)
}
/// Extract the trailing `@<version>` from an `npm:<name>@<version>`
/// or `jsr:<name>@<version>` alias spec. Returns the input unchanged
/// when the spec isn't an alias or doesn't carry a version tail.
#[inline]
pub(crate) fn strip_alias_prefix(range: &str) -> &str {
    for prefix in ["npm:", "jsr:"] {
        if let Some(rest) = range.strip_prefix(prefix) {
            return match rest.rfind('@') {
                Some(at) if at > 0 => &rest[at + 1..],
                _ => rest,
            };
        }
    }
    range
}

#[inline]
pub(crate) fn version_satisfies(version: &str, range_str: &str) -> bool {
    let Ok(v) = node_semver::Version::parse(version) else {
        return false;
    };
    with_cached_range(normalize_range(range_str), |r| match r {
        Some(r) => v.satisfies(r),
        None => false,
    })
}

/// npm / pnpm / yarn all treat an empty or whitespace-only version
/// range as equivalent to `"*"` (match any). `node_semver` rejects it
/// with `No valid ranges could be parsed`. Normalize here so the
/// resolver and every `version_satisfies` caller agree with the
/// upstream registry semantics. Real-world case: `hashring@0.0.8`
/// declares `"bisection": ""` in its dependencies.
pub(crate) fn normalize_range(range_str: &str) -> &str {
    if range_str.trim().is_empty() {
        "*"
    } else {
        range_str
    }
}

/// Thread-local `node_semver::Range` parse cache.
///
/// Resolver hot loops (sibling dedupe, lockfile-reuse scan,
/// peer-context fixed-point, catalog pick) call `version_satisfies`
/// thousands of times against a small repeating range set
/// (`"^18.2.0"`, `"*"`, `"1.x"`). Re-parsing burns CPU. Memo turns
/// 15k reparses on a 500-pkg graph into ~500 parses plus hits.
///
/// `thread_local!` beats a global mutex. Each tokio worker owns its
/// slice of ranges, lock contention would erase the parse savings.
/// Two workers parsing the same range twice is cheaper than one
/// lock round-trip.
fn with_cached_range<R>(range_str: &str, f: impl FnOnce(Option<&node_semver::Range>) -> R) -> R {
    thread_local! {
        static CACHE: std::cell::RefCell<rustc_hash::FxHashMap<String, Option<node_semver::Range>>> =
            std::cell::RefCell::default();
    }
    CACHE.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(range_str) {
            let parsed = node_semver::Range::parse(range_str).ok();
            map.insert(range_str.to_string(), parsed);
        }
        f(map.get(range_str).and_then(Option::as_ref))
    })
}
