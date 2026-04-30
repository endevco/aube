#!/usr/bin/env bats
#
# `--lockfile-dir` / `lockfileDir`: relocate aube-lock.yaml to a
# different directory than the project root, with the project recorded
# under the lockfile's `importers:` map keyed by its relative path
# (mirrors pnpm's `--lockfile-dir`). See [pnpm/test/install/misc.ts:112]
# for the canonical pnpm behavior this matches.

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install --lockfile-dir: writes lockfile to the named dir, not the project root" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-host",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	# Lockfile lives in the parent, not the project root.
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml

	# node_modules and the linker output stay with the project.
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube install --lockfile-dir: importer key is the project's relative path" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-importer-key",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	# Importer is keyed by `project` (the directory name), not `.`.
	# Match leading whitespace + `project:` to avoid matching the
	# package name embedded inside other lockfile fields.
	run grep -E "^[[:space:]]+project:" ../aube-lock.yaml
	assert_success
	run grep -E "^[[:space:]]+\.:" ../aube-lock.yaml
	assert_failure
}

@test "aube install --lockfile-dir: creates the target directory if missing" {
	# pnpm parity: `--lockfile-dir <missing-path>` materializes the
	# directory rather than aborting with a `canonicalize` ENOENT.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-mkdir",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir ../shared-locks/nested --no-frozen-lockfile
	assert_success

	assert_file_exists ../shared-locks/nested/aube-lock.yaml
	assert_file_not_exists aube-lock.yaml
}

@test "aube install --lockfile-dir: warm install reads the relocated lockfile" {
	# Round-trip: write once, wipe node_modules, install again. The
	# second install must read the relocated lockfile (not regenerate)
	# and the `importers:` block must still use `project`, not `.`.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "lfd-roundtrip",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success

	rm -rf node_modules

	run aube install --lockfile-dir .. --frozen-lockfile
	assert_success
	assert_file_exists node_modules/is-odd/index.js

	run grep -E "^[[:space:]]+project:" ../aube-lock.yaml
	assert_success
}
