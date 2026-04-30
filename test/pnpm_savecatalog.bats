#!/usr/bin/env bats
#
# Ported from pnpm/test/saveCatalog.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Status: ALL 8 tests are stubbed pending implementation of `--save-catalog`
# and `--save-catalog-name` flags in `aube add`. aube ships an equivalent-
# adjacent surface (`catalogMode={prefer,strict,manual}`) but the semantics
# differ: catalogMode REWRITES the manifest specifier to `catalog:` when the
# package is *already in* the catalog. pnpm's `--save-catalog` is the missing
# inverse — it WRITES new entries INTO the catalog as part of `add`. Until
# that flag (or a config equivalent) lands in aube, removing the `skip`
# lines below should be enough to validate the new feature against pnpm.
#
# Substitutions for the offline registry (no @pnpm.e2e fixtures yet):
#   @pnpm.e2e/bar    -> is-odd  (versions 0.1.2, 3.0.1)
#   @pnpm.e2e/foo    -> is-even (version 1.0.0)
#   @pnpm.e2e/pkg-a  -> is-odd
#   @pnpm.e2e/pkg-b  -> is-number
#   @pnpm.e2e/pkg-c  -> semver

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube add --save-catalog: writes catalogs to manifest of single-package workspace" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:12

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: ^3.0.1
YAML

	# Initial install: catalog: dep resolves through the workspace catalog.
	run aube install
	assert_success
	run grep -F "is-odd:" aube-lock.yaml
	assert_success
	run grep -F "specifier: catalog:" aube-lock.yaml
	assert_success

	# `aube add --save-catalog` should:
	#   - write `catalog:` (not `^1.0.0`) into package.json dependencies for is-even
	#   - add `is-even: ^1.0.0` into pnpm-workspace.yaml's catalog
	#   - leave the existing is-odd entry untouched
	run aube add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F '"is-even": "catalog:"' package.json
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	run grep -F "is-odd: ^3.0.1" pnpm-workspace.yaml
	assert_success
}

@test "aube add --save-catalog: writes catalogs in a shared-lockfile workspace" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:106

	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
sharedWorkspaceLockfile: true
catalog:
  is-odd: ^3.0.1
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success
	# Single root lockfile records the catalog and project-0's importer.
	assert_file_exists aube-lock.yaml
	run grep -F "is-odd: ^3.0.1" aube-lock.yaml
	assert_success

	# Filtered add into project-1 with --save-catalog: catalog should grow
	# with the is-even entry and project-1's manifest should write `catalog:`.
	run aube --filter=project-1 add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog: writes catalogs in a multi-lockfile workspace" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:213

	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
sharedWorkspaceLockfile: false
catalog:
  is-odd: ^3.0.1
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success
	# Each project gets its own lockfile in this layout.
	assert_file_exists project-0/aube-lock.yaml
	assert_file_exists project-1/aube-lock.yaml

	run aube --filter=project-1 add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F "is-even:" pnpm-workspace.yaml
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog: never adds a workspace: dep to the catalog" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:333

	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0" }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success

	run aube --filter=project-1 add --save-catalog "project-0@workspace:*"
	assert_success
	# project-0 is a local workspace package — must NOT land in the catalog
	# even though --save-catalog was passed.
	run grep -F "project-0:" pnpm-workspace.yaml
	# pnpm-workspace.yaml's only `project-0:` reference should be in `packages:`,
	# not under `catalog:`. Easiest assertion: no `catalog:` block exists.
	run bash -c "grep -E '^catalog:' pnpm-workspace.yaml || true"
	assert_output ""
	# project-1's manifest writes `workspace:*`, not `catalog:`.
	run grep -F '"project-0": "workspace:*"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog: doesn't catalogize deps that were edited into package.json directly" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:392

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: 3.0.1
YAML

	run aube install
	assert_success

	# Edit package.json directly to introduce a new bare-spec dep.
	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": {
    "is-odd": "catalog:",
    "is-number": "*"
  }
}
JSON

	# Now add a third dep with --save-catalog. Only the *added* package
	# (semver) should be catalogized; is-number stays as the bare `*` spec.
	run aube add --save-catalog "semver@^7.0.0"
	assert_success
	run grep -F '"is-odd": "catalog:"' package.json
	assert_success
	run grep -F '"is-number": "*"' package.json
	assert_success
	run grep -F '"semver": "catalog:"' package.json
	assert_success
	# Catalog should have is-odd + semver, but NOT is-number.
	run grep -F "is-odd: 3.0.1" pnpm-workspace.yaml
	assert_success
	run grep -F "semver:" pnpm-workspace.yaml
	assert_success
	run grep -F "is-number:" pnpm-workspace.yaml
	assert_failure
}

@test "aube add --save-catalog: never overwrites an existing catalog entry" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:488

	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	# Catalog deliberately pins an OLD version; --save-catalog should not
	# silently overwrite it, even when adding a higher range for the same pkg.
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: =0.1.2
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success

	run aube add --filter=project-1 --save-catalog "is-even@1.0.0" "is-odd@3.0.1"
	assert_success
	# is-odd's existing catalog pin must be preserved.
	run grep -F "is-odd: =0.1.2" pnpm-workspace.yaml
	assert_success
	# is-even is brand new — gets catalogized.
	run grep -F "is-even: 1.0.0" pnpm-workspace.yaml
	assert_success
	# project-1 gets the explicit is-odd@3.0.1 (NOT catalog:, since the
	# existing catalog entry doesn't match), and is-even via catalog:.
	run grep -F '"is-odd": "3.0.1"' project-1/package.json
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog --recursive: creates a fresh workspace manifest with the new catalog" {
	skip "aube has no --save-catalog flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:593

	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0" }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	# Note: NO pnpm-workspace.yaml exists yet. Recursive --save-catalog
	# should create one with the catalog entry seeded.
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-0
  - project-1
YAML

	run aube add --recursive --save-catalog "is-even@1.0.0"
	assert_success
	run grep -F "is-even: 1.0.0" pnpm-workspace.yaml
	assert_success
	# Both projects now reference the catalog.
	run grep -F '"is-even": "catalog:"' project-0/package.json
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog-name=<name>: writes into a named catalog" {
	skip "aube has no --save-catalog-name flag (see test/PNPM_TEST_IMPORT.md saveCatalog.ts row)"
	# Ported from pnpm/test/saveCatalog.ts:672

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog-name",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: ^3.0.1
YAML

	run aube install
	assert_success

	# Add into a named catalog rather than `default`. Manifest specifier
	# should be `catalog:my-catalog`, and the named catalog should appear
	# under `catalogs:` (plural) in the workspace yaml.
	run aube add --save-catalog-name=my-catalog "is-even@^1.0.0"
	assert_success
	run grep -F '"is-even": "catalog:my-catalog"' package.json
	assert_success
	run grep -F "my-catalog:" pnpm-workspace.yaml
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	# is-odd's default catalog entry stays put.
	run grep -F "is-odd: ^3.0.1" pnpm-workspace.yaml
	assert_success
}
