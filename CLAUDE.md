# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Pre-1.0: pnpm parity

Until aube ships 1.0 and hits drop-in parity with pnpm, this section
calls out workflow rules that only apply during the parity push. Once
we're done, delete the whole section.

- **Every new CLI arg or flag must update two docs in the same PR**:
  - [`README.md`](README.md) — the user-facing feature comparison table
    (command row + any relevant flag row). If a command's notes column
    is stale, fix it while you're there.
  - [`CLI_SPEC.md`](CLI_SPEC.md) — the per-flag parity tracker. Flip
    the status cell from ❌/🟡 to ✅ when you land the flag, and add
    new rows for any pnpm flag the PR exposes for the first time.
  Think of both docs as part of the CLI surface: a PR that skips them
  leaves future contributors unsure whether a flag is shipped,
  half-wired, or intentionally deferred.
- **Keep `aube.usage.kdl` in sync**. Run `cargo build && ./target/debug/aube usage > aube.usage.kdl`
  after any clap change. The golden test in `crates/aube/src/main.rs`
  fails loudly if you forget.
- **Hide aliases that aube makes redundant**. Commands we only keep for
  pnpm muscle memory (e.g. `install-test`, `ll`, `la`) should be
  `#[command(hide = true)]` so `aube --help` doesn't grow noise, and
  the README row should explain *why* they're hidden. Feature-complete
  commands stay visible.
- **Every new command needs a BATS test.** The offline Verdaccio
  fixture registry (`test/registry/`) exists so these tests can run
  without network access; see the testing section below for the
  recipe to add a fixture package.

## What is Aube?

Aube is a fast Node.js package manager written in Rust. It mirrors pnpm's CLI surface and isolated symlink layout so users can swap it in, but it owns its own on-disk state: the global store lives in `~/.aube-store/`, the per-project virtual store is `node_modules/.aube/`, and the canonical lockfile is `aube-lock.yaml`. Aube reads and writes several lockfile formats — `aube-lock.yaml`, `pnpm-lock.yaml` (v9), `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock` — and preserves whatever kind is already on disk, so a project with only `pnpm-lock.yaml` keeps getting `pnpm-lock.yaml` updated. `aube-lock.yaml` is the default only when no lockfile exists yet. If a project already has a `node_modules` from another package manager, aube leaves it alone and installs into its own tree alongside — it never reaches into `.pnpm/` or `~/.pnpm-store/`. Lifecycle scripts are skipped by default for security.

## Common Commands

```bash
cargo build                    # Build the `aube` binary
cargo test                     # Run unit tests
cargo clippy --all-targets -- -D warnings  # Lint (CI enforces zero warnings)
cargo fmt --check              # Check formatting

# BATS integration tests (requires Node.js 22 and GNU parallel).
# `cargo build` first, then run the mise task — it shards across cores
# via `bats --jobs`, so prefer it over raw `./test/bats/bin/bats`.
mise run test:bats                       # Run the full suite in parallel
mise run test:bats test/install.bats     # Run a single file (or several)
```

## Commit Messages

Match the existing history: short, imperative subjects with an optional
lowercase area prefix.

- Preferred shape: `area: do the thing`
- Use a scoped area when it adds useful context, e.g.
  `docs(installation): add cargo install aube` or
  `ci(release-plz): grant contents:write to upload-assets caller`.
- Good recent examples: `cli: add aubr/aubx multicall shims for run and dlx`,
  `publish: ship aube on npm as @endevco/aube`, and
  `release: use cross + rustls-tls for linux targets`.
- Do not prefix commits or PR titles with `[codex]`, agent names, or tool
  branding.
- Do not include PR numbers in local commit messages; GitHub may add those
  when squash-merging.

## Architecture

Cargo workspace with 9 crates under `crates/`. The binary entry point is `crates/aube/src/main.rs`.

**Install pipeline flow:** CLI (`aube`) -> resolve deps (`aube-resolver`) -> fetch from registry (`aube-registry`) -> import tarballs into global CAS (`aube-store`) -> link into `node_modules` (`aube-linker`)

