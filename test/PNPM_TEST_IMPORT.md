# pnpm test import — TODO

Tracking the import of pnpm's test suite into aube's bats suite for parity coverage. License is fine (pnpm is MIT, copy at [licenses/pnpm-LICENSE](../licenses/pnpm-LICENSE)).

Source: [pnpm/pnpm](https://github.com/pnpm/pnpm) checkout. Translation pattern: `prepare(manifest)` → write `package.json` + `cd`; `execPnpm([...])` → `aube ...`; `project.has(name)` → `assert_link_exists node_modules/$name`; `project.readLockfile()` → parse `aube-lock.yaml`.

## Phase 0 — infrastructure (done)

- [x] Mirror the ~25 `@pnpm.e2e/*` fixture packages used by Tier 1 tests into [test/registry/storage/@pnpm.e2e/](registry/storage/@pnpm.e2e/) ([#424](https://github.com/endevco/aube/pull/424)). Procedure documented at the top of [test/registry/config.yaml](registry/config.yaml). All 24 packages mirrored.
- [x] Add an `add_dist_tag` bash helper in [test/test_helper/common_setup.bash:84](test_helper/common_setup.bash) ([#422](https://github.com/endevco/aube/pull/422)).

## Phase 1 — Tier 1 translations (~88 tests, highest signal density)

Goal: highest install-path parity coverage for lowest cost. Each row is a pnpm source file → aube target file, counts are pnpm's actual `test()` cases (not all will translate cleanly — expect 60-80% yield).

- [ ] `pnpm/test/install/misc.ts` (37 tests, 645 LOC) → [test/pnpm_install_misc.bats](pnpm_install_misc.bats) (13/37 ported)
  - Done: `--save-exact + --save-dev` (124), `--use-stderr` (73), `lockfile=false` in pnpm-workspace.yaml (83), `--prefix` (97), spec-preserved-verbatim (150), bin-on-PATH-in-root-postinstall (36), run-script-invokes-dep-bin (219), case-only-filename-collision-installs-cleanly (163), create-package.json-if-missing (233 — required a small `add.rs` change to write `{}` when no project root exists), bare-add-fails (245), top-level-plugins (190 — top-level bin resolves a sibling top-level package via Node's parent-`node_modules` walk), not-top-level-plugins (204 — top-level dep's bin resolves its own non-top-level dep; minimal `aube-test-bin-uses-dep` fixture in lieu of mirroring `standard@8.6.0`'s 170-package tree), circular-peer-deps-don't-hang (556 — synthesized minimal workspace fixture inline; pnpm's 100-package real-world fixture is impractical to mirror against the offline registry, the regression is the resolver terminating).
  - Remaining high-value: install timeout.
  - Equivalent coverage already exists in aube: strict-store-pkg-content-check (516) — aube's `strictStorePkgContentCheck` setting is fully implemented in `aube-store` and tested in [test/store_settings.bats](store_settings.bats) against the `aube-test-content-liar` fixture (a registry-substitution attack simulation). pnpm's misc.ts:516 mutates pnpm's `StoreIndex` Node API directly, which is pnpm-internal and doesn't translate to aube's CAS architecture.
  - Trust-policy block (578-643, 6 tests): aube has the feature, but the tests need fixtures we don't mirror yet — `@pnpm/e2e.test-provenance` (different namespace from the mirrored `@pnpm.e2e/`) with two versions whose `dist.attestations.provenance` ranks differ, and `@pnpm.e2e/has-untrusted-optional-dep` is already mirrored at `1.0.0`. pnpm exposes `--trust-policy=…` as a CLI flag; aube reads `trustPolicy` only from `.npmrc`/workspace yaml/`AUBE_TRUST_POLICY`, so port via `.npmrc` writes, not flags. Track as a separate fixture-import task.
  - Documented divergences (don't port without aube-side fix): `--lockfile-dir` (112 — aube has no flag for placing the lockfile outside the project root), peer-deps-warning (541 — pnpm asserts a stdout warning + status 0 when peers are unmet; aube's default is silent matching bun/npm/yarn, and `strict-peer-dependencies=true` flips to a hard failure with the same `"Issues with peer dependencies found"` line. The string-level assertion is already covered in [test/peer_deps.bats:297](peer_deps.bats); aube has no warn-and-succeed mode to port the exact pnpm behavior into).
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

- [ ] `pnpm/test/update.ts` (22 tests, 50 dist-tag uses) → fold into [test/update.bats](update.bats)
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
