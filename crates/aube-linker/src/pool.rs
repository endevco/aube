use tracing::warn;

pub fn default_linker_parallelism() -> usize {
    let default_limit = if cfg!(target_os = "macos") { 4 } else { 16 };

    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(default_limit)
}

type LinkPoolCache = std::sync::Mutex<Vec<(usize, std::sync::Arc<rayon::ThreadPool>)>>;
static LINK_POOL_CACHE: std::sync::OnceLock<LinkPoolCache> = std::sync::OnceLock::new();

fn link_pool(threads: usize) -> Option<std::sync::Arc<rayon::ThreadPool>> {
    let cache = LINK_POOL_CACHE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = cache.lock().ok()?;
    if let Some((_, pool)) = guard.iter().find(|(t, _)| *t == threads) {
        return Some(pool.clone());
    }
    match rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("aube-linker-{i}"))
        .build()
    {
        Ok(pool) => {
            let pool = std::sync::Arc::new(pool);
            guard.push((threads, pool.clone()));
            Some(pool)
        }
        Err(err) => {
            warn!("failed to build aube linker thread pool: {err}; falling back to caller thread");
            None
        }
    }
}

pub(crate) fn with_link_pool<R: Send>(threads: usize, f: impl FnOnce() -> R + Send) -> R {
    match link_pool(threads) {
        Some(pool) => pool.install(f),
        None => f(),
    }
}
