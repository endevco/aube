#!/usr/bin/env bats
#
# dlx installs and immediately executes transient bins. Keep these tests out of
# the parallel pool so setup/network failures do not surface as BATS BW01
# command-not-found warnings.
#
# bats file_tags=serial

# Force within-file tests to run one at a time regardless of --jobs.
# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube dlx runs a package binary" {
	# `semver <version>` prints the version back if it parses. Cheap smoke
	# test that proves the bin actually ran, not just that clap swallowed
	# a `--help` flag before it reached the binary. Use --partial because
	# aube's install pipeline interleaves progress lines on stderr, which
	# bats merges into `output`.
	run aube dlx semver 1.2.3
	assert_success
	assert_line "1.2.3"
}

@test "aube dlx -p installs a different package than the bin name" {
	# The `which` npm package ships a binary named `node-which`, not `which`.
	# Running `node-which node` prints the absolute path of the `node`
	# executable on PATH, so we assert the output contains `/node`.
	run aube dlx --package which node-which node
	assert_success
	assert_output --partial "/node"
}

@test "aube dlx falls back to the package's single bin when names differ" {
	# `which` ships its bin as `node-which`, so the naive
	# "bin name == unscoped package name" inference would look for
	# `.bin/which` and fail. With the installed-package fallback aube
	# should pick `node-which` (the only bin) and run it — exactly
	# what `npx which node` does.
	run aube dlx which node
	assert_success
	assert_output --partial "/node"
}

@test "aube dlx accepts an @version suffix on the command" {
	# semver@7.7.4 is what the fixture set has pinned. Run it against a
	# prerelease version so the output is distinguishable from the default.
	run aube dlx semver@7.7.4 1.2.3-alpha.1
	assert_success
	assert_line "1.2.3-alpha.1"
}

@test "aube dlx --shell-mode runs the joined line through sh -c" {
	# `semver 1.2.3` would print "1.2.3"; piping through tr proves the
	# command actually ran inside a shell instead of being exec'd as a
	# single argv. We pass `-p semver` because in shell-mode the first
	# positional is a shell line, not a bin name.
	run aube dlx --shell-mode -p semver 'semver 1.2.3 | tr 0-9 a-z'
	assert_success
	assert_line "b.c.d"
}

@test "aube dlx -c infers the package from the first word when -p is omitted" {
	# Without -p the first whitespace-separated word is taken as the
	# install spec — same convention as plain `aube dlx <cmd>`.
	run aube dlx -c 'semver 7.0.0'
	assert_success
	assert_line "7.0.0"
}

@test "aube dlx accepts the global gvs override after the subcommand" {
	run aube dlx --enable-gvs semver 1.2.3
	assert_success
	assert_line "1.2.3"
}
