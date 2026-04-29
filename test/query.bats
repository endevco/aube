#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube query filters lockfile packages by name" {
	cat >package.json <<'JSON'
{
  "name": "query-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success

	run aube query '[name=is-number]' --parseable
	assert_success
	assert_output --partial $'\tis-number\t'
	assert_output --partial $'\tregistry\ttransitive'
}

@test "aube query supports comma-separated selector groups" {
	cat >package.json <<'JSON'
{
  "name": "query-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  },
  "devDependencies": {
    "is-number": "7.0.0"
  }
}
JSON
	run aube install
	assert_success

	run aube query ':prod, :dev' --json
	assert_success
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"name": "is-number"'
}
