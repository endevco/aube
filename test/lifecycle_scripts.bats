#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# -- Root lifecycle hooks run during `aube install` ---------------------------

@test "aube install runs root preinstall hook" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"preinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube -v install
	assert_success
	assert_file_exists preinstall.marker
}

@test "aube install runs root postinstall hook after deps are linked" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"is-odd\"); require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube -v install
	assert_success
	assert_file_exists postinstall.marker
}

@test "aube install runs prepare hook last" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"order.log\", \"pre\\n\", {flag: \"a\"})'",
    "postinstall": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"post\\n\")'",
    "prepare": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"prepare\\n\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	# Exact order: pre → post → prepare
	run cat order.log
	assert_output "pre
post
prepare"
}

@test "requiredScripts enforces root package scripts" {
	cat >.npmrc <<'EOF'
requiredScripts=build,test
EOF
	cat >package.json <<'JSON'
{
  "name": "required-scripts-test",
  "version": "1.0.0",
  "scripts": {
    "build": "echo build"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "requiredScripts check failed"
	assert_output --partial ". is missing \`test\`"
}

@test "strictDepBuilds fails for unreviewed dependency build scripts" {
	cat >.npmrc <<'EOF'
strictDepBuilds=true
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "strict-dep-builds-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "dep-with-build@1.0.0"
	assert_file_exists pnpm-workspace.yaml
	run grep -q 'dep-with-build: false' pnpm-workspace.yaml
	assert_success
}

@test "strictDepBuilds=false keeps unreviewed dependency build scripts skipped" {
	cat >.npmrc <<'EOF'
strictDepBuilds=false
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "strict-dep-builds-off-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  }
}
JSON
	run aube install
	assert_success
	[ ! -e node_modules/dep-with-build/built.marker ]
}

@test "sideEffectsCacheReadonly restores but does not write dependency build cache" {
	cat >.npmrc <<'EOF'
sideEffectsCacheReadonly=true
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "side-effects-readonly-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["dep-with-build"]
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists node_modules/dep-with-build/built.marker
	[ ! -e node_modules/side-effects-v1 ]
}

@test "aube install fails fast if a root lifecycle script exits non-zero" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "exit 17"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "preinstall"
	# node_modules should NOT have been populated — preinstall runs before link
	assert [ ! -e node_modules/is-odd ]
}

@test "aube install --ignore-scripts skips root lifecycle hooks" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"should-not-exist\", \"x\")'",
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"should-not-exist\", \"x\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	assert [ ! -e should-not-exist ]
	# Deps should still be installed though
	assert_file_exists node_modules/is-odd/package.json
}

@test "root hooks can use binaries from node_modules/.bin via PATH" {
	# Classic pnpm workflow: postinstall invokes a tool installed as a dep.
	# Use is-odd's CLI? — it doesn't have one. Instead use `which` on a
	# known binary we install. Easier: touch a marker from a script and
	# verify PATH contains node_modules/.bin.
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "echo \"$PATH\" > path.log"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	run cat path.log
	assert_output --partial "node_modules/.bin"
}

@test "root hooks receive npm_package_* env vars" {
	cat >package.json <<'JSON'
{
  "name": "env-test-pkg",
  "version": "1.2.3",
  "scripts": {
    "postinstall": "node -e 'console.log(process.env.npm_package_name + \"@\" + process.env.npm_package_version)'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "env-test-pkg@1.2.3"
}

@test "install hooks are a no-op when script field is undefined" {
	# Just asserting that nothing weird happens when there's nothing to run.
	_setup_basic_fixture
	run aube install
	assert_success
	# No mention of "Running" anything since basic fixture has no lifecycle scripts
	refute_output --partial "Running preinstall"
	refute_output --partial "Running postinstall"
}

# -- Dep lifecycle scripts can invoke transitive bins -------------------------

# Regression test for the bug where a dep's postinstall couldn't spawn
# a bin declared in the dep's own `dependencies` (e.g.
# `unrs-resolver`'s postinstall calling `prebuild-install`). The fix
# writes a per-dep `.bin/` at `.aube/<subdir>/node_modules/.bin/` and
# prepends it to PATH when the dep's lifecycle scripts run.
#
# Fixtures: `aube-test-transitive-consumer` depends on
# `aube-test-transitive-bin` (which ships a bin named
# `aube-transitive-bin-probe`) and has `postinstall:
# "aube-transitive-bin-probe"`. The probe writes
# `aube-transitive-bin-probe.txt` into `$INIT_CWD` when it runs, so
# the marker's presence proves the transitive bin was reachable on
# PATH during the dep's lifecycle script.
@test "dep postinstall can invoke a transitive-dep bin by bare name" {
	cat >package.json <<'JSON'
{
  "name": "transitive-bin-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-transitive-consumer": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-transitive-consumer": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-transitive-bin-probe.txt
}
