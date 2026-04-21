# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
