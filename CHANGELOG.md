# Changelog

## [Unreleased]

### Changed
- `aube approve-builds` (and the install-time auto-seed of unreviewed
  build scripts) now creates `aube-workspace.yaml` when no workspace
  yaml exists in the project, parallel to the `aube-lock.yaml` shape
  already used for the lockfile. Existing `pnpm-workspace.yaml` files
  are still read and mutated in place — backward-compat with
  pnpm-style projects is unchanged. Closes the asymmetry where the
  lockfile got the `aube-` prefix but the workspace yaml didn't.

## [v1.0.0-beta.2](https://github.com/endevco/aube/releases/tag/v1.0.0-beta.2) - 2026-04-18

### Added
- npm distribution: install aube globally via `npm install -g @endevco/aube` with platform-specific binary packages for macOS, Linux, and Windows (arm64 + x64) ([#12](https://github.com/endevco/aube/pull/12))

### Changed
- Switched TLS backend from system OpenSSL (`native-tls`) to pure-Rust `rustls-tls`, eliminating the OpenSSL system dependency and fixing cross-compilation failures on `aarch64-unknown-linux-gnu` ([#15](https://github.com/endevco/aube/pull/15))
- Linux release binaries are now built with `cross`, providing broader glibc compatibility across Linux distributions ([#15](https://github.com/endevco/aube/pull/15))

### Fixed
- Fixed per-registry client certificate authentication to work with the rustls TLS backend by using combined PEM format (`Identity::from_pem`) instead of the native-tls-only `Identity::from_pkcs8_pem` ([#15](https://github.com/endevco/aube/pull/15))
