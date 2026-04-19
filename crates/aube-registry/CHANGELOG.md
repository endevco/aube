# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
