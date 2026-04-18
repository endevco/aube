#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_script_project() {
	cat >package.json <<-'JSON'
		{
		  "name": "guardrails-probe",
		  "version": "1.0.0",
		  "scripts": {
		    "ok": "echo ok",
		    "env": "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || '')); console.log('FORCE_COLOR=' + (process.env.FORCE_COLOR || '')); console.log('CLICOLOR_FORCE=' + (process.env.CLICOLOR_FORCE || ''))\""
		  }
		}
	JSON
}

_setup_workspace_fixture() {
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .
}

@test "color setting from .npmrc disables color for child processes" {
	_make_script_project
	echo "color=false" >.npmrc
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	run aube run env
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
	[[ "$output" != *"FORCE_COLOR=1"* ]]
}

@test "color setting from environment forces color for child processes" {
	_make_script_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	NPM_CONFIG_COLOR=always run aube run env
	assert_success
	[[ "$output" == *"FORCE_COLOR=1"* ]]
	[[ "$output" == *"CLICOLOR_FORCE=1"* ]]
}

@test "color setting honors --workspace-root before chdir" {
	_setup_workspace_fixture
	node <<-'NODE'
		let p = require("./package.json")
		p.scripts = { env: "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || ''))\"" }
		require("fs").writeFileSync("package.json", JSON.stringify(p))
	NODE
	{
		echo "color=false"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc
	cd packages/app
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	run aube --workspace-root run env
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
}

@test "color setting from environment works when startup cwd lookup fails" {
	_make_script_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	NPM_CONFIG_COLOR=false run aube --workspace-root run env
	assert_failure
	[[ "$output" == *"no aube-workspace.yaml or pnpm-workspace.yaml found"* ]]
	[[ "$output" != *$'\033['* ]]
}

@test "loglevel setting from .npmrc enables debug logging" {
	_setup_basic_fixture
	echo "loglevel=debug" >.npmrc

	run --separate-stderr aube install
	assert_success
	[[ "$stderr" == *"DEBUG"* ]]
}

@test "packageManagerStrict rejects unsupported package managers" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "verifyDepsBeforeRun=false" >.npmrc

	run aube run ok
	assert_failure
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrict=false skips packageManager guard" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrict=false"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "packageManagerStrict checks workspace root from package subdirectory" {
	_setup_workspace_fixture
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "verifyDepsBeforeRun=false" >.npmrc
	cd packages/app

	run aube run start
	assert_failure
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrictVersion rejects mismatched aube version" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="aube@0.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrictVersion=true"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_failure
	[[ "$output" == *"requires aube@0.0.0"* ]]
}

@test "packageManagerStrictVersion accepts aube version with corepack hash" {
	_make_script_project
	current="$(aube --version | awk '{print $2}')"
	node -e 'let p=require("./package.json"); p.packageManager=`aube@'"$current"'+sha512.abc123`; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrictVersion=true"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "bare aube prints help without packageManager guardrail" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'

	run aube
	assert_success
	[[ "$output" == *"A fast Node.js package manager"* ]]
}

@test "recursiveInstall=false limits plain install to root importer" {
	_setup_workspace_fixture
	node -e 'let p=require("./package.json"); p.dependencies={"@test/lib":"workspace:*","is-number":"^6.0.0"}; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "recursiveInstall=false" >.npmrc

	run aube install
	assert_success
	assert_dir_exists node_modules/@test/lib
	assert_dir_exists node_modules/is-number
	assert_not_exists packages/lib/node_modules
	assert_not_exists packages/app/node_modules
}

@test "recursiveInstall=false does not block explicit --filter" {
	_setup_workspace_fixture
	echo "recursiveInstall=false" >.npmrc

	run aube install --filter @test/lib
	assert_success
	assert_dir_exists packages/lib/node_modules/is-odd
	assert_not_exists packages/app/node_modules
}

@test "verifyDepsBeforeRun=false skips auto-install before run" {
	_make_script_project
	echo "verifyDepsBeforeRun=false" >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "verifyDepsBeforeRun=error fails instead of auto-installing" {
	_make_script_project
	echo "verifyDepsBeforeRun=error" >.npmrc

	run aube run ok
	assert_failure
	[[ "$output" == *"dependencies need install before run"* ]]
}

@test "verifyDepsBeforeRun=error fails after node_modules is removed" {
	_setup_basic_fixture
	node -e 'let p=require("./package.json"); p.scripts={dev:"echo hello-dev"}; require("fs").writeFileSync("package.json", JSON.stringify(p))'

	run aube install
	assert_success
	assert_dir_exists node_modules

	rm -rf node_modules
	echo "verifyDepsBeforeRun=error" >.npmrc

	run aube dev
	assert_failure
	[[ "$output" == *"dependencies need install before run"* ]]
	[[ "$output" != *"hello-dev"* ]]
}

@test "npmPath delegates npm-only fallback commands" {
	fake_npm="$TEST_TEMP_DIR/fake-npm"
	cat >"$fake_npm" <<-'SH'
		#!/usr/bin/env bash
		printf 'fake-npm %s\n' "$*"
	SH
	chmod +x "$fake_npm"
	printf 'npmPath=%s\n' "$fake_npm" >.npmrc

	run aube whoami --registry=https://registry.example.test/
	assert_success
	[[ "$output" == *"fake-npm whoami --registry=https://registry.example.test/"* ]]
}

@test "npmPath fallback keeps child stderr visible under --silent" {
	fake_npm="$TEST_TEMP_DIR/fake-npm"
	cat >"$fake_npm" <<-'SH'
		#!/usr/bin/env bash
		printf 'fake-npm stderr %s\n' "$*" >&2
	SH
	chmod +x "$fake_npm"
	printf 'npmPath=%s\n' "$fake_npm" >.npmrc

	run --separate-stderr aube --silent whoami
	assert_success
	[[ "$stderr" == *"fake-npm stderr whoami"* ]]
}
