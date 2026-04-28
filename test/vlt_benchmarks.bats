#!/usr/bin/env bats
# bats file_tags=serial

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_vlt_fixture() {
	local fixture="$1"
	cp -R "$PROJECT_ROOT/fixtures/vlt-benchmarks/$fixture/." .
}

_resolve_vlt_fixture() {
	# These fixtures mirror https://benchmarks.vlt.sh/ install inputs.
	run aube install \
		--lockfile-only \
		--no-frozen-lockfile \
		--registry=https://registry.npmjs.org/ \
		--reporter append-only \
		--network-concurrency 16
	assert_success
	assert_file_exists aube-lock.yaml
	assert_output --partial "Lockfile written"
}

@test "vlt benchmark fixture: next resolves" {
	_setup_vlt_fixture next
	_resolve_vlt_fixture
}

@test "vlt benchmark fixture: svelte resolves" {
	_setup_vlt_fixture svelte
	_resolve_vlt_fixture
}

@test "vlt benchmark fixture: vue resolves" {
	_setup_vlt_fixture vue
	_resolve_vlt_fixture
}

@test "vlt benchmark fixture: large resolves" {
	_setup_vlt_fixture large
	_resolve_vlt_fixture
}

@test "vlt benchmark fixture: babylon resolves" {
	_setup_vlt_fixture babylon
	_resolve_vlt_fixture
}
