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
	assert [ -z "$output" ]
	# `assert` can't wrap `[[ ... ]]` (bash keyword, not a command), so use grep.
	assert grep -qF "is-odd" <<<"$stderr"
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
	assert_file_not_exists aube-lock.yaml
}

@test "aube --lockfile-dir: writes the lockfile to a parent dir with a relative importer key" {
	# Ported from pnpm/test/install/misc.ts:112 ('install with external
	# lockfile directory'). pnpm's `install <pkg> --lockfile-dir ../`
	# becomes aube's `add <pkg> --lockfile-dir ../`. is-positive
	# substituted with is-odd.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "pnpm-misc-lockfile-dir",
  "version": "1.0.0"
}
JSON

	cd project || return
	run aube --lockfile-dir .. add is-odd
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	# Lockfile must land in the parent dir, not next to package.json.
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml
	# Importer key in the lockfile is the project's path relative to
	# the lockfile dir — `project` here, not `.` (which would mean
	# the parent dir is itself the project).
	run grep -E '^  project:$' ../aube-lock.yaml
	assert_success
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

@test "aube install: bin files from deps are on PATH for the root postinstall script" {
	# Ported from pnpm/test/install/misc.ts:36 ('bin files are found by
	# lifecycle scripts'). Uses the @pnpm.e2e/hello-world-js-bin fixture
	# now available via test/registry/storage/.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-bin-in-lifecycle",
  "version": "1.0.0",
  "dependencies": { "@pnpm.e2e/hello-world-js-bin": "*" },
  "scripts": { "postinstall": "hello-world-js-bin" }
}
JSON

	run aube install
	assert_success
	assert_output --partial "Hello world!"
}

@test "aube run: a script can invoke a bin from an installed dep" {
	# Ported from pnpm/test/install/misc.ts:219 ('run js bin file').
	# pnpm runs `npm test`; we use `aube run test` to keep the assertion
	# purely about aube's PATH wiring for run-scripts.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-run-js-bin",
  "version": "1.0.0",
  "scripts": { "test": "hello-world-js-bin" }
}
JSON

	run aube add @pnpm.e2e/hello-world-js-bin
	assert_success

	run aube run test
	assert_success
	assert_output --partial "Hello world!"
}

@test "aube add: creates package.json if there is none" {
	# Ported from pnpm/test/install/misc.ts:233 ('create a package.json
	# if there is none'). pnpm `install <pkg>` ≈ aube `add <pkg>`.
	# is-positive substituted with is-odd.

	# Deliberately no package.json in cwd. _common_setup parks us in a
	# fresh tmp dir with HOME isolated, so the find_project_root walk
	# can't escape into the user's real home and find a package.json
	# higher up.
	run aube add is-odd@3.0.1
	assert_success
	assert_file_exists package.json
	assert_file_exists node_modules/is-odd/index.js

	run cat package.json
	assert_output --partial '"is-odd"'
	assert_output --partial '"3.0.1"'
}

@test "aube add: fails when no package name is provided" {
	# Ported from pnpm/test/install/misc.ts:245 ('pnpm add should fail
	# if no package name was provided'). Asserts exit code + error text;
	# the wording is deliberately generic ('packages') so a future
	# rephrasing won't break the test.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-add-no-name",
  "version": "1.0.0"
}
JSON

	run aube add
	assert_failure
	assert_output --partial "no packages specified"
}

@test "aube add: a tarball with case-only filename collisions installs cleanly" {
	# Ported from pnpm/test/install/misc.ts:163 ('don't fail on case
	# insensitive filesystems when package has 2 files with same name').
	# pnpm's version asserts on its StoreIndex internals to confirm both
	# Foo.js and foo.js are tracked — that's pnpm-specific. We just assert
	# that the install succeeds and the package appears under node_modules,
	# which is the user-visible parity guarantee. The store-side
	# case-collision handling is an aube-internal CAS concern.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-case-conflict",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm.e2e/with-same-file-in-different-cases
	assert_success
	assert_dir_exists 'node_modules/@pnpm.e2e/with-same-file-in-different-cases'
	assert_file_exists 'node_modules/@pnpm.e2e/with-same-file-in-different-cases/package.json'
}
