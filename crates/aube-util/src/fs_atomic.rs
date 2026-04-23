use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

pub fn sibling_tempdir(final_path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut name: OsString = final_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("aube-tmp"));
    name.push(format!(".tmp.{pid}.{nanos}.{n}"));
    match final_path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

pub fn rename_with_retry(src: &Path, dst: &Path) -> io::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut backoff_ms = 20u64;
    for attempt in 0..MAX_ATTEMPTS {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !is_transient(&err) || attempt == MAX_ATTEMPTS - 1 {
                    if dst.exists() {
                        let _ = std::fs::remove_dir_all(src).or_else(|_| std::fs::remove_file(src));
                        return Ok(());
                    }
                    return Err(err);
                }
                thread::sleep(Duration::from_millis(backoff_ms));
                backoff_ms = backoff_ms.saturating_mul(2);
            }
        }
    }
    Err(io::Error::other("rename_with_retry exhausted attempts"))
}

fn is_transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::AlreadyExists
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    )
}

pub fn atomic_write(final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = sibling_tempdir(final_path);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        let _ = f.sync_all();
    }
    match rename_with_retry(&tmp, final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

pub mod sentinel {
    use super::*;

    pub fn mark(path: &Path, tag: &str) -> io::Result<bool> {
        match std::fs::read(path) {
            Ok(existing) if existing.as_slice() == tag.as_bytes() => Ok(false),
            Ok(_) => {
                write_sentinel(path, tag)?;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_sentinel(path, tag)?;
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    pub fn read_tag(path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    pub fn present(path: &Path) -> bool {
        std::fs::metadata(path).is_ok()
    }

    fn write_sentinel(path: &Path, tag: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_tempdir_differs_per_call() {
        let base = Path::new("/tmp/aube-fake-final");
        let a = sibling_tempdir(base);
        let b = sibling_tempdir(base);
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
    }

    #[test]
    fn atomic_write_replaces_existing() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join("target.bin");
        atomic_write(&path, b"first")?;
        assert_eq!(std::fs::read(&path)?, b"first");
        atomic_write(&path, b"second")?;
        assert_eq!(std::fs::read(&path)?, b"second");
        Ok(())
    }

    #[test]
    fn atomic_write_creates_parent() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join("nested/deep/file.txt");
        atomic_write(&path, b"ok")?;
        assert_eq!(std::fs::read(&path)?, b"ok");
        Ok(())
    }

    #[test]
    fn sentinel_roundtrip() -> io::Result<()> {
        let dir = tempdir()?;
        let path = dir.join(".aube-initialized");
        assert!(!sentinel::present(&path));
        assert!(sentinel::mark(&path, "v1")?);
        assert!(sentinel::present(&path));
        assert!(!sentinel::mark(&path, "v1")?);
        assert_eq!(sentinel::read_tag(&path).as_deref(), Some(b"v1".as_ref()));
        assert!(sentinel::mark(&path, "v2")?);
        assert_eq!(sentinel::read_tag(&path).as_deref(), Some(b"v2".as_ref()));
        Ok(())
    }

    fn tempdir() -> io::Result<PathBuf> {
        let base = std::env::temp_dir().join(format!(
            "aube-util-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base)?;
        Ok(base)
    }
}
