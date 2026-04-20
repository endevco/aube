# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/v1.0.0-beta.8...v1.0.0-beta.9) - 2026-04-20

### Other

- reject path-traversing bin names and targets ([#162](https://github.com/endevco/aube/pull/162))
- wipe node_modules when global virtual store toggles ([#160](https://github.com/endevco/aube/pull/160))
- render package.json parse errors with miette source span ([#157](https://github.com/endevco/aube/pull/157))
- *(config)* add --local shortcut for --location project ([#161](https://github.com/endevco/aube/pull/161))
- silence peer-dep mismatches by default (bun parity) ([#158](https://github.com/endevco/aube/pull/158))
- *(troubleshooting)* lead with disable-gvs as first step ([#156](https://github.com/endevco/aube/pull/156))
- short-circuit warm path when install-state matches ([#127](https://github.com/endevco/aube/pull/127))
- create scoped bin shim parents ([#149](https://github.com/endevco/aube/pull/149))
- emit colored stderr under CI even when not a TTY ([#146](https://github.com/endevco/aube/pull/146))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/v1.0.0-beta.7...v1.0.0-beta.8) - 2026-04-20

### Other

- rewrite gvs auto-disable warning in plain English ([#140](https://github.com/endevco/aube/pull/140))
- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/endevco/aube/pull/129))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/v1.0.0-beta.6...v1.0.0-beta.7) - 2026-04-19

### Other

- write per-dep .bin for transitive lifecycle-script bins ([#122](https://github.com/endevco/aube/pull/122))
- make workspace warm installs incremental ([#110](https://github.com/endevco/aube/pull/110))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/endevco/aube/pull/107))
- drop webpack and rollup from gvs auto-disable defaults ([#117](https://github.com/endevco/aube/pull/117))
- registry + install: tolerate napi-rs packuments and warn on ignored builds ([#113](https://github.com/endevco/aube/pull/113))
- include bun.lock in --lockfile removal set ([#105](https://github.com/endevco/aube/pull/105))
- fix --version / -V on aubr and aubx multicall shims ([#106](https://github.com/endevco/aube/pull/106))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/v1.0.0-beta.5...v1.0.0-beta.6) - 2026-04-19

### Other

- widen disableGlobalVirtualStoreForPackages default list ([#101](https://github.com/endevco/aube/pull/101))
- widen aube-lock.yaml to every common platform ([#94](https://github.com/endevco/aube/pull/94))
- split into frozen/settings/side_effects_cache submodules ([#88](https://github.com/endevco/aube/pull/88))
- *(progress)* split ci-mode state into own module ([#87](https://github.com/endevco/aube/pull/87))
- move install state to node_modules/.aube-state ([#80](https://github.com/endevco/aube/pull/80))
- Fix two aube install issues on real RN monorepos ([#82](https://github.com/endevco/aube/pull/82))
- exit silently on ctrl-c at script picker ([#81](https://github.com/endevco/aube/pull/81))

## [1.0.0-beta.5](https://github.com/endevco/aube/compare/v1.0.0-beta.4...v1.0.0-beta.5) - 2026-04-19

### Other

- pluralize counted nouns in CLI output ([#70](https://github.com/endevco/aube/pull/70))
- use strum derives for Severity and NodeLinker ([#69](https://github.com/endevco/aube/pull/69))
- keep filtered workspace installs rooted ([#67](https://github.com/endevco/aube/pull/67))
- accept registry flag on install ([#63](https://github.com/endevco/aube/pull/63))
- add global gvs override ([#61](https://github.com/endevco/aube/pull/61))

## [1.0.0-beta.4](https://github.com/endevco/aube/compare/v1.0.0-beta.3...v1.0.0-beta.4) - 2026-04-19

### Other

- discover root catalogs via package.json workspaces field ([#56](https://github.com/endevco/aube/pull/56))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/v1.0.0-beta.2...v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/endevco/aube/pull/19))

### Fixed

- *(dlx)* resolve bin from installed package when names differ ([#25](https://github.com/endevco/aube/pull/25))
- verifyDepsBeforeRun fires when node_modules is removed ([#23](https://github.com/endevco/aube/pull/23))

### Other

- discover from workspace root + package.json sources ([#44](https://github.com/endevco/aube/pull/44))
- AUBE_DEBUG/AUBE_LOG replace RUST_LOG for log control ([#43](https://github.com/endevco/aube/pull/43))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/endevco/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/endevco/aube/pull/35))
- clarify packageManagerStrict rejection message ([#40](https://github.com/endevco/aube/pull/40))
- swap CAS hash from SHA-512 to BLAKE3 ([#36](https://github.com/endevco/aube/pull/36))
- auto-disable global virtual store for packages known to break on it ([#32](https://github.com/endevco/aube/pull/32))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/endevco/aube/pull/30))
- print "Already up to date" on a no-op install ([#17](https://github.com/endevco/aube/pull/17))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/v1.0.0-beta.1...v1.0.0-beta.2) - 2026-04-18

### Other

- update Cargo.toml dependencies
