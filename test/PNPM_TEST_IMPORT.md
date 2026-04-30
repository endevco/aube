# pnpm test import — TODO

Tracking the import of pnpm's test suite into aube's bats suite for parity coverage. License is fine (pnpm is MIT, copy at [licenses/pnpm-LICENSE](../licenses/pnpm-LICENSE)).

Source: [pnpm/pnpm](https://github.com/pnpm/pnpm) checkout. Translation pattern: `prepare(manifest)` → write `package.json` + `cd`; `execPnpm([...])` → `aube ...`; `project.has(name)` → `assert_link_exists node_modules/$name`; `project.readLockfile()` → parse `aube-lock.yaml`.

## Phase 0 — infrastructure (done)

- [x] Mirror the ~25 `@pnpm.e2e/*` fixture packages used by Tier 1 tests into [test/registry/storage/@pnpm.e2e/](registry/storage/@pnpm.e2e/) ([#424](https://github.com/endevco/aube/pull/424)). Procedure documented at the top of [test/registry/config.yaml](registry/config.yaml). All 24 packages mirrored.
- [x] Add an `add_dist_tag` bash helper in [test/test_helper/common_setup.bash:84](test_helper/common_setup.bash) ([#422](https://github.com/endevco/aube/pull/422)).

## Phase 1 — Tier 1 translations (~88 tests, highest signal density)

Goal: highest install-path parity coverage for lowest cost. Each row is a pnpm source file → aube target file, counts are pnpm's actual `test()` cases (not all will translate cleanly — expect 60-80% yield).

- [ ] `pnpm/test/install/misc.ts` (37 tests, 645 LOC) → [test/pnpm_install_misc.bats](pnpm_install_misc.bats) (12/37 ported)
  - Done: `--save-exact + --save-dev` (124), `--use-stderr` (73), `lockfile=false` in pnpm-workspace.yaml (83), `--prefix` (97), spec-preserved-verbatim (150), bin-on-PATH-in-root-postinstall (36), run-script-invokes-dep-bin (219), case-only-filename-collision-installs-cleanly (163), create-package.json-if-missing (233 — required a small `add.rs` change to write `{}` when no project root exists), bare-add-fails (245), top-level-plugins (190 — top-level bin resolves a sibling top-level package via Node's parent-`node_modules` walk), not-top-level-plugins (204 — top-level dep's bin resolves its own non-top-level dep; minimal `aube-test-bin-uses-dep` fixture in lieu of mirroring `standard@8.6.0`'s 170-package tree).
  - Remaining high-value: install timeout.
  - Equivalent coverage already exists in aube: strict-store-pkg-content-check (516) — aube's `strictStorePkgContentCheck` setting is fully implemented in `aube-store` and tested in [test/store_settings.bats](store_settings.bats) against the `aube-test-content-liar` fixture (a registry-substitution attack simulation). pnpm's misc.ts:516 mutates pnpm's `StoreIndex` Node API directly, which is pnpm-internal and doesn't translate to aube's CAS architecture.
  - Skipped (still need fixtures): peer-deps-warning (541 — needs `@udecode/plate-*`), circular-peer-deps (556), trust-policy (578-643 — pnpm-specific feature).
  - Documented divergences (don't port without aube-side fix): `--lockfile-dir` (112 — aube has no flag for placing the lockfile outside the project root).
- [ ] `pnpm/test/install/hooks.ts` (22 tests, 698 LOC) → [test/pnpm_install_hooks.bats](pnpm_install_hooks.bats) (8/22 ported, 2 skipped divergences)
  - Done: async readPackage on transitive (43), async afterAllResolved (498), syntax error in pnpmfile (292), require() of missing module (303), readPackage normalizes optional/peer/dev fields on transitive (528), readPackage during `aube update` (263), `--ignore-pnpmfile` on `aube update` (338), `preResolution` hook fires before resolve (624).
  - Not yet ported (Phase 0 unblocked): sync readPackage (18), custom pnpmfile location (85 — needs `--pnpmfile` CLI flag), global pnpmfile (110, 135, 176 — needs `--global-pnpmfile`), workspace pnpmfile (217), context.log via ndjson reporter (366, 404 — needs ndjson `pnpm:hook` log surface), shared workspace lockfile (661).
  - Documented divergences (don't port without aube-side fix): readPackage returning undefined fails install (68), readPackage on root project's manifest applies (551). The 314 install-side --ignore-pnpmfile case is already covered by [test/pnpmfile.bats](pnpmfile.bats:215).
- [ ] `pnpm/test/install/lifecycleScripts.ts` (21 tests, 356 LOC) → folded into [test/lifecycle_scripts.bats](lifecycle_scripts.bats) (8/21 ported, [#421](https://github.com/endevco/aube/pull/421))
  - Done: preinstall/postinstall/prepare stdout reaches the user (43, 56, 95), `npm_config_user_agent` set on lifecycle scripts (29), root postinstall NOT triggered by `aube add` / root prepare NOT triggered by `aube add` (69, 82), root postinstall NOT triggered by `aube remove` / `aube update`.
  - Remaining: exit-code propagation, env-var inheritance specifics, script-not-found handling, ordering edge cases.
- [x] `pnpm/test/saveCatalog.ts` (8 tests, 224 LOC) → [test/pnpm_savecatalog.bats](pnpm_savecatalog.bats) (8/8 ported)
  - Implements `aube add --save-catalog` and `--save-catalog-name=<name>`, `<pkg>@workspace:*` CLI parsing for `aube add`, and `sharedWorkspaceLockfile=false` per-project lockfile writes.

## Phase 2 — unblocked (`add_dist_tag` helper landed in [#422](https://github.com/endevco/aube/pull/422))

- [ ] `pnpm/test/update.ts` (22 tests, 50 dist-tag uses) → [test/pnpm_update.bats](pnpm_update.bats) (8/22 ported)
  - Done: update <dep> (14), update --no-save (34), update --latest --prod (225), recursive update --no-save (72), recursive update --no-shared-workspace-lockfile (118), recursive update --latest on shared lockfile (426 — asserted per-project, see divergence below), recursive update --latest --prod on shared lockfile (478 — same), update with tag @latest downgrades prerelease (659 — translated from `update <pkg>@latest` to `update --latest <pkg>`).
  - Phase 0 fixture: mirrored `@pnpm.e2e/has-prerelease` (versions 1.0.0, 2.0.0, 3.0.0-rc.0) at [test/registry/storage/@pnpm.e2e/has-prerelease/](registry/storage/@pnpm.e2e/has-prerelease/). Tarballs synthesized via `npm pack` on a minimal `package.json`; packument hand-built (no upstream — `@pnpm.e2e/has-prerelease` doesn't exist on registry.npmjs.org).
  - Skipped (still need fixtures): update --latest (143), update --latest --save-exact (170), update --latest specific dep (197), recursive update --latest specific no-shared (369), recursive update --latest specific shared (543) — all need `@pnpm.e2e/qar` (alias dep) + GitHub spec support (`kevva/is-negative`). recursive update --latest no-shared (249) and recursive update --latest --prod no-shared (302) are subsumed by the shared-lockfile ports above (aube's `update -r` always writes per-project lockfiles regardless of `sharedWorkspaceLockfile`).
  - Documented divergences (don't port without aube-side fix): update without --latest (51) and recursive update without --latest (95) — pnpm rewrites the manifest specifier when the existing range allows the new version; aube only rewrites under `--latest`. deep update (599) — `aube update --depth N` is parsed-but-no-op and `aube update` (no flags) preserves indirect-dep versions from the lockfile (only direct deps are excluded from the filtered-existing snapshot in `update.rs:212-218`), so the `--depth 1` semantics don't translate. update indirect dep (690) — `aube update <pkg>` errors when `<pkg>` is not in package.json's deps map (`update.rs:139-146`). `aube update -r` always writes per-project lockfiles even when `sharedWorkspaceLockfile=true` — there's no shared-lockfile aggregation on the update path; use `aube install` from the workspace root for that. Prerelease preservation across `--latest`: pnpm's `update --latest` keeps a manifest pin like `3.0.0-rc.0` when the registry's `latest` dist-tag is older (2.0.0); aube downgrades to the dist-tag. Affects 615, 728, 807 (the has-prerelease tests not ported above).
- [ ] `pnpm/test/recursive/update.ts` (5 tests, 2 dist-tag uses)
- [ ] `pnpm/test/install/preferOffline.ts` (3 dist-tag uses)

## Phase 3 — Tier 2 (workspace + extras, batched)

- [ ] `pnpm/test/monorepo/index.ts` (41 tests, 2026 LOC) — workspace-wide install behavior. Bite off in batches of 10-15:
  - [ ] batch 1: filter + `--filter` semantics
  - [ ] batch 2: workspace: protocol edge cases
  - [ ] batch 3: shared-workspace-lockfile behavior
  - [ ] batch 4: dedupePeers across workspace
- [ ] `pnpm/test/monorepo/dedupePeers.test.ts` (4 tests)
- [ ] `pnpm/test/monorepo/peerDependencies.ts` (~4 tests)
- [ ] `pnpm/test/configurationalDependencies.test.ts` (7 tests) — only if aube targets parity
- [ ] `installing/deps-installer/test/catalogs.ts` — resolver-side catalog coverage

## Explicitly skipped (Tier 3)

These test pnpm-internal library APIs (`@pnpm/...`) and don't translate without a Rust port of the same library:
- All `installing/commands/test/*.ts` (~25 files)
- All `lockfile/*/test/*.ts`
- All `resolving/*/test/*.ts`
- All `pkg-manager/*/test/*.ts`

These test pnpm-specific behavior aube doesn't replicate:
- `pnpm/test/install/global.ts` — global install
- `pnpm/test/install/selfUpdate.ts` — pnpm self-update
- `pnpm/test/install/pnpmRegistry.ts` — pnpm-specific registry
- `pnpm/test/install/nodeRuntime.ts` — pnpm `node` runtime feature
- `pnpm/test/install/runtimeOnFail.ts` — pnpm `node` runtime feature
- `pnpm/test/syncInjectedDepsAfterScripts*.ts` — `injected: true` (aube doesn't ship this)

## Conventions for translations

See [test/pnpm_install_misc.bats](pnpm_install_misc.bats) for a worked example covering all the conventions below.

- **File naming**: ported tests live in `test/pnpm_<source_file>.bats` (e.g. `pnpm/test/install/misc.ts` → `test/pnpm_install_misc.bats`). One bats file per pnpm source file. The file header comments cite the pnpm source path.
- **Per-test citation**: each `@test` block opens with `# Ported from pnpm/test/<path>:<line>` so the audit trail is intact. If you adapt the test (e.g. substitute a package), note the substitution on the next line.
- **`pnpm install <pkg>` ≈ `aube add <pkg>`**: pnpm overloads `install` to also add new deps. aube splits them. When porting, switch to `aube add` and call out the swap in the comment.
- **Package substitutions**: pnpm tests lean on `is-positive`, `rimraf`, `@pnpm.e2e/*`. The Tier 1 `@pnpm.e2e/*` fixtures are mirrored in [test/registry/storage/@pnpm.e2e/](registry/storage/@pnpm.e2e/) — use them when the test needs the specific shape (peer chains, lifecycle hooks, plugin-host trees). For generic deps where any leaf will do, prefer in-tree fixtures (`is-odd`, `is-even`, `is-number`, `semver`) and note the substitution in the test comment.
- **Don't assert on pnpm-internal paths**: when a pnpm test asserts on `.pnpm/`, `STORE_VERSION`, `node_modules/.modules.yaml` etc., translate the *behavior* and assert on the aube equivalent (`.aube/`, store v1, `node_modules/.aube-state`).
- **Surfaced bugs**: if a port exposes a real aube divergence, file it in [Discussions](https://github.com/endevco/aube/discussions) and mark the test with `skip "aube divergence: <link>"` rather than blocking the import.
