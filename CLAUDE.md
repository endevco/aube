# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Aube?

Aube is a fast Node.js package manager written in Rust. It mirrors pnpm's CLI surface and isolated symlink layout so users can swap it in, but it owns its own on-disk state: the global store lives in `$XDG_DATA_HOME/aube/store/` (defaulting to `~/.local/share/aube/store/`), the per-project virtual store is `node_modules/.aube/`, and the canonical lockfile is `aube-lock.yaml`. Aube reads and writes several lockfile formats — `aube-lock.yaml`, `pnpm-lock.yaml` (v9), `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock` — and preserves whatever kind is already on disk, so a project with only `pnpm-lock.yaml` keeps getting `pnpm-lock.yaml` updated. `aube-lock.yaml` is the default only when no lockfile exists yet. If a project already has a `node_modules` from another package manager, aube leaves it alone and installs into its own tree alongside — it never reaches into `.pnpm/` or `~/.pnpm-store/`. Lifecycle scripts are skipped by default for security.

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

## Commit Messages (REQUIRED)

All commit messages and PR titles MUST follow the aube convention: short,
imperative subjects with a lowercase area prefix. Aube uses **area-based**
prefixes (subsystem / crate names), *not* strict conventional-commits
`feat:` / `fix:` / `chore:` verbs — a commit touching the resolver starts
with `resolver:`, not `fix(resolver):`. Reserve `feat:` / `fix:` for the
rare case where no subsystem prefix fits.

**Format:** `<area>(<subscope>): <description>` (subscope is optional)

**Areas:**
- Crates: `cli`, `resolver`, `registry`, `lockfile`, `store`, `linker`,
  `manifest`, `scripts`, `workspace`, `settings`
- Commands / features: `install`, `add`, `publish`, `dlx`, `audit`,
  `login`, `logout`, `deprecate`, `unpublish`, `fetch`, `import`,
  `catalog`
- Infrastructure: `ci`, `docs`, `bench`, `test`, `packaging`, `release`,
  `chore`
- Generic fallbacks (only when nothing else fits): `feat`, `fix`

**Subscopes:** narrow the area when it adds useful context, e.g.
`lockfile(yarn)`, `lockfile(npm)`, `lockfile(bun)`, `bench(hermetic)`,
`ci(release-plz)`, `ci(ppa-publish)`, `docs(home)`, `docs(installation)`,
`fix(dlx)`.

**Description style:**
- Lowercase after the colon
- Imperative mood ("add feature", not "added feature")
- Concise but descriptive — for most commits the subject line is the
  whole message

**Examples (from recent history):**
- `cli: add aubr/aubx multicall shims for run and dlx`
- `resolver: preserve npm-alias as folder name on fresh resolve`
- `store: swap CAS hash from SHA-512 to BLAKE3`
- `lockfile(yarn): add yarn berry (v2+) parse + write support`
- `bench(hermetic): warm registry with every PM, pin via .npmrc`
- `docs(home): plain-English speed copy + link stats to benchmarks`
- `ci(release-plz): grant contents:write to upload-assets caller`
- `chore: release v1.0.0-beta.4`

**Do not:**
- Prefix commits or PR titles with `[codex]`, agent names, or tool
  branding.
- Include PR numbers in local commit messages; GitHub adds those
  automatically when squash-merging.
- Use `feat(<area>):` / `fix(<area>):` when a subsystem prefix exists —
  prefer `<area>:` directly.

## User support channels

