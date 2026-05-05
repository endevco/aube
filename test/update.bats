#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Helper: set up a project with is-odd locked to an old version
_setup_outdated_project() {
	cat >package.json <<'EOF'
{
  "name": "test-update",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": ">=0.1.0"
  }
}
EOF

	cat >aube-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      is-odd:
        specifier: '>=0.1.0'
        version: 0.1.2

packages:
  is-number@3.0.0:
    resolution: {integrity: sha512-4cboCqIpliH+mAvFNegjZQ4kgKc3ZUhQVr3HvWbSh5q3WH2v82ct+T2Y1hdU5Gdtorx/cLifQjqCbL7bpznLTg==}
  is-odd@0.1.2:
    resolution: {integrity: sha512-Ri7C2K7o5IrUU9UEI8losXJCCD/UtsaIrkR5sxIcFg4xQ9cRJXlWA5DQvTE0yDc0krvSNLsRGXN11UPS6KyfBw==}
  kind-of@3.2.2:
    resolution: {integrity: sha512-NOW9QQXMoZGg/oqnVNoNTTIFEIid1627WCffUBJEdMxYApq7mNE7CpzucIPc+ZQg25Phej7IJSmX3hO+oblOtQ==}

snapshots:
  is-number@3.0.0:
    dependencies:
      kind-of: 3.2.2
  is-odd@0.1.2:
    dependencies:
      is-number: 3.0.0
  kind-of@3.2.2: {}
EOF
}

@test "aube update: updates a named package to latest matching version" {
	_setup_outdated_project

	run aube update is-odd
	assert_success

	# Lockfile should now have a newer is-odd (3.x)
	run grep 'is-odd@3' aube-lock.yaml
	assert_success

	# node_modules should be populated
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube update: reports version change in output" {
	_setup_outdated_project

	run aube update is-odd
	assert_success
	# Should report the version bump
	assert_output --partial '0.1.2 ->'
}

@test "aube update: all deps updates everything" {
	_setup_outdated_project

	run aube update
	assert_success

	# Should update is-odd
	run grep 'is-odd@3' aube-lock.yaml
	assert_success
}

@test "aube update: skips registry for package.json workspace deps" {
	cat >package.json <<'EOF'
{"workspaces":["sub"],"dependencies":{"happy-sunny-hippo":"workspace:"}}
EOF
	mkdir sub
	cat >sub/package.json <<'EOF'
{"name":"happy-sunny-hippo"}
EOF

	run aube update
	assert_success
	refute_output --partial "package not found"
	assert_file_exists node_modules/happy-sunny-hippo/package.json
}

@test "aube update: updateConfig.ignoreDependencies skips all-deps updates" {
	_setup_outdated_project
	cat >package.json <<'EOF'
{
  "name": "test-update",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": ">=0.1.0"
  },
  "updateConfig": {
    "ignoreDependencies": ["is-odd"]
  }
}
EOF

	run aube update
	assert_success
	run grep 'is-odd@0.1.2' aube-lock.yaml
	assert_success
}

@test "aube update: workspace updateConfig.ignoreDependencies skips all-deps updates" {
	_setup_outdated_project
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "."
updateConfig:
  ignoreDependencies:
    - is-odd
EOF

	run aube update
	assert_success
	run grep 'is-odd@0.1.2' aube-lock.yaml
	assert_success
}

@test "aube update: explicit ignored dependency errors" {
	_setup_outdated_project
	cat >>.npmrc <<'EOF'
updateConfig.ignoreDependencies=["is-odd"]
EOF

	run aube update is-odd
	assert_failure
	assert_output --partial "ignored by updateConfig.ignoreDependencies"
}

@test "aube update: reports already latest when nothing to update" {
	cat >package.json <<'EOF'
{
  "name": "test-update-noop",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	# Install first to get a lockfile
	run aube add is-odd
	assert_success

	# Update should report "already latest"
	run aube update is-odd
	assert_success
	assert_output --partial 'already latest'
}

@test "aube update: errors on unknown package" {
	cat >package.json <<'EOF'
{
  "name": "test-update-unknown",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.0"
  }
}
EOF

	run aube update nonexistent-pkg
	assert_failure
	assert_output --partial "not a dependency"
}

@test "aube update: preserves package.json specifiers" {
	_setup_outdated_project

	run aube update is-odd
	assert_success

	# package.json should still have the original specifier
	run grep '>=0.1.0' package.json
	assert_success
}

@test "aube update --latest --no-save: bumps the lockfile but not package.json" {
	_setup_outdated_project

	run aube update --latest --no-save is-odd
	assert_success
	assert_output --partial 'Skipping package.json update (--no-save)'

	# package.json range stays exactly as the user wrote it.
	run grep '>=0.1.0' package.json
	assert_success

	# The lockfile picked up a newer version than 0.1.2 (the seed pin).
	run grep -c 'is-odd@3' aube-lock.yaml
	assert_success
}
