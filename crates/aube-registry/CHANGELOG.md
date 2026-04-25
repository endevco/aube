# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.0](https://github.com/endevco/aube/compare/aube-registry-v1.1.0...aube-registry-v1.2.0) - 2026-04-25

### Added

- *(registry)* make packument + tarball body caps configurable, raise packument default to 200 MiB ([#282](https://github.com/endevco/aube/pull/282))

### Fixed

- cross-platform install correctness pass ([#293](https://github.com/endevco/aube/pull/293))

### Security

- cve-class hardening across linker, registry, resolver, install ([#296](https://github.com/endevco/aube/pull/296))

## [1.1.0](https://github.com/endevco/aube/compare/aube-registry-v1.0.0...aube-registry-v1.1.0) - 2026-04-24

### Other

- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.11...aube-registry-v1.0.0-beta.12) - 2026-04-22

### Other

- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- cross-crate security hardening ([#202](https://github.com/endevco/aube/pull/202))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.11](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.10...aube-registry-v1.0.0-beta.11) - 2026-04-21

### Other

- retry cold fetch body decode errors ([#189](https://github.com/endevco/aube/pull/189))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.9...aube-registry-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

### Other

- strip matched surrounding quotes from .npmrc values ([#182](https://github.com/endevco/aube/pull/182))
- parse cached full packuments directly ([#184](https://github.com/endevco/aube/pull/184))
- increase packument cache ttl for repeat installs ([#173](https://github.com/endevco/aube/pull/173))

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.8...aube-registry-v1.0.0-beta.9) - 2026-04-20

### Other

- short-circuit warm path when install-state matches ([#127](https://github.com/endevco/aube/pull/127))
- tolerate string engines metadata ([#150](https://github.com/endevco/aube/pull/150))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.7...aube-registry-v1.0.0-beta.8) - 2026-04-20

### Other

- quiet retry warnings; settings: kebab-case gvs npmrc aliases ([#139](https://github.com/endevco/aube/pull/139))
- tolerate legacy array engines shape in packuments ([#138](https://github.com/endevco/aube/pull/138))
- *(auth)* longest-prefix .npmrc lookup with default-port stripping ([#131](https://github.com/endevco/aube/pull/131))
- honor NPM_CONFIG_USERCONFIG for user-level .npmrc path ([#130](https://github.com/endevco/aube/pull/130))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.6...aube-registry-v1.0.0-beta.7) - 2026-04-19

### Other

- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/endevco/aube/pull/107))
- registry + install: tolerate napi-rs packuments and warn on ignored builds ([#113](https://github.com/endevco/aube/pull/113))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.5...aube-registry-v1.0.0-beta.6) - 2026-04-19

### Other

- gate slow-tarball warning on elapsed > 1s to match pnpm ([#93](https://github.com/endevco/aube/pull/93))
- gate tokenHelper to user scope and sanitize the value ([#89](https://github.com/endevco/aube/pull/89))
- tolerate object-valued dep-map entries in packuments ([#92](https://github.com/endevco/aube/pull/92))
- url-encode scoped names and expand packument accept header ([#83](https://github.com/endevco/aube/pull/83))
- tolerate null values in packument string maps ([#76](https://github.com/endevco/aube/pull/76))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.2...aube-registry-v1.0.0-beta.3) - 2026-04-19

### Added

- *(cli)* support jsr: specifier protocol ([#19](https://github.com/endevco/aube/pull/19))

### Other

- honor npm_config_* env vars in NpmConfig::load ([#47](https://github.com/endevco/aube/pull/47))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-registry-v1.0.0-beta.1...aube-registry-v1.0.0-beta.2) - 2026-04-18

### Other

- use cross + rustls-tls for linux targets ([#15](https://github.com/endevco/aube/pull/15))
- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
