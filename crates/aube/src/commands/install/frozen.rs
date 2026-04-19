/// How `aube install` should treat an existing lockfile relative to the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrozenMode {
    /// Hard-fail if the lockfile drifts from the manifest. Default in CI.
    Frozen,
    /// Use the lockfile when it's fresh, re-resolve when it's stale. Default outside CI.
    Prefer,
    /// Always re-resolve, never trust the lockfile.
    No,
    /// Re-resolve, but seed the resolver with the existing lockfile so
    /// unchanged specs keep their pinned versions and only drifted
    /// entries get re-resolved. Corresponds to `--fix-lockfile`.
    Fix,
}

/// Global (top-level) `--frozen-lockfile` / `--no-frozen-lockfile` /
/// `--prefer-frozen-lockfile` values threaded in from `Cli`. Mirrors
/// pnpm's "accepted on every command" semantics.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalFrozenFlags {
    pub frozen: bool,
    pub no_frozen: bool,
    pub prefer_frozen: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct GlobalVirtualStoreFlags {
    pub enable: bool,
    pub disable: bool,
}

impl GlobalVirtualStoreFlags {
    pub fn to_cli_flag_bag(self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if self.enable {
            out.push((
                "enable-global-virtual-store".to_string(),
                "true".to_string(),
            ));
        }
        if self.disable {
            out.push((
                "disable-global-virtual-store".to_string(),
                "false".to_string(),
            ));
        }
        out
    }

    pub fn is_set(self) -> bool {
        self.enable || self.disable
    }
}

impl FrozenMode {
    /// Resolve the user's flag combination to a single mode.
    /// `--frozen-lockfile` and `--no-frozen-lockfile` and `--prefer-frozen-lockfile`
    /// are mutually exclusive (clap enforces this), so at most one is true.
    /// If none are set, honor `preferFrozenLockfile` from the workspace
    /// config; otherwise fall back to the env-aware default.
    pub fn from_flags(
        frozen: bool,
        no_frozen: bool,
        prefer_frozen: bool,
        yaml_prefer_frozen: Option<bool>,
    ) -> Self {
        if frozen {
            Self::Frozen
        } else if no_frozen {
            Self::No
        } else if prefer_frozen {
            Self::Prefer
        } else {
            match yaml_prefer_frozen {
                Some(true) => Self::Prefer,
                Some(false) => Self::No,
                None => Self::default_for_env(),
            }
        }
    }

    /// pnpm's default: `frozen-lockfile=true` in CI, `prefer-frozen-lockfile=true` otherwise.
    fn default_for_env() -> Self {
        if std::env::var_os("CI").is_some() {
            Self::Frozen
        } else {
            Self::Prefer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_frozen_beats_yaml() {
        let m = FrozenMode::from_flags(true, false, false, Some(false));
        assert!(matches!(m, FrozenMode::Frozen));
    }

    #[test]
    fn yaml_prefer_true_maps_to_prefer() {
        let m = FrozenMode::from_flags(false, false, false, Some(true));
        assert!(matches!(m, FrozenMode::Prefer));
    }

    #[test]
    fn yaml_prefer_false_maps_to_no() {
        let m = FrozenMode::from_flags(false, false, false, Some(false));
        assert!(matches!(m, FrozenMode::No));
    }
}
