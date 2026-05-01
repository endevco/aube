#!/usr/bin/env bats
#
# Ported from pnpm/test/monorepo/index.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# This file covers Phase 3 batch 1 — filter + `--filter` semantics for
# workspace commands. pnpm's monorepo suite is large (41 tests, 2026
# LOC); the batches in PNPM_TEST_IMPORT.md slice it by topic.

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# pnpm's `preparePackages` creates each package as a sibling subdir
# without writing a root package.json. aube requires a root manifest at
# the workspace root, so all of these fixtures add a private root
# package.json — matching the conventional aube workspace shape and
# keeping the tests focused on filter behavior, not manifest discovery.

_setup_no_match_workspace() {
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{"name": "project", "version": "1.0.0"}
	EOF
}

@test "aube list --filter=<no-match>: warns to stdout and exits 0" {
	# Ported from pnpm/test/monorepo/index.ts:31 ('no projects matched the filters').
	_setup_no_match_workspace

	run aube list --filter=not-exists
	assert_success
	assert_output --partial "No projects matched the filters in"
}

@test "aube list --filter=<no-match> --fail-if-no-match: exits 1" {
	# Ported from pnpm/test/monorepo/index.ts:31 (sub-case 2).
	_setup_no_match_workspace

	run aube list --filter=not-exists --fail-if-no-match
	assert_failure
	assert_output --partial "did not match"
}

@test "aube list --filter=<no-match> --parseable: silent stdout, exits 0" {
	# Ported from pnpm/test/monorepo/index.ts:31 (sub-case 3). Machine
	# consumers expect empty stdout on no-match — the warning is
	# suppressed when --parseable is requested.
	_setup_no_match_workspace

	run aube list --filter=not-exists --parseable
	assert_success
	assert_output ""
}

@test "aube list --filter=<no-match>: --format parseable / --format json suppress the warning" {
	# Regression: the no-match suppression must check the resolved
	# output format, not just the `--parseable` / `--json` shortcuts.
	# `--format parseable` and `--format json` carry the same
	# machine-readable contract — printing the human "No projects
	# matched..." message would corrupt downstream parsers.
	_setup_no_match_workspace

	run aube list --filter=not-exists --format parseable
	assert_success
	assert_output ""

	run aube list --filter=not-exists --format json
	assert_success
	assert_output ""

	run aube list --filter=not-exists --json
	assert_success
	assert_output ""
}

@test "aube --filter=...<pkg> run: dependents run after the seed (topological order)" {
	# Ported from pnpm/test/monorepo/index.ts:512
	# ('do not get confused by filtered dependencies when searching for
	# dependents in monorepo'). The scenario: project-2 is filtered with
	# `...project-2` so dependents (project-3, project-4) join the run,
	# but two unrelated workspace packages (unused-project-{1,2}) sit in
	# project-2's dep list and shouldn't perturb the dependent search.
	# Topological order requires project-2 to run BEFORE project-3 and
	# project-4 — they depend on it.
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF
	mkdir unused-project-1 unused-project-2 project-2 project-3 project-4
	cat >unused-project-1/package.json <<-'EOF'
		{"name": "unused-project-1", "version": "1.0.0"}
	EOF
	cat >unused-project-2/package.json <<-'EOF'
		{"name": "unused-project-2", "version": "1.0.0"}
	EOF
	cat >project-2/package.json <<-'EOF'
		{
		  "name": "project-2",
		  "version": "1.0.0",
		  "dependencies": {"unused-project-1": "1.0.0", "unused-project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-2')\""}
		}
	EOF
	cat >project-3/package.json <<-'EOF'
		{
		  "name": "project-3",
		  "version": "1.0.0",
		  "dependencies": {"project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-3')\""}
		}
	EOF
	cat >project-4/package.json <<-'EOF'
		{
		  "name": "project-4",
		  "version": "1.0.0",
		  "dependencies": {"project-2": "1.0.0", "unused-project-1": "1.0.0", "unused-project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-4')\""}
		}
	EOF

	cd project-2
	run aube --filter='...project-2' run test
	assert_success
	assert_output --partial "printed by project-2"
	assert_output --partial "printed by project-3"
	assert_output --partial "printed by project-4"

	# Topological order: project-2 (the seed) before its dependents.
	# Flatten the captured output so newlines in install banners don't
	# break the substring search.
	local flat="${output//$'\n'/ }"
	local p2_idx="${flat%%printed by project-2*}"
	local p3_idx="${flat%%printed by project-3*}"
	local p4_idx="${flat%%printed by project-4*}"
	[ "${#p2_idx}" -lt "${#p3_idx}" ]
	[ "${#p2_idx}" -lt "${#p4_idx}" ]
}

# pnpm's "directory filtering" test (monorepo/index.ts:1662) covers two
# sub-cases. Sub-case 1 (`--filter=./packages` matches nothing) is an
# aube divergence: aube's path selector is "at or under", so
# `./packages` already matches packages nested below it. pnpm v9
# changed this to require the explicit `/**` recursive glob, gated on a
# `legacyDirFiltering` workspace setting. aube does not implement that
# setting (see test/PNPM_TEST_IMPORT.md "Explicitly skipped"). Only the
# `./packages/**` sub-case ports cleanly.
@test "aube list --filter=./packages/**: matches every package under the directory" {
	# Ported from pnpm/test/monorepo/index.ts:1662 (sub-case 2).
	# `--depth=-1` is pnpm's spelling for "no transitive deps"; aube
	# accepts it as an alias for `--depth=0`.
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir -p packages/project-1 packages/project-2
	cat >packages/project-1/package.json <<-'EOF'
		{"name": "project-1", "version": "1.0.0"}
	EOF
	cat >packages/project-2/package.json <<-'EOF'
		{"name": "project-2", "version": "1.0.0"}
	EOF

	# Populate the lockfile so `list --parseable` has something to walk.
	run aube install
	assert_success

	run aube list --filter='./packages/**' --parseable --depth=-1
	assert_success
	# Filtered `--parseable` leads each importer with its absolute
	# directory path (matches the help-text contract in list.rs and
	# pnpm's `list --filter=… --parseable` shape). Each project gets
	# its own line ending with the package directory.
	assert_line --regexp '/packages/project-1$'
	assert_line --regexp '/packages/project-2$'
}
