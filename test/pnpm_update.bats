#!/usr/bin/env bats
#
# Ported from pnpm/test/update.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# These tests mutate `dist-tags` on the committed Verdaccio storage via
# `add_dist_tag` and restore them via `git checkout` in teardown — same
# pattern as test/deprecate.bats. Tag the file as serial and disable
# within-file parallelism.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	# Restore any mutated dist-tags so the fixture stays clean across runs.
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/@pnpm.e2e/foo/package.json \
			test/registry/storage/@pnpm.e2e/bar/package.json \
			test/registry/storage/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json \
			test/registry/storage/@pnpm.e2e/has-prerelease/package.json \
			test/registry/storage/@pnpm.e2e/pkg-with-1-dep/package.json 2>/dev/null || true
	fi
	_common_teardown
}

# Skip if the local Verdaccio fixture isn't running. add_dist_tag mutates
# its on-disk storage, so without it these tests have nothing to PUT.
_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

@test "aube update --latest <pkg>: bumps a single dep past its declared range" {
	# Ported from pnpm/test/update.ts:14 ('update <dep>').
	# pnpm: `pnpm update <pkg>@latest`. aube does not parse `<pkg>@<spec>`
	# in update args, so translate to `aube update --latest <pkg>` — same
	# behavior: rewrite the manifest range to track the resolved version
	# rather than the existing range.
	_require_registry

	# Pin 100.0.0 as latest while the user installs at the lower range,
	# then publish 101.0.0 as the new latest before running update.
	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-dep",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0'
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0

	run aube update --latest '@pnpm.e2e/dep-of-pkg-with-1-dep'
	assert_success

	# Lockfile resolves to the new latest.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success

	# package.json range bumped to track the new version.
	run grep '"\^101.0.0"' package.json
	assert_success
}

@test "aube update --no-save: refreshes the lockfile, leaves package.json range alone" {
	# Ported from pnpm/test/update.ts:34 ('update --no-save').
	# `--no-save` without `--latest` is a no-op for the manifest in aube
	# (plain `update` already doesn't rewrite specifiers), so the assertion
	# shape matches pnpm: lockfile resolves to the new latest in-range,
	# package.json keeps the original `^100.0.0`.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-no-save",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON

	run aube update --no-save
	assert_success

	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success

	# package.json range untouched.
	run grep '"\^100.0.0"' package.json
	assert_success
}

@test "aube update --latest --prod: bumps prod deps, leaves devDeps pinned" {
	# Ported from pnpm/test/update.ts:225 ('update --latest --prod').
	# aube's `add` defaults to prod (no `-P` flag — pnpm requires it for
	# parity with npm), so the second `add` here drops `-P`.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-prod",
  "version": "0.0.0"
}
JSON

	run aube add -D '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0'
	assert_success
	run aube add '@pnpm.e2e/bar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0

	run aube update --latest --prod
	assert_success

	# Prod dep bumped past its range.
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success

	# Dev dep stays pinned at 100.0.0 — --prod skipped it.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_failure
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' package.json
	assert_success
}

@test "aube update -r --no-save: refreshes a workspace lockfile, leaves manifests alone" {
	# Ported from pnpm/test/update.ts:72 ('recursive update --no-save').
	# pnpm writes the lockfile at the workspace root via
	# shared-workspace-lockfile=true; aube's `update -r` fans out per
	# project and writes per-project lockfiles regardless of the
	# `sharedWorkspaceLockfile` setting (a divergence — see
	# PNPM_TEST_IMPORT.md). Assert the per-project lockfile shape.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
YAML

	run aube update -r --no-save
	assert_success

	# Per-project lockfile carries the bumped version.
	run grep '@pnpm.e2e/foo@100.1.0' project/aube-lock.yaml
	assert_success

	# Project manifest range unchanged.
	run grep '"\^100.0.0"' project/package.json
	assert_success
}

@test "aube update -r --no-shared-workspace-lockfile: writes a per-project lockfile" {
	# Ported from pnpm/test/update.ts:118 ('recursive update
	# --no-shared-workspace-lockfile').
	# pnpm exposes this as a CLI flag; aube reads
	# `sharedWorkspaceLockfile` from `.npmrc` / pnpm-workspace.yaml /
	# env vars (no CLI flag yet). Set it via `.npmrc` to opt out, then
	# verify the per-project lockfile lands in `project/aube-lock.yaml`
	# instead of the workspace root.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
YAML
	cat >>.npmrc <<'EOF'
shared-workspace-lockfile=false
EOF

	run aube update -r --latest
	assert_success

	# Per-project lockfile carries the bumped version.
	assert_file_exists project/aube-lock.yaml
	run grep '@pnpm.e2e/foo@100.1.0' project/aube-lock.yaml
	assert_success

	# Manifest rewritten to track the new latest.
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project/package.json
	assert_success

	# No shared root lockfile.
	assert_file_not_exists aube-lock.yaml
}

@test "aube update -r --latest: bumps every workspace project's manifest" {
	# Ported from pnpm/test/update.ts:426 ('recursive update --latest on
	# projects with a shared a lockfile'). aube fans out per project
	# (per-project lockfiles); the shared-lockfile assertion at
	# pnpm/test/update.ts:471-475 is dropped — aube divergence noted in
	# PNPM_TEST_IMPORT.md. The `@pnpm.e2e/qar` alias dep is omitted (no
	# fixture mirrored yet).
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube update -r --latest
	assert_success

	# Both manifests rewritten to the new latest.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.1.0"' project-2/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-2/package.json
	assert_success

	# Per-project lockfiles each carry the bumped versions.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' project-1/aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/foo@100.1.0' project-1/aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' project-2/aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/foo@100.1.0' project-2/aube-lock.yaml
	assert_success
}

@test "aube update -r --latest --prod: skips devDeps in workspace fanout" {
	# Ported from pnpm/test/update.ts:478 ('recursive update --latest
	# --prod on projects with a shared a lockfile'). Verifies the
	# prod/dev split survives the recursive fanout. Same shared-lockfile
	# divergence as the previous test — assertions are per-project.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"
  },
  "devDependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  },
  "devDependencies": {
    "@pnpm.e2e/bar": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube update -r --latest --prod
	assert_success

	# Prod deps bumped past their pins.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-2/package.json
	assert_success

	# Dev deps left alone — --prod skipped them.
	run grep '"@pnpm.e2e/foo": "100.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.0.0"' project-2/package.json
	assert_success
}

@test "aube update --latest <pkg>: downgrades prerelease to the latest dist-tag" {
	# Ported from pnpm/test/update.ts:659 ('update with tag @latest will
	# downgrade prerelease'). pnpm uses `pnpm update <pkg>@latest` to
	# force the latest dist-tag; aube doesn't parse `<pkg>@<spec>` in
	# update args (see PNPM_TEST_IMPORT.md), so translate to
	# `aube update --latest <pkg>` — same effect: rewrite the manifest
	# to track the resolved version, even when that downgrades a
	# prerelease pin.
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-prerelease-downgrade",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/has-prerelease@3.0.0-rc.0'
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success

	run aube update --latest '@pnpm.e2e/has-prerelease'
	assert_success

	# Manifest now points at the dist-tag's resolved version.
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' package.json
	assert_success
	run grep '@pnpm.e2e/has-prerelease@2.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_failure
}
