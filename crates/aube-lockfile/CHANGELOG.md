# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.5.2](https://github.com/endevco/aube/compare/aube-lockfile-v1.5.1...aube-lockfile-v1.5.2) - 2026-04-30

### Fixed

- *(lockfile)* accept scalar os/cpu/libc in npm package-lock.json ([#405](https://github.com/endevco/aube/pull/405))
- *(lockfile)* synthesize npm-alias entries for transitive deps in pnpm lockfiles ([#403](https://github.com/endevco/aube/pull/403))
- *(install)* fetch hosted git deps over https, not ssh ([#394](https://github.com/endevco/aube/pull/394))

### Other

- thank Namespace for GitHub Actions runner support ([#412](https://github.com/endevco/aube/pull/412))
- refresh benchmarks for v1.5.1 ([#392](https://github.com/endevco/aube/pull/392))

## [1.5.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.4.0...aube-lockfile-v1.5.0) - 2026-04-29

### Fixed

- *(cli,linker,lockfile)* patch-commit destination, CRLF patches, npm-alias catalog ([#384](https://github.com/endevco/aube/pull/384))
- *(lockfile)* preserve pnpm registry tarball urls ([#378](https://github.com/endevco/aube/pull/378))
- *(lockfile)* hoist npm workspace links to root importer deps ([#374](https://github.com/endevco/aube/pull/374))

### Other

- *(lockfile)* add property roundtrip coverage ([#376](https://github.com/endevco/aube/pull/376))

## [1.4.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.3.0...aube-lockfile-v1.4.0) - 2026-04-28

### Fixed

- *(lockfile)* store bun dependency tails ([#355](https://github.com/endevco/aube/pull/355))
- *(lockfile)* apply overrides before frozen-lockfile spec comparison ([#354](https://github.com/endevco/aube/pull/354))
- *(packaging)* include README on published aube crate ([#349](https://github.com/endevco/aube/pull/349))

### Other

- warn about npm install caveats ([#368](https://github.com/endevco/aube/pull/368))

## [1.3.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.2.1...aube-lockfile-v1.3.0) - 2026-04-27

### Fixed

- *(lockfile)* preserve non-registry and bun platform entries ([#338](https://github.com/endevco/aube/pull/338))
- *(lockfile)* preserve package and bun lock compatibility ([#339](https://github.com/endevco/aube/pull/339))
- *(lockfile)* parse scalar pnpm platform fields ([#337](https://github.com/endevco/aube/pull/337))
- *(lockfile)* preserve npm platform optional metadata ([#329](https://github.com/endevco/aube/pull/329))
- bun.lock parity for workspaces, platforms, and locked versions ([#327](https://github.com/endevco/aube/pull/327))

### Other

- *(deps)* replace serde_yaml with yaml_serde ([#340](https://github.com/endevco/aube/pull/340))

## [1.2.1](https://github.com/endevco/aube/compare/aube-lockfile-v1.2.0...aube-lockfile-v1.2.1) - 2026-04-26

### Fixed

- pnpm snapshot round-trip + workspace negation patterns ([#312](https://github.com/endevco/aube/pull/312))

## [1.2.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.1.0...aube-lockfile-v1.2.0) - 2026-04-25

### Fixed

- support git url specs in dlx and parser ([#295](https://github.com/endevco/aube/pull/295))
- *(install)* link bins with mixed metadata ([#300](https://github.com/endevco/aube/pull/300))
- lockfile and resolver correctness pass ([#291](https://github.com/endevco/aube/pull/291))

## [1.1.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0...aube-lockfile-v1.1.0) - 2026-04-24

### Added

- *(resolver)* support pnpm `&path:/<sub>` git dep selector ([#273](https://github.com/endevco/aube/pull/273))

### Fixed

- *(resolver)* wire transitive url/git subdeps into parent snapshot ([#276](https://github.com/endevco/aube/pull/276))

### Other

- *(bun)* preserve top-level + per-entry metadata on roundtrip ([#250](https://github.com/endevco/aube/pull/250))
- *(pnpm)* preserve workspace importer specifiers ([#260](https://github.com/endevco/aube/pull/260))
- dedup pass + registry/store perf wave ([#254](https://github.com/endevco/aube/pull/254))
- resolve catalog: in overrides + honor override-rewritten importer specs ([#249](https://github.com/endevco/aube/pull/249))
- shared helpers + migrate hardcoded sites ([#245](https://github.com/endevco/aube/pull/245))

## [1.0.0](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.12...aube-lockfile-v1.0.0) - 2026-04-23

### Other

- *(yarn)* drop per-lookup String allocs in berry parser ([#234](https://github.com/endevco/aube/pull/234))
- extract read_lockfile helper to dedupe parser I/O ([#232](https://github.com/endevco/aube/pull/232))

## [1.0.0-beta.12](https://github.com/endevco/aube/compare/aube-lockfile-v1.0.0-beta.11...aube-lockfile-v1.0.0-beta.12) - 2026-04-22

### Other

- *(pnpm)* strip peer-context suffix from URL importer versions ([#214](https://github.com/endevco/aube/pull/214))
- cross-crate dedup pass ([#208](https://github.com/endevco/aube/pull/208))
- *(pnpm)* prefer pnpm version field for url-keyed transitives ([#204](https://github.com/endevco/aube/pull/204))
- cross-crate security hardening ([#202](https://github.com/endevco/aube/pull/202))
- *(npm)* parse workspace link entries ([#198](https://github.com/endevco/aube/pull/198))
- cross-crate correctness and security fixes ([#196](https://github.com/endevco/aube/pull/196))

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
