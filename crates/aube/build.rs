fn main() {
    // Windows defaults the main-thread stack to 1 MB. The `async_main`
    // dispatcher's future state machine — which holds locals across every
    // await in every CLI command arm — exceeds that on debug builds and
    // crashes startup with `thread 'main' has overflowed its stack`.
    // Bump the reserve to 8 MB (matching Linux/macOS) via the MSVC
    // linker's `/STACK:` flag. No-op on every other target.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
