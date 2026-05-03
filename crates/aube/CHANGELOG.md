# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.7.0](https://github.com/endevco/aube/compare/v1.6.2...v1.7.0) - 2026-05-03

### Added

- *(cli)* support link: and file: specs in aube add ([#487](https://github.com/endevco/aube/pull/487))
- *(cli)* support yaml-only workspace roots in list/run/install/query/why ([#486](https://github.com/endevco/aube/pull/486))
- *(cli)* support git specs in aube add ([#483](https://github.com/endevco/aube/pull/483))
- *(cli)* rewrite manifest specifier on update without --latest ([#479](https://github.com/endevco/aube/pull/479))
- *(cli)* aube rebuild <pkg> targets a specific package ([#477](https://github.com/endevco/aube/pull/477))
- *(install)* persist unreviewed-builds warning across repeat installs ([#476](https://github.com/endevco/aube/pull/476))
- *(cli)* warn when aube update --depth is set ([#473](https://github.com/endevco/aube/pull/473))

### Fixed

- *(cli)* wrap doc comments so -h help stays one line per flag ([#478](https://github.com/endevco/aube/pull/478))
- *(install)* allow workspace members without `version` field ([#480](https://github.com/endevco/aube/pull/480))
- *(resolver)* resolve nested link:/file: deps from local parents and overrides ([#470](https://github.com/endevco/aube/pull/470))
- *(lockfile)* parse bare user/repo as github shorthand ([#472](https://github.com/endevco/aube/pull/472))

### Other

- refresh benchmarks for v1.6.2 ([#474](https://github.com/endevco/aube/pull/474))
- streaming sha512, parallel cas, tls prewarm, fetch reorder ([#469](https://github.com/endevco/aube/pull/469))
- refresh benchmarks for v1.6.2 ([#467](https://github.com/endevco/aube/pull/467))

## [1.6.2](https://github.com/endevco/aube/compare/v1.6.1...v1.6.2) - 2026-05-01

### Added

- *(cli)* check engines.{aube,pnpm} and workspace per-project engines ([#458](https://github.com/endevco/aube/pull/458))

## [1.6.1](https://github.com/endevco/aube/compare/v1.6.0...v1.6.1) - 2026-05-01

### Other

- refresh benchmarks for v1.5.2 ([#459](https://github.com/endevco/aube/pull/459))

## [1.6.0](https://github.com/endevco/aube/compare/v1.5.1...v1.6.0) - 2026-05-01

### Added

- *(cli)* aube update parses <pkg>@<spec> args + accepts indirect deps ([#446](https://github.com/endevco/aube/pull/446))
- *(cli)* add generic --config.<key>=<value> flags ([#447](https://github.com/endevco/aube/pull/447))
- *(cli)* emit pnpm's verbatim error for empty --allow-build values ([#444](https://github.com/endevco/aube/pull/444))
- *(pnpmfile)* emit ctx.log records as pnpm:hook ndjson on stdout ([#440](https://github.com/endevco/aube/pull/440))
- *(cli)* add --pnpmfile and --global-pnpmfile flags ([#439](https://github.com/endevco/aube/pull/439))
- *(cli)* add --lockfile-dir / lockfileDir setting ([#431](https://github.com/endevco/aube/pull/431))
- *(cli)* add --fetch-timeout / --fetch-retries / retry backoff flags ([#436](https://github.com/endevco/aube/pull/436))
- *(pnpmfile)* wire hooks into update; add preResolution hook ([#423](https://github.com/endevco/aube/pull/423))
- --save-catalog, workspace:* parsing, and sharedWorkspaceLockfile=false ([#418](https://github.com/endevco/aube/pull/418))
- *(cli)* aube add bootstraps package.json + 10 misc.ts ports ([#417](https://github.com/endevco/aube/pull/417))

### Fixed

- *(cli)* honor AUBE_VIRTUAL_STORE_DIR env var + port 5 more pnpm/misc tests ([#456](https://github.com/endevco/aube/pull/456))
- *(cli)* aube update --latest preserves higher-than-latest prerelease pins ([#445](https://github.com/endevco/aube/pull/445))
- *(cli)* reject `.` as a foreign --lockfile-dir importer; correct docs ([#442](https://github.com/endevco/aube/pull/442))
- *(scripts)* close 3 lifecycle parity gaps with pnpm ([#421](https://github.com/endevco/aube/pull/421))
- *(cli)* honor full gitignore semantics in pack/publish ([#411](https://github.com/endevco/aube/pull/411))
- *(dlx)* pick .cmd shim on Windows so bin runs without --shell-mode ([#401](https://github.com/endevco/aube/pull/401))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/endevco/aube/pull/394))

### Other

- *(cli)* port pnpm monorepo filter tests + wire --fail-if-no-match ([#457](https://github.com/endevco/aube/pull/457))
- cache hot-path work across install, resolver, and registry ([#453](https://github.com/endevco/aube/pull/453))
- refresh benchmarks for v1.5.2 ([#452](https://github.com/endevco/aube/pull/452))
- dedupe and cache hot-path work in install and resolver ([#449](https://github.com/endevco/aube/pull/449))
- refresh benchmarks for v1.5.2 ([#448](https://github.com/endevco/aube/pull/448))
- *(install)* port four allowBuilds review tests from pnpm lifecycleScripts.ts ([#441](https://github.com/endevco/aube/pull/441))
- *(install)* port pnpm/test/update.ts (13/22) ([#438](https://github.com/endevco/aube/pull/438))
- refresh benchmarks for v1.5.1 ([#426](https://github.com/endevco/aube/pull/426))
- release v1.5.2 ([#389](https://github.com/endevco/aube/pull/389))
- *(resolver)* add bundled metadata primer ([#397](https://github.com/endevco/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/endevco/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/endevco/aube/pull/392))

## [1.5.2](https://github.com/endevco/aube/compare/v1.5.1...v1.5.2) - 2026-04-30

### Fixed

- *(cli)* honor full gitignore semantics in pack/publish ([#411](https://github.com/endevco/aube/pull/411))
- *(dlx)* pick .cmd shim on Windows so bin runs without --shell-mode ([#401](https://github.com/endevco/aube/pull/401))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/endevco/aube/pull/394))

### Other

- *(resolver)* add bundled metadata primer ([#397](https://github.com/endevco/aube/pull/397))
- thank Namespace for GitHub Actions runner support ([#412](https://github.com/endevco/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/endevco/aube/pull/392))

## [1.5.0](https://github.com/endevco/aube/compare/v1.4.0...v1.5.0) - 2026-04-29

### Added

- *(cli)* add dependency graph query command ([#380](https://github.com/endevco/aube/pull/380))

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/endevco/aube/pull/384))
- *(workspace)* default-write aube-workspace.yaml instead of pnpm-workspace.yaml ([#382](https://github.com/endevco/aube/pull/382))
- *(resolver)* bound resolved package stream ([#377](https://github.com/endevco/aube/pull/377))

### Other

- *(bench)* add install phase timings ([#381](https://github.com/endevco/aube/pull/381))

## [1.4.0](https://github.com/endevco/aube/compare/v1.3.0...v1.4.0) - 2026-04-28

### Added

- *(audit)* support update fix mode ([#363](https://github.com/endevco/aube/pull/363))
- *(install)* adopt pnpm 11 allowBuilds reviews ([#364](https://github.com/endevco/aube/pull/364))
- *(pnpmfile)* support esm pnpmfiles ([#362](https://github.com/endevco/aube/pull/362))
- *(scripts)* enforce build jails on linux ([#350](https://github.com/endevco/aube/pull/350))

### Fixed

- *(npm)* preserve extensionless bin shims ([#369](https://github.com/endevco/aube/pull/369))
- roundup of critical/high audit findings ([#361](https://github.com/endevco/aube/pull/361))
- *(resolver)* exclude provenance churn packages ([#360](https://github.com/endevco/aube/pull/360))
- *(linker)* link workspace bins into dependent packages ([#353](https://github.com/endevco/aube/pull/353))
- *(packaging)* include README on published aube crate ([#349](https://github.com/endevco/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/endevco/aube/pull/368))

## [1.3.0](https://github.com/endevco/aube/compare/v1.2.1...v1.3.0) - 2026-04-27

### Added

- *(config)* add settings discovery ([#347](https://github.com/endevco/aube/pull/347))
- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/endevco/aube/pull/333))
- *(scripts)* add jailed dependency builds ([#306](https://github.com/endevco/aube/pull/306))

### Fixed

- *(resolver)* accept abbreviated git commit SHAs in user specs ([#346](https://github.com/endevco/aube/pull/346))
- *(lockfile)* preserve package and bun lock compatibility ([#339](https://github.com/endevco/aube/pull/339))
- *(registry)* surface retry warnings and cap timeout retries at 1 ([#331](https://github.com/endevco/aube/pull/331))
- bun.lock parity for workspaces, platforms, and locked versions ([#327](https://github.com/endevco/aube/pull/327))

### Other

- *(add)* drop redundant pre-install resolve, use FrozenMode::Fix ([#348](https://github.com/endevco/aube/pull/348))
- *(install)* skip unused dep bin links ([#343](https://github.com/endevco/aube/pull/343))
- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/endevco/aube/pull/340))

## [1.2.1](https://github.com/endevco/aube/compare/v1.2.0...v1.2.1) - 2026-04-26

### Fixed

- *(add)* preserve package manifest field order ([#315](https://github.com/endevco/aube/pull/315))

### Other

- *(resolver)* avoid full packuments for aged metadata ([#314](https://github.com/endevco/aube/pull/314))

## [1.2.0](https://github.com/endevco/aube/compare/v1.1.0...v1.2.0) - 2026-04-25

### Added

- *(cli)* mise-style --version + scope update notifier to version commands ([#301](https://github.com/endevco/aube/pull/301))
- *(cli)* add short command aliases ([#299](https://github.com/endevco/aube/pull/299))

### Fixed

- support git url specs in dlx and parser ([#295](https://github.com/endevco/aube/pull/295))
- *(install)* link bins with mixed metadata ([#300](https://github.com/endevco/aube/pull/300))
- cross-platform install correctness pass ([#293](https://github.com/endevco/aube/pull/293))
- *(install)* restore missing lockfile from install state ([#289](https://github.com/endevco/aube/pull/289))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/endevco/aube/pull/296))

## [1.1.0](https://github.com/endevco/aube/compare/v1.0.0...v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/endevco/aube/pull/273))
- *(install)* support global approve-builds ([#274](https://github.com/endevco/aube/pull/274))
- *(scripts)* run pack/publish/version lifecycle hooks ([#262](https://github.com/endevco/aube/pull/262))

### Fixed

- *(store)* speed up cold installs ([#267](https://github.com/endevco/aube/pull/267))
- *(linker)* strip windows verbatim prefix before diffing bin-shim paths ([#275](https://github.com/endevco/aube/pull/275))
- *(publish)* report post-hook name/version in PublishOutcome ([#272](https://github.com/endevco/aube/pull/272))
- *(global)* strip Windows \\?\\ verbatim prefix from canonicalized install dir ([#243](https://github.com/endevco/aube/pull/243))

### Other

- *(install)* split warm freshness state ([#271](https://github.com/endevco/aube/pull/271))
- avoid duplicate warm state reads ([#266](https://github.com/endevco/aube/pull/266))
- use warm path in frozen mode ([#264](https://github.com/endevco/aube/pull/264))
- always shim self-bin so CI artifact round-trips work ([#259](https://github.com/endevco/aube/pull/259))
- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- resolve catalog: in overrides + honor override-rewritten importer specs ([#249](https://github.com/endevco/aube/pull/249))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0](https://github.com/endevco/aube/compare/v1.0.0-beta.12...v1.0.0) - 2026-04-23

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
