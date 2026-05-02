#!/usr/bin/env bats
#
# Network-dependent ports of pnpm/test/update.ts. These exercise paths
# that hit real upstream services (github.com codeload for git deps),
# which the offline Verdaccio fixture can't host.
#
# Gated behind AUBE_NETWORK_TESTS=1 so the default `mise run test:bats`
# stays offline. CI opts in by setting the env var explicitly.
#
# Mirrors the dist-tag mutation pattern of test/pnpm_update.bats —
# tagged serial and parallel-disabled within the file.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/@pnpm.e2e/bar/package.json \
			test/registry/storage/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json \
			test/registry/storage/@pnpm.e2e/foo/package.json \
			test/registry/storage/@pnpm.e2e/qar/package.json 2>/dev/null || true
	fi
	_common_teardown
}

_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

_require_network() {
	if [ "${AUBE_NETWORK_TESTS:-}" != "1" ]; then
		skip "set AUBE_NETWORK_TESTS=1 to run network tests"
	fi
}

@test "aube update --latest preserves bare github shorthand alongside registry deps" {
	# Ported from pnpm/test/update.ts:143 ('update --latest') with the
	# `kevva/is-negative` GitHub-shorthand assertion restored.
	#
	# Regression guard: aube_lockfile::parse_git_spec recognizes bare
	# `user/repo`, the resolver routes it through the git path, and
	# `aube update --latest` skips non-registry specs in the manifest
	# rewrite (otherwise the bare shorthand would silently become a
	# semver range pin and break install).
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-with-github",
  "version": "0.0.0",
  "dependencies": {
    "is-negative": "kevva/is-negative"
  }
}
JSON

	# Install the github dep first so the lockfile has it, then add the
	# registry deps. Installing through `aube add` would fail today
	# because the CLI add path doesn't recognize bare shorthand —
	# tracked as a separate feature.
	run aube install
	assert_success

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0' '@pnpm.e2e/bar@^100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest
	assert_success

	# Registry deps bumped past their original ranges.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success

	# Manifest specs tracked the new versions, preserving caret + alias.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "\^101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# The github shorthand survives `update --latest` untouched —
	# parse_git_spec recognizes the bare form, the rewrite branch skips
	# it, the lockfile retains the resolved git source.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative + aube update --latest preserves the github shorthand" {
	# Ported from pnpm/test/update.ts:143 ('update --latest') — full
	# end-to-end including `aube add <bare-shorthand>`. pnpm overloads
	# `pnpm install <pkg>` for both add-and-install; aube splits it
	# into `aube add <pkg>`. parse_pkg_spec routes bare `user/repo`
	# through the git-spec branch and writes it verbatim into
	# package.json; the chained install then resolves through the git
	# path.
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-add-github",
  "version": "0.0.0"
}
JSON

	run aube add kevva/is-negative
	assert_success
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0' '@pnpm.e2e/bar@^100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest
	assert_success

	# Registry deps bumped past their original ranges.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success

	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "\^101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# The github shorthand `aube add` wrote survives `update --latest`.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative + aube update --latest -E preserves the github shorthand" {
	# Ported from pnpm/test/update.ts:170 ('update --latest --save-exact')
	# with `kevva/is-negative` restored end-to-end via `aube add`.
	# `--save-exact` (`-E`) drops the caret on registry rewrites; the
	# github shorthand is non-registry and never touched by
	# `update --latest` (rewrite branch skips git specs).
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-exact-add-github",
  "version": "0.0.0"
}
JSON

	run aube add kevva/is-negative
	assert_success

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@100.0.0' 'alias@npm:@pnpm.e2e/qar@100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest -E
	assert_success

	# Registry deps rewritten as exact pins.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@100.1.0"' package.json
	assert_success

	# Github shorthand untouched.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative + aube update -L <name> leaves the github shorthand pinned" {
	# Ported from pnpm/test/update.ts:197 ('update --latest specific
	# dependency'). pnpm uses `pnpm update -L @pnpm.e2e/bar alias
	# is-negative`; aube doesn't update git deps via name lookup (the
	# git-spec branch in the rewrite loop skips them regardless of
	# whether they're named), so we drop `is-negative` from the
	# `update -L` arg list and assert the manifest entry survives
	# unchanged.
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-specific-add-github",
  "version": "0.0.0"
}
JSON

	run aube add kevva/is-negative
	assert_success

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@^100.0.0' '@pnpm.e2e/foo@100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update -L '@pnpm.e2e/bar' alias
	assert_success

	# Named registry deps bumped, caret + alias prefix preserved.
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# Unnamed registry deps stay at their pins.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.0.0"' package.json
	assert_success

	# Github shorthand survives even though the user's `update -L`
	# arg list elsewhere targets registry deps — git specs skip the
	# rewrite loop unconditionally.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}
