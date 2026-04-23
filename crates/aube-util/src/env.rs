pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

pub fn home_dir() -> Option<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(h.into());
    }
    #[cfg(windows)]
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Some(h.into());
    }
    None
}
