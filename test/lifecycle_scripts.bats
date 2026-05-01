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
	# No yaml + no pnpm namespace in package.json → seed lands in
	# package.json#aube.allowBuilds with the canonical placeholder
	# string that matches pnpm's wording.
	assert_file_not_exists aube-workspace.yaml
	run grep -q '"dep-with-build": "set this to true or false"' package.json
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

# -- Ported from pnpm/test/install/lifecycleScripts.ts ------------------------
#
# Existing aube tests above cover most of pnpm's filesystem-marker assertions
# (preinstall ran / postinstall ran / prepare ran / exit-non-zero fails install
# / --ignore-scripts skips hooks / npm_package_* env vars). The block below
# adds the orthogonal stdout-visibility assertions from pnpm's suite (the
# script's echo reaches the user), plus three parity tests that previously
# documented divergences and now ride the corresponding fixes:
# `npm_config_user_agent` is exported, and root postinstall/prepare no longer
# fire on `aube add <pkg>`.

@test "aube install: preinstall script stdout reaches the user" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:43
	# ('preinstall is executed before general installation').
	# Complements the existing filesystem-marker test by also asserting
	# that the script's echoed output makes it through aube's progress UI.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-preinstall-stdout",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "echo HELLO_FROM_PREINSTALL"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_PREINSTALL"
}

@test "aube install: postinstall script stdout reaches the user" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:56
	# ('postinstall is executed after general installation').
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-postinstall-stdout",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "echo HELLO_FROM_POSTINSTALL"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_POSTINSTALL"
}

@test "aube install: prepare script stdout reaches the user (argumentless install)" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:95
	# ('prepare is executed after argumentless installation').
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-prepare-stdout",
  "version": "1.0.0",
  "scripts": {
    "prepare": "echo HELLO_FROM_PREPARE"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_PREPARE"
}

@test "aube: lifecycle scripts receive npm_config_user_agent" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:29
	# ('lifecycle script runs with the correct user agent').
	# aube exports the same env var so dep build scripts (husky,
	# unrs-resolver, node-pre-gyp, etc.) can detect the running PM.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-user-agent",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'console.log(\"UA=\" + (process.env.npm_config_user_agent || \"\"))'"
  }
}
JSON
	run aube install
	assert_success
	# pnpm asserts the user agent starts with `${pkgName}/${pkgVersion}`.
	assert_output --regexp "UA=aube/[0-9]+\.[0-9]+\.[0-9]+"
}

@test "aube add: root postinstall is NOT triggered when adding a named dep" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:69
	# ('postinstall is not executed after named installation').
	# pnpm's contract: lifecycle hooks only run during an argumentless
	# `install` — `pnpm install <pkg>` (i.e. `aube add <pkg>`) skips
	# them so adding a single dep doesn't re-run codegen / build steps.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-named-postinstall",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  }
}
JSON
	run aube add is-odd@3.0.1
	assert_success
	assert [ ! -e postinstall.marker ]
}

@test "aube add: root prepare is NOT triggered when adding a named dep" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:82
	# ('prepare is not executed after installation with arguments').
	# Same contract as the postinstall case above.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-named-prepare",
  "version": "1.0.0",
  "scripts": {
    "prepare": "node -e 'require(\"fs\").writeFileSync(\"prepare.marker\", \"ran\")'"
  }
}
JSON
	run aube add is-odd@3.0.1
	assert_success
	assert [ ! -e prepare.marker ]
}

@test "aube remove: root postinstall is NOT triggered" {
	# Same pnpm contract as the `aube add` cases — root hooks fire only
	# on argumentless `aube install`. `pnpm remove <pkg>` is a chained
	# operation that must not re-run them.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-remove",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	# Seed node_modules with --ignore-scripts so the marker isn't written
	# during setup, then exercise `aube remove` under regular settings.
	run aube install --ignore-scripts
	assert_success
	rm -f postinstall.marker

	run aube remove is-odd
	assert_success
	assert [ ! -e postinstall.marker ]
}

@test "aube update: root postinstall is NOT triggered" {
	# Same pnpm contract — `aube update` is a chained operation.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-update",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	rm -f postinstall.marker

	run aube update
	assert_success
	assert [ ! -e postinstall.marker ]
}

# -- Dep build-policy ports from pnpm/test/install/lifecycleScripts.ts --------
#
# Cover aube's `allowBuilds` review machinery and `--allow-build` CLI
# flag. Aube writes the same canonical `"set this to true or false"`
# placeholder string as pnpm. Aube's strict-dep-builds error message
# differs from pnpm's ("dependencies with build scripts must be reviewed"
# vs "Ignored build scripts:") and the assertions below reflect that.

@test "aube add seeds an allowBuilds review placeholder for unreviewed dep build scripts" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:260
	# ('ignored builds are auto-populated as placeholders in allowBuilds').
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allowbuilds-seed",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_success
	# No workspace yaml present and no `pnpm` namespace in package.json
	# → seed lands in package.json#aube.allowBuilds with the canonical
	# placeholder string.
	assert_file_not_exists pnpm-workspace.yaml
	assert_file_not_exists aube-workspace.yaml
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "set this to true or false"' package.json
	assert_success
}

@test "aube add merges allowBuilds review placeholder with existing approvals in workspace yaml" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:268
	# ('auto-populated placeholders are merged with existing allowBuilds').
	# Pre-existing approval is preserved verbatim; the new build-script
	# dep is appended with the placeholder string.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allowbuilds-merge",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
