# pnpm test import — TODO

Tracking the import of pnpm's test suite into aube's bats suite for parity coverage. License is fine (pnpm is MIT, copy at [licenses/pnpm-LICENSE](../licenses/pnpm-LICENSE)).

Source: [pnpm/pnpm](https://github.com/pnpm/pnpm) checkout. Translation pattern: `prepare(manifest)` → write `package.json` + `cd`; `execPnpm([...])` → `aube ...`; `project.has(name)` → `assert_link_exists node_modules/$name`; `project.readLockfile()` → parse `aube-lock.yaml`.

## Phase 0 — infrastructure (done)

- [x] Mirror the ~25 `@pnpm.e2e/*` fixture packages used by Tier 1 tests into [test/registry/storage/@pnpm.e2e/](registry/storage/@pnpm.e2e/) ([#424](https://github.com/endevco/aube/pull/424)). Procedure documented at the top of [test/registry/config.yaml](registry/config.yaml). All 24 packages mirrored.
- [x] Add an `add_dist_tag` bash helper in [test/test_helper/common_setup.bash:84](test_helper/common_setup.bash) ([#422](https://github.com/endevco/aube/pull/422)).

## Phase 1 — Tier 1 translations (~88 tests, highest signal density)

Goal: highest install-path parity coverage for lowest cost. Each row is a pnpm source file → aube target file, counts are pnpm's actual `test()` cases (not all will translate cleanly — expect 60-80% yield).

- [ ] `pnpm/test/install/misc.ts` (37 tests, 645 LOC) → [test/pnpm_install_misc.bats](pnpm_install_misc.bats) (22/37 ported)
  - Done: `--save-exact + --save-dev` (124), `--use-stderr` (73), `lockfile=false` in pnpm-workspace.yaml (83), `--prefix` (97), spec-preserved-verbatim (150), bin-on-PATH-in-root-postinstall (36), run-script-invokes-dep-bin (219), case-only-filename-collision-installs-cleanly (163), create-package.json-if-missing (233 — required a small `add.rs` change to write `{}` when no project root exists), bare-add-fails (245), `--lockfile-dir` (112 — flag implemented in [#431](https://github.com/endevco/aube/pull/431)), top-level-plugins (190 — top-level bin resolves a sibling top-level package via Node's parent-`node_modules` walk), not-top-level-plugins (204 — top-level dep's bin resolves its own non-top-level dep; minimal `aube-test-bin-uses-dep` fixture in lieu of mirroring `standard@8.6.0`'s 170-package tree), circular-peer-deps-don't-hang (556 — synthesized minimal workspace fixture inline; pnpm's 100-package real-world fixture is impractical to mirror against the offline registry, the regression is the resolver terminating), trust-policy-block (578, 589, 600, 612, 624, 635 — six trust-policy tests; mirrored `@pnpm/e2e.test-provenance` at 0.0.0/0.0.4/0.0.5 with the SLSA provenance + GitHub trustedPublisher metadata pnpm's check looks at, and translated `--trust-policy=…` flags to `.npmrc` writes since aube reads `trustPolicy`/`trustPolicyExclude`/`trustPolicyIgnoreAfter` from `.npmrc`/workspace yaml/`AUBE_TRUST_POLICY` only — there's no CLI flag), peer-deps-warning-renders (541 — strict-peer-dependencies-mode variant; pnpm warns + status 0, aube's `strict-peer-dependencies=true` is the only mode that surfaces the same `"Issues with peer dependencies found"` line, and aube routes it through a hard-fail rather than warn-and-succeed. `@udecode/plate-*` substituted with the mirrored `@pnpm.e2e/abc-parent-with-missing-peers`. The regression guard — peer-deps diagnostic renderer not crashing — is preserved either way), fetch-timeout-fails (508 — added the global `--fetch-timeout` / `--fetch-retries` / `--fetch-retry-{factor,mintimeout,maxtimeout}` CLI surface alongside the port).
  - Equivalent coverage already exists in aube: strict-store-pkg-content-check (516) — aube's `strictStorePkgContentCheck` setting is fully implemented in `aube-store` and tested in [test/store_settings.bats](store_settings.bats) against the `aube-test-content-liar` fixture (a registry-substitution attack simulation). pnpm's misc.ts:516 mutates pnpm's `StoreIndex` Node API directly, which is pnpm-internal and doesn't translate to aube's CAS architecture.
- [ ] `pnpm/test/install/hooks.ts` (22 tests, 698 LOC) → [test/pnpm_install_hooks.bats](pnpm_install_hooks.bats) (14/22 ported, 2 skipped divergences)
  - Done: async readPackage on transitive (43), async afterAllResolved (498), syntax error in pnpmfile (292), require() of missing module (303), readPackage normalizes optional/peer/dev fields on transitive (528), readPackage during `aube update` (263), `--ignore-pnpmfile` on `aube update` (338), `preResolution` hook fires before resolve (624), `--pnpmfile` custom location (85), `--global-pnpmfile` (110), global+local readPackage composition (135), async global+local readPackage composition (176), readPackage `ctx.log` surfaces as `pnpm:hook` ndjson on stdout (366), afterAllResolved `ctx.log` surfaces as `pnpm:hook` ndjson on stdout (468).
  - Not yet ported (Phase 0 unblocked): sync readPackage (18), workspace pnpmfile (217), `ctx.log` via global+local pnpmfile composition (404 — both blockers landed, just needs the port), shared workspace lockfile (661).
  - Documented divergences (don't port without aube-side fix): readPackage returning undefined fails install (68), readPackage on root project's manifest applies (551). The 314 install-side --ignore-pnpmfile case is already covered by [test/pnpmfile.bats](pnpmfile.bats:215).
- [ ] `pnpm/test/install/lifecycleScripts.ts` (21 tests, 356 LOC) → folded into [test/lifecycle_scripts.bats](lifecycle_scripts.bats) (15/21 ported, [#421](https://github.com/endevco/aube/pull/421))
  - Done: preinstall/postinstall/prepare stdout reaches the user (43, 56, 95), `npm_config_user_agent` set on lifecycle scripts (29), root postinstall NOT triggered by `aube add` / root prepare NOT triggered by `aube add` (69, 82), root postinstall NOT triggered by `aube remove` / `aube update`, strictDepBuilds throws on unreviewed registry build scripts (226), allowBuilds review placeholder auto-populated (260) and merged with existing approvals (268), strictDepBuilds still fails for packages with cached side-effects (303 — covers [pnpm/pnpm#11035](https://github.com/pnpm/pnpm/issues/11035)), `--allow-build=<pkg>` selectively pre-approves a dep's build scripts (149), bare `--allow-build` errors out (164), `--allow-build=<pkg>` errors when conflicting with `allowBuilds: false` (347).
  - Aube divergence worth noting in ports: aube's strict-dep-builds error reads "dependencies with build scripts must be reviewed before install" where pnpm's reads "Ignored build scripts:". Aube now writes the same `"set this to true or false"` placeholder string as pnpm, so review-placeholder ports can assert verbatim.
  - Equivalent coverage already exists in aube: installation-fails-on-lifecycle-script-error (17 — aube's "fails fast if a root lifecycle script exits non-zero" at lifecycle_scripts.bats:187 covers the same contract).
  - Skipped (need new fixtures or non-trivial setup): rollback-dep-on-build-failure (108 — needs a `package-that-cannot-be-installed`-style failing-install fixture), node-gyp on PATH (128 — node-gyp is bootstrapped lazily in aube, testing without bootstrap is slow), repeat-install warning preservation (245 — aube's freshness check short-circuits before re-emitting the ignored-builds warning), git-dep prepare under dangerouslyAllowAllBuilds (336 — needs a hosted git fixture, network).
  - Documented divergences (don't port without aube-side fix): `verify-deps-before-run` interaction with preinstall sub-aube calls (179, 200 — aube's auto-install gate has different recursion semantics), selective `aube rebuild <pkg>` (282 — aube's `aube rebuild` rebuilds the whole policy, no per-package selector).
- [x] `pnpm/test/saveCatalog.ts` (8 tests, 224 LOC) → [test/pnpm_savecatalog.bats](pnpm_savecatalog.bats) (8/8 ported)
  - Implements `aube add --save-catalog` and `--save-catalog-name=<name>`, `<pkg>@workspace:*` CLI parsing for `aube add`, and `sharedWorkspaceLockfile=false` per-project lockfile writes.

## Phase 2 — unblocked (`add_dist_tag` helper landed in [#422](https://github.com/endevco/aube/pull/422))

- [ ] `pnpm/test/update.ts` (22 tests, 50 dist-tag uses) → [test/pnpm_update.bats](pnpm_update.bats) (13/22 ported)
  - Done: update <dep> (14), update --no-save (34), update --latest --prod (225), recursive update --no-save (72), recursive update --no-shared-workspace-lockfile (118), recursive update --latest on shared lockfile (426 — asserted per-project, see divergence below), recursive update --latest --prod on shared lockfile (478 — same), update with tag @latest downgrades prerelease (659 — translated from `update <pkg>@latest` to `update --latest <pkg>`), update --latest (143), update --latest --save-exact (170), update --latest specific dep (197), recursive update --latest specific no-shared (369 — same as 543 in aube's per-project model), recursive update --latest specific shared (543 — asserted per-project).
  - Phase 0 fixtures mirrored: `@pnpm.e2e/has-prerelease` (1.0.0, 2.0.0, 3.0.0-rc.0) and `@pnpm.e2e/qar` (100.0.0, 100.1.0). Synthesized via `npm pack` on a minimal `package.json`; packument hand-built (no upstream — these packages don't exist on registry.npmjs.org).
  - Aube-side fixes that landed alongside the ports: (1) `update.rs` look-up by manifest key for aliased direct deps so `--latest` rewrites the `npm:` spec correctly, (2) `--save-exact` / `-E` flag on `update`, (3) recursive update silently skips projects that don't declare any of the named args (matching pnpm).
  - Skipped (still need GitHub spec support): the `kevva/is-negative` GitHub-shorthand assertion was dropped from the 143/170/197 ports (the only `is-negative`-related assertion in those tests). Adding `user/repo` GitHub-shorthand resolution is its own feature (parser + git tarball fetcher + storage) — tracked separately. recursive update --latest no-shared (249) and recursive update --latest --prod no-shared (302) are subsumed by the shared-lockfile ports above (aube's `update -r` always writes per-project lockfiles regardless of `sharedWorkspaceLockfile`).
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
- **Package substitutions**: pnpm tests lean on `is-positive`, `is-negative`, `rimraf`, `@pnpm.e2e/*`. The Tier 1 `@pnpm.e2e/*` fixtures are mirrored in [test/registry/storage/@pnpm.e2e/](registry/storage/@pnpm.e2e/), and `is-positive` (1.0.0–3.1.0) + `is-negative` (1.0.0–2.1.0) are mirrored alongside the other in-tree fixtures — use them when the test needs the specific shape (peer chains, lifecycle hooks, plugin-host trees, or two distinguishable leaves like `is-positive`/`is-negative` together). For generic deps where any leaf will do, prefer in-tree fixtures (`is-odd`, `is-even`, `is-number`, `semver`) and note the substitution in the test comment.
- **Don't assert on pnpm-internal paths**: when a pnpm test asserts on `.pnpm/`, `STORE_VERSION`, `node_modules/.modules.yaml` etc., translate the *behavior* and assert on the aube equivalent (`.aube/`, store v1, `node_modules/.aube-state`).
- **Surfaced bugs**: if a port exposes a real aube divergence, file it in [Discussions](https://github.com/endevco/aube/discussions) and mark the test with `skip "aube divergence: <link>"` rather than blocking the import.
