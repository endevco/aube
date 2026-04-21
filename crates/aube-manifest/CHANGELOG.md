# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
