# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0](https://github.com/endevco/aube/compare/aube-store-v1.0.0...aube-store-v1.1.0) - 2026-04-24

### Fixed

- *(store)* speed up cold installs ([#267](https://github.com/endevco/aube/pull/267))

### Other

- accept legacy sha1/sha256/sha384 integrity in verify_integrity ([#263](https://github.com/endevco/aube/pull/263))
- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- copy small files instead of reflinking ([#251](https://github.com/endevco/aube/pull/251))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.11...aube-store-v1.0.0-beta.12) - 2026-04-22

### Other

- include integrity in package index cache key ([#209](https://github.com/endevco/aube/pull/209))
- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- skip pkg-content version check for URL-shaped lockfile entries ([#203](https://github.com/endevco/aube/pull/203))
- cross-crate security hardening ([#202](https://github.com/endevco/aube/pull/202))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.9...aube-store-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.7...aube-store-v1.0.0-beta.8) - 2026-04-20

### Other

- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/endevco/aube/pull/129))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.5...aube-store-v1.0.0-beta.6) - 2026-04-19

### Other

- skip PAX global/extension tar headers ([#100](https://github.com/endevco/aube/pull/100))
- tolerate leading `v` in tarball package.json version ([#95](https://github.com/endevco/aube/pull/95))
- reject traversing and non-regular tar entries on import ([#85](https://github.com/endevco/aube/pull/85))
- cap tarball decompression to prevent gzip-bomb dos ([#79](https://github.com/endevco/aube/pull/79))
- reject dash-prefixed urls and commits passed to git ([#75](https://github.com/endevco/aube/pull/75))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.2...aube-store-v1.0.0-beta.3) - 2026-04-19

### Other

- swap CAS hash from SHA-512 to BLAKE3 ([#36](https://github.com/endevco/aube/pull/36))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-store-v1.0.0-beta.1...aube-store-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
