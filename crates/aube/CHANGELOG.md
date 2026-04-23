# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.13](https://github.com/endevco/aube/compare/v1.0.0-beta.12...v1.0.0-beta.13) - 2026-04-23

### Other

- split lib.rs into focused modules ([#235](https://github.com/endevco/aube/pull/235))
- split mod.rs into bin_linking/git_prepare/lifecycle submodules ([#237](https://github.com/endevco/aube/pull/237))
- *(delta)* invalidate changed no-gvs subtrees ([#233](https://github.com/endevco/aube/pull/233))
- link importer's own bin into node_modules/.bin ([#230](https://github.com/endevco/aube/pull/230))
- windows install correctness + workspace filter fixes ([#229](https://github.com/endevco/aube/pull/229))
- *(pack)* drop CHANGELOG from always-included files ([#227](https://github.com/endevco/aube/pull/227))
- *(install)* show transfer rate + elapsed timer in progress bars ([#225](https://github.com/endevco/aube/pull/225))
- speed up babylon warm reinstalls ([#224](https://github.com/endevco/aube/pull/224))
- *(install)* fix node-gyp bootstrap walkup causing bats parallel hang ([#220](https://github.com/endevco/aube/pull/220))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/v1.0.0-beta.11...v1.0.0-beta.12) - 2026-04-22

### Other

- anchor aube install at workspace root from member subdir ([#217](https://github.com/endevco/aube/pull/217))
- apply dependency policy in add/remove/update/dedupe resolver ([#218](https://github.com/endevco/aube/pull/218))
- compare packageManagerStrictVersion against user-facing version ([#216](https://github.com/endevco/aube/pull/216))
- anchor auto-install freshness check at workspace root ([#215](https://github.com/endevco/aube/pull/215))
- make packageManagerStrict a tri-state, default warn ([#213](https://github.com/endevco/aube/pull/213))
- append -DEBUG to version on non-release builds ([#212](https://github.com/endevco/aube/pull/212))
- include integrity in package index cache key ([#209](https://github.com/endevco/aube/pull/209))
- bootstrap node-gyp when absent from PATH ([#210](https://github.com/endevco/aube/pull/210))
- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- enrich NoMatch error with importer, chain, available versions ([#205](https://github.com/endevco/aube/pull/205))
- raise RLIMIT_NOFILE soft limit at startup ([#207](https://github.com/endevco/aube/pull/207))
- cross-crate security hardening ([#202](https://github.com/endevco/aube/pull/202))
- *(filter)* keep root importer deps in workspace selects ([#199](https://github.com/endevco/aube/pull/199))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.11](https://github.com/endevco/aube/compare/v1.0.0-beta.10...v1.0.0-beta.11) - 2026-04-21

### Other

- recognize package.json#workspaces as a workspace-root marker ([#194](https://github.com/endevco/aube/pull/194))
- verify warm-path deps from install state ([#188](https://github.com/endevco/aube/pull/188))
- warm-install speedup ([#177](https://github.com/endevco/aube/pull/177))
- short-circuit bin linking on packages with no bin metadata ([#192](https://github.com/endevco/aube/pull/192))
- warn instead of erroring on packageManager mismatch for run ([#191](https://github.com/endevco/aube/pull/191))
- skip pnpm v9 virtual importers in workspace link passes ([#190](https://github.com/endevco/aube/pull/190))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/v1.0.0-beta.9...v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/endevco/aube/pull/175))
- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/endevco/aube/pull/181))
- hint at `aube deprecations --transitive` when transitives exist ([#183](https://github.com/endevco/aube/pull/183))
- support $name references in overrides ([#180](https://github.com/endevco/aube/pull/180))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/endevco/aube/pull/170))
- read top-level trustedDependencies as allow-source ([#172](https://github.com/endevco/aube/pull/172))
- collapse install bool bags into enums, FxHashMap in resolver ([#165](https://github.com/endevco/aube/pull/165))
- render parse errors with miette source span ([#166](https://github.com/endevco/aube/pull/166))

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
