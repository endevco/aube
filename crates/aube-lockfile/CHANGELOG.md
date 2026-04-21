# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-beta.11](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.10...aube-lockfile-v1.0.0-beta.11) - 2026-04-21

### Other

- warm-install speedup ([#177](https://github.com/endevco/aube/pull/177))
- short-circuit bin linking on packages with no bin metadata ([#192](https://github.com/endevco/aube/pull/192))

## [1.0.0-beta.10](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.9...aube-lockfile-v1.0.0-beta.10) - 2026-04-21

### Fixed

- pnpm-workspace.yaml overrides/patches, npm: alias overrides, cross-platform pnpm-lock ([#175](https://github.com/endevco/aube/pull/175))

### Other

- honor pnpm-workspace.yaml supportedArchitectures, ignoredOptionalDependencies, pnpmfilePath ([#181](https://github.com/endevco/aube/pull/181))
- render parse errors with miette source span ([#166](https://github.com/endevco/aube/pull/166))
- *(bun)* emit version, bin, optionalPeers on non-root workspaces ([#169](https://github.com/endevco/aube/pull/169))

## [1.0.0-beta.8](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.7...aube-lockfile-v1.0.0-beta.8) - 2026-04-20

### Other

- default to ~/.local/share/aube/store per XDG spec ([#129](https://github.com/endevco/aube/pull/129))
- *(npm)* tolerate legacy array engines field ([#132](https://github.com/endevco/aube/pull/132))
- *(npm)* accept string and array funding shapes ([#133](https://github.com/endevco/aube/pull/133))

## [1.0.0-beta.7](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.6...aube-lockfile-v1.0.0-beta.7) - 2026-04-19

### Other

- pnpm compat: multi-document lockfile + override over npm-alias ([#116](https://github.com/endevco/aube/pull/116))
- *(pnpm)* normalize empty-string root importer key ([#121](https://github.com/endevco/aube/pull/121))
- byte-identical pnpm-lock.yaml / bun.lock on re-emit ([#107](https://github.com/endevco/aube/pull/107))
- classify bare http(s) URLs as tarballs ([#114](https://github.com/endevco/aube/pull/114))
- *(bun)* emit and parse non-root workspaces ([#104](https://github.com/endevco/aube/pull/104))

## [1.0.0-beta.6](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.5...aube-lockfile-v1.0.0-beta.6) - 2026-04-19

### Other

- match pnpm ignored optionals order ([#90](https://github.com/endevco/aube/pull/90))

## [1.0.0-beta.5](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.4...aube-lockfile-v1.0.0-beta.5) - 2026-04-19

### Other

- normalize git selector fragments ([#62](https://github.com/endevco/aube/pull/62))

## [1.0.0-beta.3](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.2...aube-lockfile-v1.0.0-beta.3) - 2026-04-19

### Other

- *(bun)* handle github/git 3-tuple package entries ([#42](https://github.com/endevco/aube/pull/42))
- preserve npm-alias as folder name on fresh resolve ([#37](https://github.com/endevco/aube/pull/37))
- *(npm)* resolve peer deps when installing from package-lock.json ([#35](https://github.com/endevco/aube/pull/35))
- *(npm)* support npm:<real>@<ver> aliases + fix dep_path tail ([#30](https://github.com/endevco/aube/pull/30))
- Parse pnpm snapshot optional dependencies ([#18](https://github.com/endevco/aube/pull/18))

## [1.0.0-beta.2](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.1...aube-lockfile-v1.0.0-beta.2) - 2026-04-18

### Other

- aube-cli crate -> aube ([#7](https://github.com/endevco/aube/pull/7))
