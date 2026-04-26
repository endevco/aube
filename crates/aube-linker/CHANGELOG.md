# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.1](https://github.com/endevco/aube/compare/aube-linker-v1.2.0...aube-linker-v1.2.1) - 2026-04-26

### Fixed

- *(linker)* skip self-named deps regardless of version ([#321](https://github.com/endevco/aube/pull/321))

## [1.2.0](https://github.com/endevco/aube/compare/aube-linker-v1.1.0...aube-linker-v1.2.0) - 2026-04-25

### Fixed

- cross-platform install correctness pass ([#293](https://github.com/endevco/aube/pull/293))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/endevco/aube/pull/296))

## [1.1.0](https://github.com/endevco/aube/compare/aube-linker-v1.0.0...aube-linker-v1.1.0) - 2026-04-24

### Fixed

- *(store)* speed up cold installs ([#267](https://github.com/endevco/aube/pull/267))
- *(linker)* strip windows verbatim prefix before diffing bin-shim paths ([#275](https://github.com/endevco/aube/pull/275))

### Other

- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- copy small files instead of reflinking ([#251](https://github.com/endevco/aube/pull/251))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.12...aube-linker-v1.0.0) - 2026-04-23

### Other

- windows install correctness + workspace filter fixes ([#229](https://github.com/endevco/aube/pull/229))
- speed up babylon warm reinstalls ([#224](https://github.com/endevco/aube/pull/224))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.11...aube-linker-v1.0.0-beta.12) - 2026-04-22

### Other

- include integrity in package index cache key ([#209](https://github.com/endevco/aube/pull/209))
- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- cross-crate security hardening ([#202](https://github.com/endevco/aube/pull/202))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.11](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.10...aube-linker-v1.0.0-beta.11) - 2026-04-21

### Other

- skip pnpm v9 virtual importers in workspace link passes ([#190](https://github.com/endevco/aube/pull/190))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.9...aube-linker-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.8...aube-linker-v1.0.0-beta.9) - 2026-04-20

### Other

- reject path-traversing bin names and targets ([#162](https://github.com/endevco/aube/pull/162))
- create scoped bin shim parents ([#149](https://github.com/endevco/aube/pull/149))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.6...aube-linker-v1.0.0-beta.7) - 2026-04-19

### Other

- make workspace warm installs incremental ([#110](https://github.com/endevco/aube/pull/110))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.5...aube-linker-v1.0.0-beta.6) - 2026-04-19

### Other

- reject traversing and non-regular tar entries on import ([#85](https://github.com/endevco/aube/pull/85))
- sanitize shebang interpreter before shim interpolation ([#84](https://github.com/endevco/aube/pull/84))

## [1.0.0-beta.5](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.4...aube-linker-v1.0.0-beta.5) - 2026-04-19

### Other

- use strum derives for Severity and NodeLinker ([#69](https://github.com/endevco/aube/pull/69))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.2...aube-linker-v1.0.0-beta.3) - 2026-04-19

### Other

- auto-disable global virtual store for packages known to break on it ([#32](https://github.com/endevco/aube/pull/32))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/endevco/aube/pull/30))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-linker-v1.0.0-beta.1...aube-linker-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
