# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
