#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube import --help" {
	run aube import --help
	assert_success
	assert_output --partial "Convert a supported lockfile into aube-lock.yaml"
}

@test "aube import from package-lock.json writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from package-lock.json"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
	run grep -c "@sindresorhus/is@5.6.0" aube-lock.yaml
	assert_success
}

@test "aube import from yarn.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from yarn.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube import from yarn berry (v2+) yarn.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/yarn.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from yarn.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
	run grep -c "@sindresorhus/is@5.6.0" aube-lock.yaml
	assert_success
}

@test "aube import from npm-shrinkwrap.json writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from npm-shrinkwrap.json"

	assert_file_exists aube-lock.yaml
}

@test "aube import from bun.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from bun.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube install reads bun.lock when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube -v install
	assert_success
	assert_output --partial "bun.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
}

@test "aube install smoke installs messy bun.lock fixture and doesn't change lockfile" {
	cp -R "$PROJECT_ROOT/fixtures/import-bun-messy/." .
	cp bun.lock bun.lock.before

	run aube install
	assert_success

	run cmp -s bun.lock bun.lock.before
	assert_success
}

@test "aube import refuses to overwrite existing aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .
	echo "lockfileVersion: '9.0'" >aube-lock.yaml

	run aube import
	assert_failure
	assert_output --partial "already exists"
}

@test "aube import --force overwrites existing aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .
	echo "stale" >aube-lock.yaml

	run aube import --force
	assert_success
	assert_output --partial "Imported"
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube import errors on bun.lockb (binary format)" {
	cat >package.json <<'EOF'
{"name":"test","version":"1.0.0"}
EOF
	# Write fake binary file
	printf '\x00\x01\x02\x03' >bun.lockb

	run aube import
	assert_failure
	assert_output --partial "bun.lockb"
	assert_output --partial "binary format"
}

@test "aube import errors cleanly when no source lockfile exists" {
	cat >package.json <<'EOF'
{"name":"empty","version":"1.0.0"}
EOF

	run aube import
	assert_failure
	assert_output --partial "no source lockfile found"
}

@test "aube import prefers yarn.lock over package-lock.json when both exist" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import
	assert_success
	assert_output --partial "from yarn.lock"
}

@test "aube install reads package-lock.json when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube -v install
	assert_success
	assert_output --partial "package-lock.json: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
	assert_dir_exists node_modules/kind-of
}

@test "aube install reads yarn.lock when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube -v install
	assert_success
	assert_output --partial "yarn.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
}

@test "aube install reads yarn berry yarn.lock and rewrites it as berry" {
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/yarn.lock" .

	run aube -v install
	assert_success
	assert_output --partial "yarn.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
	assert_dir_exists node_modules/kind-of

	# aube should have preserved berry format on write-back rather
	# than silently downgrading to classic (v1).
	run grep -c "^__metadata:" yarn.lock
	assert_success
	assert_file_not_exists aube-lock.yaml
}

@test "aube install reads npm-shrinkwrap.json when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .

	run aube -v install
	assert_success
	assert_output --partial "npm-shrinkwrap.json: 4 packages"

	assert_dir_exists node_modules/is-odd
}

@test "aube install prefers npm-shrinkwrap.json over package-lock.json when both exist" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube -v install
	assert_success
	assert_output --partial "npm-shrinkwrap.json: 4 packages"
	refute_output --partial "package-lock.json: 4 packages"
}

@test "aube install prefers aube-lock.yaml when multiple lockfiles coexist" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	# First import to produce aube-lock.yaml
	run aube import
	assert_success

	# Now install — should report "Lockfile:", not "package-lock.json:"
	run aube -v install
	assert_success
	assert_output --partial "Lockfile: 4 packages"
	refute_output --partial "package-lock.json: 4 packages"
}

@test "aube import --lockfile-only: accepted (parity no-op)" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import --lockfile-only
	assert_success
	assert_output --partial 'Imported'

	assert_file_exists aube-lock.yaml
	# Confirms the no-op semantics: import never linked node_modules
	# regardless of the flag.
	run test -d node_modules
	assert_failure
}
