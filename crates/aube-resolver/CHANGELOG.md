# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.1](https://github.com/endevco/aube/compare/aube-resolver-v1.2.0...aube-resolver-v1.2.1) - 2026-04-26

### Fixed

- *(install)* keep transitive peers out of root modules ([#316](https://github.com/endevco/aube/pull/316))
- pnpm snapshot round-trip + workspace negation patterns ([#312](https://github.com/endevco/aube/pull/312))

### Other

- *(resolver)* avoid full packuments for aged metadata ([#314](https://github.com/endevco/aube/pull/314))

## [1.2.0](https://github.com/endevco/aube/compare/aube-resolver-v1.1.0...aube-resolver-v1.2.0) - 2026-04-25

### Fixed

- lockfile and resolver correctness pass ([#291](https://github.com/endevco/aube/pull/291))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/endevco/aube/pull/296))

## [1.1.0](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0...aube-resolver-v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/endevco/aube/pull/273))

### Fixed

- *(resolver)* wire transitive url/git subdeps into parent snapshot ([#276](https://github.com/endevco/aube/pull/276))

### Other

- *(bun)* preserve top-level + per-entry metadata on roundtrip ([#250](https://github.com/endevco/aube/pull/250))
- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.12...aube-resolver-v1.0.0) - 2026-04-23

### Other

- split lib.rs into focused modules ([#235](https://github.com/endevco/aube/pull/235))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.11...aube-resolver-v1.0.0-beta.12) - 2026-04-22

### Other

- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- enrich NoMatch error with importer, chain, available versions ([#205](https://github.com/endevco/aube/pull/205))
- treat empty version range as `*` ([#206](https://github.com/endevco/aube/pull/206))
- allow exotic subdeps from local parents ([#201](https://github.com/endevco/aube/pull/201))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.11](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.10...aube-resolver-v1.0.0-beta.11) - 2026-04-21

### Other

- warm-install speedup ([#177](https://github.com/endevco/aube/pull/177))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.9...aube-resolver-v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/endevco/aube/pull/175))

### Other

- avoid sorting packument versions during picks ([#176](https://github.com/endevco/aube/pull/176))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/endevco/aube/pull/170))
- collapse install bool bags into enums, FxHashMap in resolver ([#165](https://github.com/endevco/aube/pull/165))

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.8...aube-resolver-v1.0.0-beta.9) - 2026-04-20

### Other

- silence peer-dep mismatches by default (bun parity) ([#158](https://github.com/endevco/aube/pull/158))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.6...aube-resolver-v1.0.0-beta.7) - 2026-04-19

### Other

- pnpm compat: multi-document lockfile + override over npm-alias ([#116](https://github.com/endevco/aube/pull/116))
- link bare-semver deps to workspace packages (yarn/npm/bun style) ([#118](https://github.com/endevco/aube/pull/118))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/endevco/aube/pull/107))
- classify bare http(s) URLs as tarballs ([#114](https://github.com/endevco/aube/pull/114))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.5...aube-resolver-v1.0.0-beta.6) - 2026-04-19

### Other

- dedupe root deps declared in multiple sections ([#102](https://github.com/endevco/aube/pull/102))
- widen aube-lock.yaml to every common platform ([#94](https://github.com/endevco/aube/pull/94))
- honor pnpm overrides "-" removal marker ([#98](https://github.com/endevco/aube/pull/98))
- extract peer-context pass into its own module ([#91](https://github.com/endevco/aube/pull/91))
- resolve catalog: indirection on override targets ([#78](https://github.com/endevco/aube/pull/78))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.2...aube-resolver-v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/endevco/aube/pull/19))

### Other

- discover from workspace root + package.json sources ([#44](https://github.com/endevco/aube/pull/44))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/endevco/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/endevco/aube/pull/35))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/endevco/aube/pull/30))
- Parse pnpm snapshot optional dependencies ([#18](https://github.com/endevco/aube/pull/18))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-resolver-v1.0.0-beta.1...aube-resolver-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
