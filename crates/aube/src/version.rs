use std::sync::LazyLock;

/// User-facing version string. Appends `-DEBUG` on non-release builds so
/// a stray `cargo run` binary on `$PATH` is obvious in `aube --version`
/// and the install progress header.
pub static VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut v = env!("CARGO_PKG_VERSION").to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    v
});