Key crates:
- **aube** — Clap-based CLI, command implementations, auto-install state tracking (`state.rs`)
- **aube-resolver** — BFS dependency resolver with semver satisfaction and packument caching
- **aube-registry** — HTTP client for npm registry (abbreviated packument format, tarball downloads)
- **aube-lockfile** — Read/write support for `aube-lock.yaml`, `pnpm-lock.yaml` (v9), `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock`. The install path preserves the existing lockfile kind via `detect_existing_lockfile_kind` (precedence: aube > pnpm > bun > yarn > npm-shrinkwrap > npm)
- **aube-store** — Content-addressable store under `~/.aube-store/v1/files/` using SHA-512 with 2-char directory sharding
- **aube-linker** — isolated symlink layout (same shape as pnpm's `node-linker=isolated`, but under `.aube/` instead of `.pnpm/`): top-level `node_modules/<name>` entries are symlinks into `.aube/<dep_path>/node_modules/<name>`. Transitive deps live as sibling symlinks inside `.aube/<dep_path>/node_modules/` so Node's directory walk finds them.
- **aube-manifest** — package.json parser with workspace glob support
- **aube-scripts** — Root-package lifecycle script runner (preinstall, install, postinstall, prepare). Dependency scripts are always skipped; the allowlist surface is designed but not yet wired
- **aube-workspace** — Discovers workspace packages from `pnpm-workspace.yaml` and resolves `workspace:` protocol links

## Key Design Decisions

- **Isolated symlink layout, aube-owned**: Top-level `node_modules/<name>` entries are symlinks into `.aube/<dep_path>/node_modules/<name>`, the same shape as pnpm's `node-linker=isolated` mode but under `.aube/` so we never collide with an existing pnpm tree. Each `.aube/<dep_path>/node_modules/` directory contains the real package and sibling symlinks to its declared dependencies, so Node's directory walk gives strict isolation. The store materializes package files via reflink (APFS/btrfs), hardlink (ext4), or copy (fallback) — only when extracting tarballs into the global virtual store, never at link time.
- **Aube-owned global store**: `~/.aube-store/v1/files/` is a SHA-512 CAS with 2-char directory sharding. Aube never reads from or writes to `~/.pnpm-store/`.
- **Lockfile format**: `aube-lock.yaml` is the canonical format and the default when no lockfile exists yet. When a project already has a supported lockfile (`pnpm-lock.yaml` v9, `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, or `bun.lock`), aube reads it and — via `write_lockfile_preserving_existing` — writes back that same kind rather than leaving a surprise `aube-lock.yaml` alongside it. Package indices are cached per version in `~/.cache/aube/index/`. Packument metadata is cached in `~/.cache/aube/packuments-v1/` with a 5-minute TTL fast path and ETag/Last-Modified revalidation beyond it.
- **Co-existence with other package managers**: If `node_modules` was built by another pm, aube leaves it alone — no detect-and-wipe, no reuse. Our tree goes into our own `.aube/` virtual store alongside whatever's already there.
- **Auto-install**: Tracks hashes of lockfile + package.json in `node_modules/.aube-state` to detect staleness.

## Output and Progress UI

Install-time progress is built on `clx::progress` (see `crates/aube/src/progress.rs`). While a progress bar is active, **never** call `eprintln!` / `println!` / `print!` / `write!(stderr, ...)` directly — the output will collide with the animated display and corrupt the terminal. Instead:

- In `aube`, route user-visible messages through `crate::progress::println(prog_ref, msg)`. It calls `ProgressJob::println` (which pauses the render, writes the line, and resumes) when a bar is active, and falls back to plain `eprintln` when it isn't.
- Lifecycle scripts and anything that writes directly to stdout/stderr (child processes, `println!` in tools we don't control) must run either *before* `InstallProgress::try_new` or *after* `InstallProgress::finish`. In `install::run`, `preinstall` runs before the progress UI is constructed; `install` / `postinstall` / `prepare` and the final summary run after `finish`.
- `tracing::*` logging is fine — tracing writes through its own path and clx handles it correctly.
- When adding a new install phase, call `InstallProgress::set_phase` so the header reflects what's running.

## Benchmarks

Any time we run a benchmark, serialize it with `flock` against a
static lock path in a tmp directory so concurrent benchmark runs (on
the same box, across worktrees, agents, terminals) can't fight each
other for disk/CPU and skew the numbers. Use
`/tmp/aube-bench.lock` as the canonical path and wrap the actual
benchmark invocation, e.g.:

```bash
flock /tmp/aube-bench.lock bench/run.sh
```

`bench/run.sh` already self-wraps with `flock` when invoked directly,
but manual benchmark commands (hyperfine one-shots, ad-hoc
`aube install` timing loops, etc.) must take the lock too.

## Rust Configuration

- Edition: 2024
- Async runtime: Tokio
- Error handling: `miette` for user-facing errors, `thiserror` for library errors
- CI matrix: macOS + Ubuntu

## Testing

BATS integration tests live in `test/`. The BATS runner and assertion
libraries are vendored under `test/bats/` and `test/test_helper/`, so a
plain clone has everything needed to run the suite. Fixtures in `fixtures/`
(basic: is-odd/is-even, medium: benchmark project). Each test gets an
isolated temp directory and HOME.

**Offline Verdaccio fixture registry:** `test/registry/` runs a local Verdaccio instance during the BATS suite. It's configured with *no uplink* — all packages are served from the committed `test/registry/storage/` directory, so the test suite runs without any network access to npmjs.org. To add a new package to the fixture set, temporarily restore the `npmjs` uplink in `test/registry/config.yaml`, run the test that needs the new package, then remove the uplink again and commit the new files under `test/registry/storage/`. See `CONTRIBUTING.md` for the exact recipe.
