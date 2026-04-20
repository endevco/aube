use std::path::PathBuf;

/// XDG-compliant cache directory for aube.
/// Uses `$XDG_CACHE_HOME/aube`, `$HOME/.cache/aube`, or `%LOCALAPPDATA%\aube` on Windows.
pub fn cache_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("aube"));
    }
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube"));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".cache/aube"))
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
/// `$XDG_DATA_HOME/aube-store/v1/files/`, falling back to
/// `$HOME/.local/share/aube-store/v1/files/` when `XDG_DATA_HOME` is
/// unset (or `%LOCALAPPDATA%\aube-store\v1\files` on Windows).
///
/// For upgraders: if the legacy path `$HOME/.aube-store/` already
/// exists, it is reused in place so an aube upgrade never silently
/// orphans a populated store. Users who want to adopt the XDG path
/// can move the directory manually; new installs go straight to the
/// XDG location.
pub fn store_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local).join("aube-store/v1/files"));
    }
    let home = std::env::var("HOME").ok()?;
    let legacy = PathBuf::from(&home).join(".aube-store");
    if legacy.is_dir() {
        return Some(legacy.join("v1/files"));
    }
    let data_home = match std::env::var("XDG_DATA_HOME") {
        Ok(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(&home).join(".local/share"),
    };
    Some(data_home.join("aube-store/v1/files"))
}
