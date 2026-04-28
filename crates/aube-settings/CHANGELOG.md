# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.4.0](https://github.com/endevco/aube/compare/aube-settings-v1.3.0...aube-settings-v1.4.0) - 2026-04-28

### Added

- *(install)* adopt pnpm 11 allowBuilds reviews ([#364](https://github.com/endevco/aube/pull/364))
- *(pnpmfile)* support esm pnpmfiles ([#362](https://github.com/endevco/aube/pull/362))
- *(scripts)* enforce build jails on linux ([#350](https://github.com/endevco/aube/pull/350))

### Fixed

- *(resolver)* exclude provenance churn packages ([#360](https://github.com/endevco/aube/pull/360))
- *(packaging)* include README on published aube crate ([#349](https://github.com/endevco/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/endevco/aube/pull/368))

## [1.3.0](https://github.com/endevco/aube/compare/aube-settings-v1.2.1...aube-settings-v1.3.0) - 2026-04-27

### Added

- *(security)* enforce trustPolicy by default, add paranoid bundle, security docs ([#333](https://github.com/endevco/aube/pull/333))
- *(scripts)* add jailed dependency builds ([#306](https://github.com/endevco/aube/pull/306))

### Other

- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/endevco/aube/pull/340))

## [1.2.1](https://github.com/endevco/aube/compare/aube-settings-v1.2.0...aube-settings-v1.2.1) - 2026-04-26

### Fixed

- *(registry)* raise fetch timeout default ([#323](https://github.com/endevco/aube/pull/323))
- *(install)* keep transitive peers out of root modules ([#316](https://github.com/endevco/aube/pull/316))

## [1.2.0](https://github.com/endevco/aube/compare/aube-settings-v1.1.0...aube-settings-v1.2.0) - 2026-04-25

### Added

- *(settings)* declare env aliases in registry ([#294](https://github.com/endevco/aube/pull/294))
- *(registry)* make packument + tarball body caps configurable, raise packument default to 200 MiB ([#282](https://github.com/endevco/aube/pull/282))

## [1.1.0](https://github.com/endevco/aube/compare/aube-settings-v1.0.0...aube-settings-v1.1.0) - 2026-04-24

### Other

- accept legacy sha1/sha256/sha384 integrity in verify_integrity ([#263](https://github.com/endevco/aube/pull/263))
- default publicHoistPattern to match pnpm ([#258](https://github.com/endevco/aube/pull/258))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.11...aube-settings-v1.0.0-beta.12) - 2026-04-22

### Other

- make packageManagerStrict a tri-state, default warn ([#213](https://github.com/endevco/aube/pull/213))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.9...aube-settings-v1.0.0-beta.10) - 2026-04-21

### Fixed

- close remaining audit findings across registry, store, and linker ([#164](https://github.com/endevco/aube/pull/164))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/endevco/aube/pull/181))
- scope deprecation warnings + add `aube deprecations` ([#170](https://github.com/endevco/aube/pull/170))

## [1.0.0-beta.9](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.8...aube-settings-v1.0.0-beta.9) - 2026-04-20

### Other

- *(settings)* collapse per-source pages into single reference ([#159](https://github.com/endevco/aube/pull/159))
- silence peer-dep mismatches by default (bun parity) ([#158](https://github.com/endevco/aube/pull/158))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.7...aube-settings-v1.0.0-beta.8) - 2026-04-20

### Other

- quiet retry warnings; settings: kebab-case gvs npmrc aliases ([#139](https://github.com/endevco/aube/pull/139))
- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/endevco/aube/pull/129))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.6...aube-settings-v1.0.0-beta.7) - 2026-04-19

### Other

- drop webpack and rollup from gvs auto-disable defaults ([#117](https://github.com/endevco/aube/pull/117))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.5...aube-settings-v1.0.0-beta.6) - 2026-04-19

### Other

- widen disableGlobalVirtualStoreForPackages default list ([#101](https://github.com/endevco/aube/pull/101))
- auto-synthesize kebab/camel npmrc key aliases ([#99](https://github.com/endevco/aube/pull/99))
- gate slow-tarball warning on elapsed > 1s to match pnpm ([#93](https://github.com/endevco/aube/pull/93))
- split into frozen/settings/side_effects_cache submodules ([#88](https://github.com/endevco/aube/pull/88))
- move install state to node_modules/.aube-state ([#80](https://github.com/endevco/aube/pull/80))
- Fix two aube install issues on real RN monorepos ([#82](https://github.com/endevco/aube/pull/82))

## [1.0.0-beta.5](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.4...aube-settings-v1.0.0-beta.5) - 2026-04-19

### Other

- remove settings count sentence ([#64](https://github.com/endevco/aube/pull/64))
- add global gvs override ([#61](https://github.com/endevco/aube/pull/61))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.2...aube-settings-v1.0.0-beta.3) - 2026-04-19

### Other

- auto-disable global virtual store for packages known to break on it ([#32](https://github.com/endevco/aube/pull/32))
- drop transitional implemented/since fields ahead of 1.0 ([#33](https://github.com/endevco/aube/pull/33))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-settings-v1.0.0-beta.1...aube-settings-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
