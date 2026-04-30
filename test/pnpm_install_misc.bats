#!/usr/bin/env bats
#
# Ported from pnpm/test/install/misc.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Note: pnpm uses `install <pkg>` for both "install everything" and "add a
# new dep". aube splits these — `aube install` only re-installs declared
# deps, and `aube add <pkg>` adds a new one. Tests that pass a package to
# `pnpm install` translate to `aube add` here.

bats_require_minimum_version 1.5.0

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

@test "aube --use-stderr add: writes everything to stderr, stdout stays empty" {
	# Ported from pnpm/test/install/misc.ts:73 ('write to stderr when
	# --use-stderr is used'). is-positive substituted with is-odd.
	# pnpm's `install <pkg>` ≈ aube `add <pkg>`.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-use-stderr",
  "version": "0.0.0"
}
JSON

	run --separate-stderr aube --use-stderr add is-odd
	assert_success
	[ -z "$output" ]
	[[ "$stderr" == *"is-odd"* ]]
}

@test "aube add: lockfile=false in pnpm-workspace.yaml suppresses aube-lock.yaml" {
	# Ported from pnpm/test/install/misc.ts:83 ('install with lockfile being
	# false in pnpm-workspace.yaml'). is-positive substituted with is-odd.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-lockfile-false",
  "version": "0.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
lockfile: false
YAML

	run aube add is-odd
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	assert [ ! -e aube-lock.yaml ]
}

@test "aube install --prefix: runs install in the named subdirectory" {
	# Ported from pnpm/test/install/misc.ts:97 ('install from any location
	# via the --prefix flag'). rimraf substituted with is-odd; we don't
	# assert on .bin/is-odd because is-odd doesn't ship a bin.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "pnpm-misc-prefix",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	# Stay in the parent dir; --prefix points at the project subdir.
	run aube install --prefix project
	assert_success
	assert_file_exists project/node_modules/is-odd/index.js
}

@test "aube add: saves the dependency spec verbatim (no rewriting tilde to caret)" {
	# Ported from pnpm/test/install/misc.ts:150 ('install save new dep with
	# the specified spec'). is-positive@~3.1.0 substituted with is-odd@~3.0.0.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-spec-verbatim",
  "version": "0.0.0"
}
JSON

	run aube add is-odd@~3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"is-odd": "~3.0.0"'
	refute_output --partial '"is-odd": "^'
}
