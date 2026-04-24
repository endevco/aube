use std::path::PathBuf;

pub fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        return Some(h.into());
    }
    #[cfg(windows)]
    if let Some(h) = std::env::var_os("USERPROFILE") {
        return Some(h.into());
    }
    None
}

fn non_empty_path_var(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

pub fn xdg_config_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CONFIG_HOME")
}

pub fn xdg_data_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_DATA_HOME")
}

pub fn xdg_cache_home() -> Option<PathBuf> {
    non_empty_path_var("XDG_CACHE_HOME")
}