allowBuilds:
  "@pnpm.e2e/install-script-example": true
YAML
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_success
	# Quote-agnostic checks: aube's yaml_serde rewrites with
	# single-quoted keys, but the test reads as documented behavior
	# rather than serializer detail.
	run grep -E "@pnpm\.e2e/install-script-example['\"]?: true" pnpm-workspace.yaml
	assert_success
	run grep -E "@pnpm\.e2e/pre-and-postinstall-scripts-example['\"]?: ['\"]?set this to true or false['\"]?" pnpm-workspace.yaml
	assert_success
}

@test "aube add fails with strictDepBuilds=true when a dep has unreviewed build scripts" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:226
	# ('throw an error when strict-dep-builds is true and there are
	# ignored scripts'). pnpm's error reads "Ignored build scripts:" and
	# uses `--config.strict-dep-builds=true`; aube has no CLI surface
	# for the setting (reads it from .npmrc / pnpm-workspace.yaml / env)
	# and surfaces a different error string. Common contract: install
	# fails, but the dep + lockfile are still written so the user can
	# flip the placeholder to `true`/`false` and re-run.
	# Append (don't overwrite) so the registry= line _common_setup wrote
	# survives when AUBE_TEST_REGISTRY is set.
	echo "strictDepBuilds=true" >>.npmrc
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-dep-builds-registry",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0"
	# Dep is still written to package.json + lockfile (matches pnpm).
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"' package.json
	assert_success
	assert_file_exists aube-lock.yaml
	# Review placeholder seeded so the user can flip it.
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "set this to true or false"' package.json
	assert_success
}

@test "strictDepBuilds fails even when side-effects are already cached" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:303
	# ('strictDepBuilds fails for packages with cached side-effects (#11035)').
	# Regression: a previously-approved build populates the side-effects
	# cache. After removing the approval, the second install must still
	# fail under strictDepBuilds=true rather than silently restoring the
	# cached output.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-cached",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "@pnpm.e2e/pre-and-postinstall-scripts-example": true
    }
  }
}
JSON
	# First install: build runs, side-effects cache populated.
	run aube install
	assert_success
	assert_file_exists node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js

	# Drop the approval and turn on strictDepBuilds. The cached output
	# is in the store, but aube must still fail rather than silently
	# restore it. Append so the registry= line survives.
	echo "strictDepBuilds=true" >>.npmrc
	echo "optimisticRepeatInstall=false" >>.npmrc
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-cached",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0"
}

@test "aube add --allow-build=<pkg> selectively pre-approves a dep's build scripts" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:149
	# ('selectively allow scripts in some dependencies by --allow-build flag').
	# Adds two build-script packages and pre-approves one via the flag —
	# only the named one runs its build, the other gets the canonical
	# review placeholder.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-selective",
  "version": "1.0.0"
}
JSON
	run aube add \
		--allow-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0 \
		@pnpm.e2e/install-script-example
	assert_success
	# Approved dep ran its install script.
	assert_file_exists node_modules/@pnpm.e2e/install-script-example/generated-by-install.js
	# Unapproved dep did NOT run pre/post-install scripts.
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-preinstall.js ]
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js ]
	# Workspace state: approved entry is `true`, unapproved gets the placeholder.
	run grep -F '"@pnpm.e2e/install-script-example": true' package.json
	assert_success
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "set this to true or false"' package.json
	assert_success
}

@test "aube add --allow-build with no value is rejected by clap" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:164
	# ('--allow-build flag should specify the package'). pnpm's error
	# reads "The --allow-build flag is missing a package name. ..."; aube
	# delegates to clap, which surfaces "a value is required for
	# '--allow-build <PKG>'". Common contract: bare `--allow-build`
	# exits non-zero and does not run the build for any dep. Place the
	# flag last so clap sees it has no value to consume.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-bare",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0 --allow-build
	assert_failure
	assert_output --partial "--allow-build"
	# Build did not run.
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-preinstall.js ]
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js ]
}

@test "aube add --allow-build is rejected when combined with --no-save" {
	# Same conflict pnpm enforces (and that --save-catalog already
	# enforces in aube): --no-save's restore path snapshots only
	# package.json + the lockfile, but --allow-build can land in the
	# workspace yaml — combining them would leak an orphaned approval.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-no-save",
  "version": "1.0.0"
}
JSON
	run aube add --no-save --allow-build=@pnpm.e2e/install-script-example @pnpm.e2e/install-script-example
	assert_failure
	assert_output --partial "--allow-build"
	assert_output --partial "--no-save"
	# Manifest untouched — clap rejected the combo before any write.
	run grep -F '"@pnpm.e2e/install-script-example"' package.json
	assert_failure
}

@test "aube add --allow-build=<pkg> errors when allowBuilds: <pkg>: false already exists" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:347
	# ('--allow-build flag should error when conflicting with allowBuilds: false').
	# Pre-existing explicit deny in pnpm-workspace.yaml. The flag must not
	# silently flip the value — pnpm errors and aube matches the wording
	# verbatim. miette wraps long error lines, so split the assertion
	# into shorter substrings that survive the wrap.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-conflict",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
allowBuilds:
  "@pnpm.e2e/install-script-example": false
YAML
	run aube add \
		--allow-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0 \
		@pnpm.e2e/install-script-example
	assert_failure
	assert_output --partial "ignored by the root project"
	assert_output --partial "allowed to be built by the current command"
	assert_output --partial "@pnpm.e2e/install-script-"
}