Aube does not use GitHub Issues — the repo has issues disabled. All bug
reports, questions, and feature requests go through
[GitHub Discussions](https://github.com/endevco/aube/discussions). When
writing docs, error messages, or any user-facing copy that points people
somewhere to get help, link Discussions and do **not** write phrases like
"file an issue" or "open an issue."

## Architecture

Cargo workspace with 9 crates under `crates/`. The binary entry point is `crates/aube/src/main.rs`.

**Install pipeline flow:** CLI (`aube`) -> resolve deps (`aube-resolver`) -> fetch from registry (`aube-registry`) -> import tarballs into global CAS (`aube-store`) -> link into `node_modules` (`aube-linker`)

Key crates:
- **aube** — Clap-based CLI, command implementations, auto-install state tracking (`state.rs`)
- **aube-resolver** — BFS dependency resolver with semver satisfaction and packument caching
- **aube-registry** — HTTP client for npm registry (abbreviated packument format, tarball downloads)
- **aube-lockfile** — Read/write support for `aube-lock.yaml`, `pnpm-lock.yaml` (v9), `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`, and `bun.lock`. The install path preserves the existing lockfile kind via `detect_existing_lockfile_kind` (precedence: aube > pnpm > bun > yarn > npm-shrinkwrap > npm)
- **aube-store** — Content-addressable store under `$XDG_DATA_HOME/aube/store/v1/files/` (defaulting to `~/.local/share/aube/store/v1/files/`) using BLAKE3 with 2-char directory sharding. Tarball integrity is still SHA-512 because that's the registry format
- **aube-linker** — isolated symlink layout (same shape as pnpm's `node-linker=isolated`, but under `.aube/` instead of `.pnpm/`): top-level `node_modules/<name>` entries are symlinks into `.aube/<dep_path>/node_modules/<name>`. Transitive deps live as sibling symlinks inside `.aube/<dep_path>/node_modules/` so Node's directory walk finds them.
- **aube-manifest** — package.json parser with workspace glob support
- **aube-scripts** — Root-package lifecycle script runner (preinstall, install, postinstall, prepare). Dependency scripts are always skipped; the allowlist surface is designed but not yet wired
- **aube-workspace** — Discovers workspace packages from `pnpm-workspace.yaml` and resolves `workspace:` protocol links

## Key Design Decisions

- **Isolated symlink layout, aube-owned**: Top-level `node_modules/<name>` entries are symlinks into `.aube/<dep_path>/node_modules/<name>`, the same shape as pnpm's `node-linker=isolated` mode but under `.aube/` so we never collide with an existing pnpm tree. Each `.aube/<dep_path>/node_modules/` directory contains the real package and sibling symlinks to its declared dependencies, so Node's directory walk gives strict isolation. The store materializes package files via reflink (APFS/btrfs), hardlink (ext4), or copy (fallback) — only when extracting tarballs into the global virtual store, never at link time.
- **Aube-owned global store**: `$XDG_DATA_HOME/aube/store/v1/files/` (defaulting to `~/.local/share/aube/store/v1/files/`) is a BLAKE3 CAS with 2-char directory sharding. Aube never reads from or writes to `~/.pnpm-store/`.
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

The canonical benchmark harness lives at `benchmarks/bench.sh` and is
driven by `mise run bench` (or `mise run bench:bump` to refresh
`benchmarks/results.json`, which the docs site reads at build time).

Any time we run a benchmark, serialize it with `flock` against a
static lock path in a tmp directory so concurrent benchmark runs (on
the same box, across worktrees, agents, terminals) can't fight each
other for disk/CPU and skew the numbers. Use
`/tmp/aube-bench.lock` as the canonical path, and default to hermetic
mode at a fixed bandwidth so ad-hoc runs are comparable across
machines and over time:

```bash
flock /tmp/aube-bench.lock \
  env BENCH_HERMETIC=1 BENCH_BANDWIDTH=500mbit mise run bench
```

`BENCH_HERMETIC=1` removes npmjs CDN variance; `BENCH_BANDWIDTH=500mbit`
pins the simulated link at a "fast home broadband" baseline so two
runs on different ISPs / CI runners produce comparable numbers. Drop
the bandwidth cap (or raise it) for loopback-speed measurements; lower
it (e.g. `6mbit`) to see how each PM scales under a constrained link.
See the Hermetic benchmark mode section below for the full mechanics.

This applies to manual benchmark commands (hyperfine one-shots, ad-hoc
`aube install` timing loops, etc.) too. **The one exception is
`mise run bench:bump`** — never pass `BENCH_HERMETIC` / `BENCH_BANDWIDTH`
there, because `results.json` is the published "real internet"
baseline.

When `mise run bench:bump` rewrites [`benchmarks/results.json`](benchmarks/results.json),
refresh the hardcoded ratios in [`README.md`](README.md) in the same
commit. The docs site and landing page pull the numbers from
`results.json` at VitePress build time, but the README is plain text —
nothing regenerates it. The `Why Try It` section quotes both the
warm-CI multiples (pnpm, bun) and the cross-fixture ranges; recompute
them from the new JSON and update the sentence to match.

### Hermetic benchmark mode

`BENCH_HERMETIC=1 mise run bench` routes all registry traffic through a
local Verdaccio instance, so the cold-cache scenario measures aube's
code path rather than npmjs CDN jitter. Implementation lives in
[`benchmarks/hermetic.bash`](benchmarks/hermetic.bash) and
[`benchmarks/registry/`](benchmarks/registry/). The first hermetic run
warms a cache at `~/.cache/aube-bench/registry/` (one network fetch
against npmjs); every subsequent run is fully offline. Blow away that
directory to force a re-warm (e.g. after bumping packages in
`benchmarks/fixture.package.json`).

Layer `BENCH_BANDWIDTH=50mbit` (or `6mbit`, or a bare integer bytes/s)
on top to simulate a realistic internet link. Traffic is piped through
[`benchmarks/throttle-proxy.mjs`](benchmarks/throttle-proxy.mjs), a
dependency-free Node token-bucket proxy that sits between the package
managers and Verdaccio. The proxy preserves the client's `Host`
header so Verdaccio's self-referential tarball URLs also flow back
through it — without that, tarball fetches silently bypass the limit.

**Never combine `BENCH_HERMETIC` or `BENCH_BANDWIDTH` with
`bench:bump`.** [`benchmarks/results.json`](benchmarks/results.json) is
the published "real internet" baseline and must not be overwritten from
a hermetic run. The `flock /tmp/aube-bench.lock` rule still applies —
hermetic mode starts local daemons on fixed ports (4874 for Verdaccio,
4875 for the proxy), so two concurrent hermetic runs would collide.

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
