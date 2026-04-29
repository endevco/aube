# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.5.0](https://github.com/endevco/aube/compare/aube-manifest-v1.4.0...aube-manifest-v1.5.0) - 2026-04-29

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/endevco/aube/pull/384))
- *(workspace)* default-write aube-workspace.yaml instead of pnpm-workspace.yaml ([#382](https://github.com/endevco/aube/pull/382))

## [1.4.0](https://github.com/endevco/aube/compare/aube-manifest-v1.3.0...aube-manifest-v1.4.0) - 2026-04-28

### Added

- *(install)* adopt pnpm 11 allowBuilds reviews ([#364](https://github.com/endevco/aube/pull/364))
- *(pnpmfile)* support esm pnpmfiles ([#362](https://github.com/endevco/aube/pull/362))

### Fixed

- *(packaging)* include README on published aube crate ([#349](https://github.com/endevco/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/endevco/aube/pull/368))

## [1.3.0](https://github.com/endevco/aube/compare/aube-manifest-v1.2.1...aube-manifest-v1.3.0) - 2026-04-27

### Added

- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/endevco/aube/pull/333))
- *(scripts)* add jailed dependency builds ([#306](https://github.com/endevco/aube/pull/306))

### Other

- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/endevco/aube/pull/340))

## [1.2.0](https://github.com/endevco/aube/compare/aube-manifest-v1.1.0...aube-manifest-v1.2.0) - 2026-04-25

### Fixed

- *(scripts)* don't fabricate pnpm-workspace.yaml on approve-builds ([#303](https://github.com/endevco/aube/pull/303))

## [1.1.0](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0...aube-manifest-v1.1.0) - 2026-04-24

### Other

- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.11...aube-manifest-v1.0.0-beta.12) - 2026-04-22

### Other

- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.9...aube-manifest-v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/endevco/aube/pull/175))
- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/endevco/aube/pull/181))
- support $name references in overrides ([#180](https://github.com/endevco/aube/pull/180))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/endevco/aube/pull/170))
- read top-level trustedDependencies as allow-source ([#172](https://github.com/endevco/aube/pull/172))
- render parse errors with miette source span ([#166](https://github.com/endevco/aube/pull/166))

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.8...aube-manifest-v1.0.0-beta.9) - 2026-04-20

### Other

- render package.json parse errors with miette source span ([#157](https://github.com/endevco/aube/pull/157))
- tolerate non-string entries in scripts map ([#155](https://github.com/endevco/aube/pull/155))
- short-circuit warm path when install-state matches ([#127](https://github.com/endevco/aube/pull/127))
- tolerate string engines metadata ([#150](https://github.com/endevco/aube/pull/150))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.7...aube-manifest-v1.0.0-beta.8) - 2026-04-20

### Other

- *(npm)* tolerate legacy array engines field ([#132](https://github.com/endevco/aube/pull/132))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.6...aube-manifest-v1.0.0-beta.7) - 2026-04-19

### Other

- tolerate legacy-array engines field ([#120](https://github.com/endevco/aube/pull/120))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.5...aube-manifest-v1.0.0-beta.6) - 2026-04-19

### Other

- widen disableGlobalVirtualStoreForPackages default list ([#101](https://github.com/endevco/aube/pull/101))
- accept aube.* as alias for pnpm.* config keys ([#97](https://github.com/endevco/aube/pull/97))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.2...aube-manifest-v1.0.0-beta.3) - 2026-04-19

### Other

- discover from workspace root + package.json sources ([#44](https://github.com/endevco/aube/pull/44))
- auto-disable global virtual store for packages known to break on it ([#32](https://github.com/endevco/aube/pull/32))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-manifest-v1.0.0-beta.1...aube-manifest-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
- move settings.toml into aube-settings; pin per-crate include lists ([#4](https://github.com/endevco/aube/pull/4))
