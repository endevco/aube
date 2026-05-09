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
	# `--depth=-1` is pnpm's spelling for "list project headers only,
	# no deps". project-1 has a real dep (is-odd) so this also locks
	# the contract that `--depth=-1` skips dep enumeration even when
	# the importer has deps to enumerate — the no-deps semantics is
	# distinct from `--depth=0` (which prints direct deps).
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
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {"is-odd": "^3.0.1"}
		}
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
	# `--depth=-1` must NOT emit any dep records (project-1 owns
	# is-odd as a direct dep — make sure it doesn't leak).
	refute_output --partial "is-odd"

	# Sanity: with `--depth=0` (direct deps only) the same fixture
	# does emit project-1's direct dep, so the suppression above is
	# specific to `-1`, not a side effect of the filter.
	run aube list --filter='./packages/**' --parseable --depth=0
	assert_success
	assert_output --partial "is-odd"
}

@test "aube --filter=<pkg> --workspace-root run: includes the workspace root" {
	# Ported from pnpm/test/monorepo/index.ts:1581.
	# pnpm names the command `test`; aube routes the same lifecycle
	# script through `run test` so the assertion stays about workspace
	# selection, not lifecycle shortcut parsing.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('root-ran','')\"" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('project-ran','')\"" }
		}
	EOF

	run aube --filter=project --workspace-root run test --no-install
	assert_success
	assert_file_exists root-ran
	assert_file_exists project/project-ran
}

@test "includeWorkspaceRoot=true: recursive run includes the workspace root" {
	# Ported from pnpm/test/monorepo/index.ts:1613.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('root-ran','')\"" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		includeWorkspaceRoot: true
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('project-ran','')\"" }
		}
	EOF

	run aube -r run test --no-install
	assert_success
	assert_file_exists root-ran
	assert_file_exists project/project-ran
}

# Helper: stand up the four-project workspace pnpm uses for the
# link-workspace-packages tests. Mirrors `preparePackages([{name, version}, …])`
# from pnpm's test harness — a flat layout under the cwd where each
# project owns a `package.json` with `name` + `version`.
_link_workspace_packages_fixture() {
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir project-1 project-2 project-3 project-4
	cat >project-1/package.json <<-'EOF'
		{"name": "project-1", "version": "1.0.0"}
	EOF
	cat >project-2/package.json <<-'EOF'
		{"name": "project-2", "version": "2.0.0"}
	EOF
	cat >project-3/package.json <<-'EOF'
		{"name": "project-3", "version": "3.0.0"}
	EOF
	cat >project-4/package.json <<-'EOF'
		{"name": "project-4", "version": "4.0.0"}
	EOF
}

# Ported from pnpm/test/monorepo/index.ts:112
# ('linking a package inside a monorepo with --link-workspace-packages
# when installing new dependencies'). Default `saveWorkspaceProtocol`
# is `rolling` in aube, matching what pnpm's test asserts: bare
# `aube add project-2` writes `workspace:^` (no version pin), and
# `--save-optional --no-save-workspace-protocol` opts the manifest
# back into a registry-style spec while the resolver still picks up
# the local sibling.
@test "aube add: --link-workspace-packages writes workspace:^ for siblings" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	run aube add project-2
	assert_success
	run aube add project-3 --save-dev
	assert_success
	run aube add project-4 --save-optional --no-save-workspace-protocol
	assert_success

	# Manifest assertions: rolling form for the default save and
	# save-dev flows, registry-style for the explicit opt-out.
	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	run grep -F '"project-3": "workspace:^"' package.json
	assert_success
	run grep -F '"project-4": "^4.0.0"' package.json
	assert_success

	# Each sibling resolved through the local workspace — node_modules
	# entries exist regardless of whether the spec form is workspace
	# or registry style.
	assert_link_exists node_modules/project-2
	assert_link_exists node_modules/project-3
	assert_link_exists node_modules/project-4
}

