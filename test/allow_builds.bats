#!/usr/bin/env bats
#
# Tests for the `allowBuilds` allowlist that gates dependency
# lifecycle scripts. The fixture package `aube-test-builds-marker`
# (committed under `test/registry/storage/`) has a single `postinstall`
# that writes `aube-builds-marker.txt` to `$INIT_CWD` — the project
# root that aube was invoked from. The marker's presence / absence
# proves whether the script ran, and reading it confirms `INIT_CWD`
# resolved to the real project rather than the pnpm virtual store.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "dep lifecycle scripts are skipped by default" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-default-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
}

@test "pnpm.allowBuilds opts a package in to running its postinstall" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-optin-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
}

@test "pnpm.allowBuilds with false explicitly denies a package" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-deny-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": false
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
}

@test "--dangerously-allow-all-builds runs every dep script" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-dangerous-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install --dangerously-allow-all-builds
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "pnpm-workspace.yaml allowBuilds is honored" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-workspace-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
allowBuilds:
  aube-test-builds-marker: true
YAML
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "pnpm.onlyBuiltDependencies allows a dep script (canonical pnpm format)" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-only-built-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-builds-marker"]
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
}

@test "pnpm.neverBuiltDependencies denies a dep already on the allowlist" {
	# Cross-format precedence: an allow in `onlyBuiltDependencies`
	# is overridden by a deny in `neverBuiltDependencies`, matching
	# pnpm's deny-wins behavior inside `BuildPolicy::decide`.
	cat >package.json <<'JSON'
{
  "name": "allow-builds-never-built-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-builds-marker"],
    "neverBuiltDependencies": ["aube-test-builds-marker"]
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
}

@test "pnpm-workspace.yaml onlyBuiltDependencies is honored" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-workspace-only-built-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
onlyBuiltDependencies:
  - aube-test-builds-marker
YAML
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "pnpm.allowBuilds honors a name wildcard" {
	# `*-marker` is a wildcard pattern that should match our fixture
	# `aube-test-builds-marker` without naming it explicitly — pnpm's
	# `@pnpm/config.matcher` supports the same syntax, so this is a
	# drop-in compatible allowlist form for scopes / suffixes.
	cat >package.json <<'JSON'
{
  "name": "allow-builds-wildcard-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "*-marker": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "pnpm.allowBuilds wildcard deny beats wildcard allow" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-wildcard-deny-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-*": true,
      "*-marker": false
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
}

@test "--ignore-scripts suppresses allowed dep scripts" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-ignore-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	assert_file_not_exists aube-builds-marker.txt
}
