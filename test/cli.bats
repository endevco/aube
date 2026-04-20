#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube --version" {
	run aube --version
	assert_success
	assert_output --partial "aube"
}

@test "aube --help" {
	run aube --help
	assert_success
	assert_output --partial "fast Node.js package manager"
}

@test "aube install --help" {
	run aube install --help
	assert_success
	assert_output --partial "frozen-lockfile"
}

@test "aube parses pnpm global workspace/output flags" {
	run aube --help
	assert_success
	assert_output --partial "--dir"
	assert_output --partial "--filter-prod"
	assert_output --partial "--workspace-root"
	assert_output --partial "--workspace-packages"
	assert_output --partial "--aggregate-output"
	assert_output --partial "--use-stderr"
}

@test "aube update parses pnpm workspace selection flags" {
	run aube update --help
	assert_success
	assert_output --partial "--recursive"
	assert_output --partial "--workspace"
	assert_output --partial "--interactive"
	assert_output --partial "--no-optional"
}

@test "aube exec --shell-mode resolves commands from PATH" {
	cat >package.json <<-'EOF'
		{"name":"shell-mode-path","version":"1.0.0"}
	EOF

	run aube exec --no-install --shell-mode node -- -e "console.log('shell-path-ok')"
	assert_success
	assert_output --partial "shell-path-ok"
}

@test "aube store path" {
	run aube store path
	assert_success
	assert_output --partial "aube-store"
}
