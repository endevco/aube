#!/usr/bin/env bats
#
# Workspace-yaml-only-root support. Common monorepo layouts (Turborepo,
# moon, etc.) keep `pnpm-workspace.yaml` / `aube-workspace.yaml` at the
# repo root with no sibling `package.json` — workspace projects own
# their own manifests and the root carries only the workspace
# declaration. The five workspace-scoped commands (`list`, `run -r`,
# `install`, `query`, `why`) must work from such a root via
# `crate::dirs::project_or_workspace_root()`.
#
# Single-project commands (`add`, `remove`, etc.) deliberately still
# require a `package.json` and surface a helpful error.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Workspace root with no package.json, two real workspace projects.
_setup_yaml_only_workspace() {
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
	YAML
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<-'JSON'
		{
		  "name": "a",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  },
		  "scripts": {
		    "echo": "echo a-ran"
		  }
		}
	JSON
	cat >packages/b/package.json <<-'JSON'
		{
		  "name": "b",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-number": "7.0.0"
		  },
		  "scripts": {
		    "echo": "echo b-ran"
		  }
		}
	JSON
}

@test "aube install from a yaml-only workspace root installs workspace projects" {
	_setup_yaml_only_workspace
	run aube install
	assert_success
	# No root manifest was written — the install path must not synthesize
	# one on disk, only treat the missing manifest as empty.
	run test -f package.json
	assert_failure
	# Each workspace project gets its deps materialized via the root
	# virtual store.
	assert_link_exists packages/a/node_modules/is-odd
	assert_link_exists packages/b/node_modules/is-number
	assert_file_exists aube-lock.yaml
}

@test "aube list -r from a yaml-only workspace root lists every project" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube list -r
	assert_success
	assert_output --partial "a@1.0.0"
	assert_output --partial "b@1.0.0"
	assert_output --partial "is-odd 3.0.1"
	assert_output --partial "is-number 7.0.0"
}

@test "aube run -r from a yaml-only workspace root runs the script in every project" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube run -r echo
	assert_success
	assert_output --partial "a-ran"
	assert_output --partial "b-ran"
}

@test "aube query from a yaml-only workspace root resolves a transitive" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube query '[name=is-odd]'
	assert_success
	assert_output --partial "is-odd"
}

@test "aube why from a yaml-only workspace root walks the graph" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube why is-odd
	assert_success
	assert_output --partial "is-odd 3.0.1"
}

@test "aube list -r from a yaml-only root with no projects prints 'No projects found'" {
	# Ported from pnpm/test/monorepo/index.ts:56 — `prepareEmpty()` +
	# `pnpm list -r` expects "No projects found in <dir>" + exit 0.
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
	YAML
	run aube list -r
	assert_success
	assert_output --partial "No projects found in"
}

@test "aube add from a yaml-only workspace root surfaces a helpful error" {
	# Single-project commands still require a real manifest. `aube add`
	# bootstraps a `{}` package.json in cwd and then refuses to mutate
	# the workspace root without `-W`. Both halves are user-visible
	# failure modes — the regression here is that the error stays
	# helpful instead of becoming a generic "no package.json" panic.
	_setup_yaml_only_workspace
	run aube add is-odd
	assert_failure
	assert_output --partial "workspace root"
}
