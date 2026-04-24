use std::path::PathBuf;

/// XDG-compliant cache directory for aube.
/// Uses `$XDG_CACHE_HOME/aube`, `$HOME/.cache/aube`, or `%LOCALAPPDATA%\aube` on Windows.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(xdg) = aube_util::env::xdg_cache_home() {
        return Some(xdg.join("aube"));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube"));
    }
    aube_util::env::home_dir().map(|h| h.join(".cache/aube"))
}

/// Global directory for linked packages.
/// Uses `$XDG_CACHE_HOME/aube/global-links`, `$HOME/.cache/aube/global-links`,
/// or `%LOCALAPPDATA%\aube\global-links` on Windows.
pub fn global_links_dir() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("global-links"))
}

/// Aube-owned global content-addressable store directory.
///
/// Follows the XDG Base Directory Specification: defaults to
/// `$XDG_DATA_HOME/aube/store/v1/files/`, falling back to
/// `$HOME/.local/share/aube/store/v1/files/` when `XDG_DATA_HOME` is
/// unset (or `%LOCALAPPDATA%\aube\store\v1\files` on Windows).
pub fn store_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube/store/v1/files"));
    }
    let data_home = match aube_util::env::xdg_data_home() {
        Some(xdg) => xdg,
        None => aube_util::env::home_dir()?.join(".local/share"),
    };
    Some(data_home.join("aube/store/v1/files"))
}
