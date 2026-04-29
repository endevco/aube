# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.5.0](https://github.com/endevco/aube/compare/aube-util-v1.4.0...aube-util-v1.5.0) - 2026-04-29

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/endevco/aube/pull/384))

## [1.4.0](https://github.com/endevco/aube/compare/aube-util-v1.3.0...aube-util-v1.4.0) - 2026-04-28

### Fixed

- *(packaging)* include README on published aube crate ([#349](https://github.com/endevco/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/endevco/aube/pull/368))

## [1.3.0](https://github.com/endevco/aube/compare/aube-util-v1.2.1...aube-util-v1.3.0) - 2026-04-27

### Fixed

- *(lockfile)* parse scalar pnpm platform fields ([#337](https://github.com/endevco/aube/pull/337))

## [1.2.0](https://github.com/endevco/aube/compare/aube-util-v1.1.0...aube-util-v1.2.0) - 2026-04-25

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/endevco/aube/pull/296))

## [1.1.0](https://github.com/endevco/aube/compare/aube-util-v1.0.0...aube-util-v1.1.0) - 2026-04-24

### Fixed

- *(linker)* strip windows verbatim prefix before diffing bin-shim paths ([#275](https://github.com/endevco/aube/pull/275))

### Other

- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
