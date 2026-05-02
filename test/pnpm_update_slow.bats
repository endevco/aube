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