# Ported from pnpm/test/monorepo/index.ts:156
# ('linking a package inside a monorepo with --link-workspace-packages
# when installing new dependencies and save-workspace-protocol is
# "rolling"'). Aube's default already matches `rolling`, so this test
# pins the explicit setting form — `saveWorkspaceProtocol: rolling`
# in the workspace yaml — and confirms the same outcomes as the
# default-only port above.
@test "aube add: --link-workspace-packages with saveWorkspaceProtocol: rolling" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
		saveWorkspaceProtocol: rolling
	EOF

	cd project-1
	run aube add project-2
	assert_success
	run aube add project-3 --save-dev
	assert_success
	run aube add project-4 --save-optional --no-save-workspace-protocol
	assert_success

	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	run grep -F '"project-3": "workspace:^"' package.json
	assert_success
	run grep -F '"project-4": "^4.0.0"' package.json
	assert_success

	assert_link_exists node_modules/project-2
	assert_link_exists node_modules/project-3
	assert_link_exists node_modules/project-4
}

# Aube-side regression guard for `saveWorkspaceProtocol: true`: the
# pinned-version form (`workspace:^<version>`) is the third valid
# manifest shape, and pnpm's docs document it as the historic default
# even though pnpm's tests have moved to assert the rolling form. The
# test stays in the aube suite (no pnpm equivalent) because the three
# saveWorkspaceProtocol variants share one code path and a regression
# in any one of them would silently slip through the rolling-only
# ports above.
@test "aube add: saveWorkspaceProtocol: true pins workspace:^<version>" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
		saveWorkspaceProtocol: true
	EOF

	cd project-1
	run aube add project-2
	assert_success

	run grep -F '"project-2": "workspace:^2.0.0"' package.json
	assert_success
	assert_link_exists node_modules/project-2
}

# Regression guard for `aube add my-alias@project-2`: the
# `linkWorkspacePackages` eligibility block must skip aliased specs
# because `workspace:` resolves by manifest key, so writing
# `"my-alias": "workspace:^"` would point the resolver at a sibling
# named `my-alias` (which doesn't exist) and 404 on the registry
# fallback. With the skip in place the aliased spec falls through to
# the registry path — which we don't run end-to-end here (the
# offline registry doesn't host `project-2`), but the failure mode
# we want to prevent is the silent `workspace:^` write.
@test "aube add: aliased spec does NOT trigger linkWorkspacePackages workspace match" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	# Aliased to a name that doesn't match any sibling, but the
	# real name (`project-2`) does. Pre-fix this would write
	# `"my-alias": "workspace:^"`. Post-fix the spec falls
	# through to the registry path and fails — the success
	# criterion is that `package.json` does NOT carry a
	# `workspace:` entry for `my-alias`.
	run aube add my-alias@project-2
	# Either failure mode (registry 404) or success is acceptable;
	# the regression guard is the manifest assertion below.
	run grep -F '"my-alias": "workspace:' package.json
	assert_failure
}

# Regression guard for `aube add project-2@^1.0.0` when project-2
# is at version 2.0.0 in the workspace: the user's explicit range
# rules out the local sibling, so the spec must fall through to the
# registry path rather than silently writing a `workspace:^` link
# that resolves to an incompatible version. The bats offline
# registry doesn't host project-2 so the registry path 404s — the
# success criterion is purely that the manifest does NOT carry a
# `workspace:` entry for project-2.
@test "aube add: explicit range mismatching sibling does NOT trigger workspace link" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	# project-2 is at 2.0.0 in the fixture; ^1.0.0 doesn't satisfy.
	run aube add project-2@^1.0.0
	# Don't assert exit status — registry 404 is the expected
	# fall-through. The regression guard is the manifest.
	run grep -F '"project-2": "workspace:' package.json
	assert_failure
}

# Companion to the mismatch guard: `aube add project-2@^2.0.0`
# (which the sibling at 2.0.0 satisfies) MUST trigger the
# workspace match. This locks the satisfies-true branch so a
# regression can't silently skip every explicit-range add.
@test "aube add: explicit range satisfying sibling DOES trigger workspace link" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	run aube add project-2@^2.0.0
	assert_success
	# Default rolling form, since the user-typed range is `^2.0.0`
	# (caret) and the eligible sibling matches.
	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	assert_link_exists node_modules/project-2
}
