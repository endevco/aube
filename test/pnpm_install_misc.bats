#!/usr/bin/env bats
#
# Ported from pnpm/test/install/misc.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Note: pnpm uses `install <pkg>` for both "install everything" and "add a
# new dep". aube splits these — `aube install` only re-installs declared
# deps, and `aube add <pkg>` adds a new one. Tests that pass a package to
# `pnpm install` translate to `aube add` here.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube add -E -D: combines --save-exact and --save-dev" {
	# Ported from pnpm/test/install/misc.ts:124 ('install --save-exact')
	# is-positive substituted with is-odd (already in test/registry/storage/).
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-save-exact-dev",
  "version": "0.0.0"
}
JSON

	run aube add -E -D is-odd@3.0.1
	assert_success
	assert_file_exists node_modules/is-odd/index.js

	run cat package.json
	assert_output --partial '"devDependencies"'
	assert_output --partial '"is-odd": "3.0.1"'
	refute_output --partial '"is-odd": "^'
	refute_output --partial '"is-odd": "~'
	# is-odd should land in devDependencies, not dependencies.
	refute_output --partial '"dependencies"'
}
