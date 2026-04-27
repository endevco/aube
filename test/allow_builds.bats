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

@test "jailBuilds runs approved dep scripts with a scrubbed env and temp HOME" {
	cat >package.json <<'JSON'
{
  "name": "jail-builds-env-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-jailed-build": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-jailed-build"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
YAML
	AUBE_AUTH_TOKEN=aube-secret NPM_TOKEN=npm-secret NODE_AUTH_TOKEN=node-secret GITHUB_TOKEN=gh-secret run aube install
	assert_success
	assert_file_not_exists jail-package-marker.txt
	run find -L node_modules -name jail-package-marker.txt -type f
	assert_success
	assert_output --partial "jail-package-marker.txt"
	run sh -c 'cat $(find -L node_modules -name jail-package-marker.txt -type f | head -n1)'
	assert_success
	assert_output --partial "name=aube-test-jailed-build"
	assert_output --partial "aube-jail"
	home_path="$(printf '%s\n' "$output" | sed -n 's/^home=//p')"
	[ -n "$home_path" ]
	[ ! -d "$home_path" ]
}

@test "jailBuildExclusions glob lets matching packages opt out of jailBuilds" {
	cat >package.json <<'JSON'
{
  "name": "jail-builds-disable-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-jailed-build": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-jailed-build"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
jailBuildExclusions:
  - aube-test-*
YAML
	run env AUBE_AUTH_TOKEN= NPM_TOKEN= NODE_AUTH_TOKEN= GITHUB_TOKEN= aube install
	assert_success
	run sh -c 'cat $(find -L node_modules -name jail-package-marker.txt -type f | head -n1)'
	assert_success
	refute_output --partial "aube-jail"
}

@test "invalid jailBuildPermissions glob warns once before dep scripts run" {
	cat >package.json <<'JSON'
{
  "name": "jail-builds-invalid-permissions-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-jailed-build": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-jailed-build"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
jailBuildPermissions:
  "aube-test-*@1.0.0":
    env:
      - SHARP_DIST_BASE_URL
YAML
	run aube install
	assert_success
	assert_output --partial "warn: jailBuildPermissions:"
	warning_count="$(printf '%s\n' "$output" | grep -c "warn: jailBuildPermissions:")"
	[ "$warning_count" -eq 1 ]
	refute_output --partial "warn: jailBuildExclusions:"
	run sh -c 'cat $(find -L node_modules -name jail-package-marker.txt -type f | head -n1)'
	assert_success
	assert_output --partial "aube-jail"
}

@test "jailBuilds prevents dep scripts from writing to INIT_CWD on supported platforms" {
	if [ "$(uname -s)" != "Darwin" ] && [ "$(uname -s)" != "Linux" ]; then
		skip "native build jail filesystem enforcement is only supported on macOS and Linux today"
	fi
	cat >package.json <<'JSON'
{
  "name": "jail-builds-write-deny-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-builds-marker"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
YAML
	run aube install
	assert_failure
	assert_file_not_exists aube-builds-marker.txt
}

@test "jailBuildPermissions glob can grant matching packages write access on supported platforms" {
	if [ "$(uname -s)" != "Darwin" ] && [ "$(uname -s)" != "Linux" ]; then
		skip "native build jail filesystem enforcement is only supported on macOS and Linux today"
	fi
	cat >package.json <<'JSON'
{
  "name": "jail-builds-write-grant-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-builds-marker"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
jailBuildPermissions:
  aube-test-*:
    write:
      - "."
YAML
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "jailBuilds denies dep script network sockets on supported platforms" {
	if [ "$(uname -s)" != "Darwin" ] && [ "$(uname -s)" != "Linux" ]; then
		skip "native build jail network enforcement is only supported on macOS and Linux today"
	fi
	cat >package.json <<'JSON'
{
  "name": "jail-builds-network-deny-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-jail-network": "^1.0.0"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["aube-test-jail-network"]
  }
}
JSON
	cat >aube-workspace.yaml <<'YAML'
jailBuilds: true
YAML
	run aube install
	assert_success
}

@test "top-level trustedDependencies (bun format) allows a dep script" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-trusted-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "trustedDependencies": ["aube-test-builds-marker"]
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
}

@test "trustedDependencies is overridden by neverBuiltDependencies" {
	cat >package.json <<'JSON'
{
  "name": "allow-builds-trusted-denied-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "trustedDependencies": ["aube-test-builds-marker"],
  "pnpm": {
    "neverBuiltDependencies": ["aube-test-builds-marker"]
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
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
