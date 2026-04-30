# pnpm test import — TODO

Tracking the import of pnpm's test suite into aube's bats suite for parity coverage. License is fine (pnpm is MIT, copy at [licenses/pnpm-LICENSE](../licenses/pnpm-LICENSE)).

Source: [pnpm/pnpm](https://github.com/pnpm/pnpm) checkout. Translation pattern: `prepare(manifest)` → write `package.json` + `cd`; `execPnpm([...])` → `aube ...`; `project.has(name)` → `assert_link_exists node_modules/$name`; `project.readLockfile()` → parse `aube-lock.yaml`.

## Phase 0 — infrastructure

- [ ] Mirror the ~25 `@pnpm.e2e/*` fixture packages used by Tier 1 tests into [test/registry/storage/](registry/storage/). Procedure already documented at the top of [test/registry/config.yaml](registry/config.yaml). Packages needed:
  - [ ] `@pnpm.e2e/abc`
  - [ ] `@pnpm.e2e/abc-grand-parent-with-c`
  - [ ] `@pnpm.e2e/abc-parent-with-ab`
  - [ ] `@pnpm.e2e/abc-parent-with-missing-peers`
  - [ ] `@pnpm.e2e/bar`
  - [ ] `@pnpm.e2e/cli-with-node-engine`
  - [ ] `@pnpm.e2e/dep-of-pkg-with-1-dep`
  - [ ] `@pnpm.e2e/foo`
  - [ ] `@pnpm.e2e/foobar`
  - [ ] `@pnpm.e2e/has-untrusted-optional-dep`
  - [ ] `@pnpm.e2e/hello-world-js-bin`
  - [ ] `@pnpm.e2e/install-script-example`
  - [ ] `@pnpm.e2e/peer-a`
  - [ ] `@pnpm.e2e/peer-b`
  - [ ] `@pnpm.e2e/peer-c`
  - [ ] `@pnpm.e2e/pkg-that-uses-plugins`
  - [ ] `@pnpm.e2e/pkg-with-1-dep`
  - [ ] `@pnpm.e2e/pkg-with-good-optional`
  - [ ] `@pnpm.e2e/plugin-example`
  - [ ] `@pnpm.e2e/postinstall-calls-pnpm`
  - [ ] `@pnpm.e2e/pre-and-postinstall-scripts-example`
  - [ ] `@pnpm.e2e/print-version`
  - [ ] `@pnpm.e2e/support-different-architectures`
  - [ ] `@pnpm.e2e/with-same-file-in-different-cases`
- [ ] Add an `add_dist_tag` bash helper in [test/test_helper/](test_helper/) that mutates `test/registry/storage/<pkg>/package.json` to set a dist-tag (Verdaccio re-reads on next request). Needed by ~10 files; heaviest in update.ts (50 uses).

## Phase 1 — Tier 1 translations (~88 tests, highest signal density)

Goal: highest install-path parity coverage for lowest cost. Each row is a pnpm source file → aube target file, counts are pnpm's actual `test()` cases (not all will translate cleanly — expect 60-80% yield).

- [ ] `pnpm/test/install/misc.ts` (37 tests, 645 LOC) → [test/pnpm_install_misc.bats](pnpm_install_misc.bats) (1/37 ported as worked example)
  - Highest-value targets: `--lockfile-only`, `--no-lockfile`, `--prefix`, case-sensitive FS, `STORE_VERSION` migrations
- [ ] `pnpm/test/install/hooks.ts` (22 tests, 698 LOC) → fold into [test/pnpmfile.bats](pnpmfile.bats)
  - `readPackage` sync/async, hook removes a dep, hook overrides version, hook fails install, hook on workspace packages
- [ ] `pnpm/test/install/lifecycleScripts.ts` (21 tests, 356 LOC) → fold into [test/lifecycle_scripts.bats](lifecycle_scripts.bats)
  - pre/postinstall ordering, exit-code propagation, env-var inheritance, script-not-found handling
- [ ] `pnpm/test/saveCatalog.ts` (8 tests, 224 LOC) → [test/pnpm_savecatalog.bats](pnpm_savecatalog.bats) (7/8 ported, 1 skipped on `sharedWorkspaceLockfile=false`)
  - Implements `aube add --save-catalog` and `--save-catalog-name=<name>`, plus `<pkg>@workspace:*` CLI parsing for `aube add`. Skipped: multi-lockfile workspaces (`sharedWorkspaceLockfile=false` not implemented in aube).

## Phase 2 — depends on add_dist_tag helper

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
- **Package substitutions**: pnpm tests lean on `is-positive`, `rimraf`, `@pnpm.e2e/*`. Until the e2e fixtures are mirrored (Phase 0), substitute equivalents already in [test/registry/storage/](registry/storage/) — `is-odd`, `is-even`, `is-number`, `semver`. Note the substitution in the test comment.
- **Don't assert on pnpm-internal paths**: when a pnpm test asserts on `.pnpm/`, `STORE_VERSION`, `node_modules/.modules.yaml` etc., translate the *behavior* and assert on the aube equivalent (`.aube/`, store v1, `node_modules/.aube-state`).
- **Surfaced bugs**: if a port exposes a real aube divergence, file it in [Discussions](https://github.com/endevco/aube/discussions) and mark the test with `skip "aube divergence: <link>"` rather than blocking the import.
