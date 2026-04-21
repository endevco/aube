#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install creates node_modules" {
	_setup_basic_fixture
	run aube install
	assert_success
	assert_dir_exists node_modules
	assert_dir_exists node_modules/.aube
}

@test "aube install prints 'Already up to date' on a re-run no-op" {
	# Matches pnpm's confirmation message when there's nothing to do.
	# The first install does real work (no output assertion — content
	# varies by cache state); the second run must be a true no-op so
	# we get the concise "Already up to date" line.
	_setup_basic_fixture
	run aube install
	assert_success
	run aube install
	assert_success
	assert_output --partial "Already up to date"
}

@test "aube install does not print 'Already up to date' when it does real work" {
	# Guard against regression: a fresh install (or one that had to
	# recreate node_modules) must not claim the tree was already up
	# to date, even when the global store is warm.
	_setup_basic_fixture
	run aube install
	assert_success
	rm -rf node_modules
	run aube install
	assert_success
	refute_output --partial "Already up to date"
}

@test "aube install --dev installs only devDependencies" {
	cat >package.json <<'JSON'
{
  "name": "install-dev-only",
  "version": "1.0.0",
  "dependencies": {
    "is-even": "1.0.0"
  },
  "devDependencies": {
    "is-odd": "3.0.1"
  }
}
JSON

	run aube install --dev
	assert_success
	assert_link_exists node_modules/is-odd
	assert_not_exists node_modules/is-even
}

@test "aube run auto-reinstalls after --dev install" {
	cat >package.json <<'JSON'
{
  "name": "install-dev-auto",
  "version": "1.0.0",
  "scripts": {
    "check": "node -e 'console.log(require(\"is-even\")(4))'"
  },
  "dependencies": {
    "is-even": "1.0.0"
  },
  "devDependencies": {
    "is-odd": "3.0.1"
  }
}
JSON

	run aube install --dev
	assert_success
	assert_not_exists node_modules/is-even

	run aube run check
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "true"
	assert_link_exists node_modules/is-even
}

@test "aube install --frozen-lockfile works" {
	_setup_basic_fixture
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules
}

@test "aube install --frozen-lockfile errors when no lockfile is present" {
	# pnpm parity: explicit --frozen-lockfile is ERR_PNPM_NO_LOCKFILE
	# when the lockfile is absent. The auto-CI default (see next test)
	# does not share this behavior.
	_setup_basic_fixture
	rm -f aube-lock.yaml
	run aube install --frozen-lockfile
	assert_failure
	assert_output --partial "no lockfile found"
}

@test "aube recursive install honors nested --frozen-lockfile" {
	cat >package.json <<'JSON'
{ "name": "root", "version": "0.0.0", "private": true }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/a
	cat >packages/a/package.json <<'JSON'
{ "name": "a", "version": "1.0.0", "dependencies": { "is-odd": "3.0.1" } }
JSON

	run aube recursive install --frozen-lockfile
	assert_failure
	assert_output --partial "no lockfile found"
}

@test "aube recursive install honors nested --registry" {
	echo "registry=http://127.0.0.1:1/" >.npmrc
	cat >package.json <<'JSON'
{ "name": "root", "version": "0.0.0", "private": true }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/a
	cat >packages/a/package.json <<'JSON'
{ "name": "a", "version": "1.0.0", "dependencies": { "is-odd": "3.0.1" } }
JSON

	run aube recursive --registry="$AUBE_TEST_REGISTRY" install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
}

@test "aube install in CI generates a lockfile when none is present" {
	# pnpm parity: CI=1 auto-enables frozen-lockfile, but only when a
	# lockfile is actually present. With no lockfile, pnpm falls through
	# to a normal resolve and writes one; aube should match so fresh-
	# checkout CI jobs (e.g. docs builds) don't need --no-frozen-lockfile.
	_setup_basic_fixture
	rm -f aube-lock.yaml
	CI=1 run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube install creates top-level entries as symlinks into .aube" {
	_setup_basic_fixture
	run aube install
	assert_success
	# Top-level entries are symlinks into .aube/<dep_path>/node_modules/<name>
	# (matching pnpm's isolated linker layout)
	run test -L node_modules/is-odd
	assert_success
	run test -L node_modules/is-even
	assert_success
	# But following the symlinks should give real directories with the package
	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}

@test "aube install creates .aube virtual store" {
	_setup_basic_fixture
	run aube install
	assert_success
	assert_dir_exists node_modules/.aube/is-odd@3.0.1
	assert_dir_exists node_modules/.aube/is-even@1.0.0
	assert_dir_exists node_modules/.aube/is-number@6.0.0
	assert_dir_exists node_modules/.aube/is-buffer@1.1.6
	assert_dir_exists node_modules/.aube/kind-of@3.2.2
}

@test "aube install creates transitive dep symlinks in .aube" {
	_setup_basic_fixture
	run aube install
	assert_success
	# is-odd@3.0.1 depends on is-number — should be symlinked
	assert_link_exists node_modules/.aube/is-odd@3.0.1/node_modules/is-number
}

@test "aube install creates sibling symlinks in .aube for transitive deps" {
	_setup_basic_fixture
	run aube install
	assert_success
	# is-odd needs is-number. The top-level is-odd is a symlink to
	# .aube/is-odd@<v>/node_modules/is-odd, and is-number lives as a
	# sibling at .aube/is-odd@<v>/node_modules/is-number so Node's
	# directory walk finds it via the symlink.
	assert_link_exists node_modules/is-odd
	# Verify is-number is reachable through is-odd's sibling layout
	run node -e 'console.log(require.resolve("is-number", { paths: [require.resolve("is-odd")] }))'
	assert_success
}

@test "aube install writes state at node_modules/.aube-state" {
	_setup_basic_fixture

	run aube install
	assert_success
	assert_file_exists node_modules/.aube-state
	run cat node_modules/.aube-state
	assert_output --partial "lockfile_hash"
	assert_output --partial "package_json_hashes"
	assert_output --partial "\"layout\""
	assert_output --partial "\"direct_entries\""
	assert_output --partial "\"packages\""
}

@test "aube install does not treat missing top-level entry as up to date" {
	_setup_basic_fixture

	run aube install
	assert_success

	rm node_modules/is-odd

	run aube install
	assert_success
	refute_output --partial "Already up to date"
	assert_link_exists node_modules/is-odd
}

@test "aube run auto-installs when installed package metadata is missing" {
	cat >package.json <<'JSON'
{
  "name": "missing-installed-metadata",
  "version": "1.0.0",
  "scripts": {
    "check": "node -e 'console.log(require(\"is-odd\")(3))'"
  },
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON

	run aube install
	assert_success

	rm node_modules/.aube/is-odd@3.0.1/node_modules/is-odd/package.json

	run aube run check
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "true"
}

@test "installed packages are requireable by node" {
	_setup_basic_fixture
	run aube install
	assert_success
	run node -e "console.log(require('is-odd')(3))"
	assert_success
	assert_output "true"
}

@test "transitive deps resolve correctly" {
	_setup_basic_fixture
	run aube install
	assert_success
	# is-even depends on is-odd@0.1.2 which depends on is-number@3.0.0
	# which depends on kind-of which depends on is-buffer
	# The full chain should resolve
	run node -e "console.log(require('is-even')(4))"
	assert_success
	assert_output "true"
}

@test "aube install handles scoped packages (top-level symlink resolves)" {
	# Scoped packages live at node_modules/@scope/name. The top-level symlink's
	# parent is node_modules/@scope/, not node_modules/, so the relative target
	# must be computed dynamically (not hardcoded). Regression test for the
	# dangling symlink bug where scoped packages got
	# .aube/@scope/name@v/node_modules/@scope/name (resolves to wrong place).
	cat >package.json <<'JSON'
{
  "name": "scoped-test",
  "version": "1.0.0",
  "dependencies": {
    "@types/parse-json": "4.0.0"
  }
}
JSON
	run aube install
	assert_success
	# Top-level entry should be a symlink that actually resolves
	run test -L node_modules/@types/parse-json
	assert_success
	# Following the symlink must reach the real package files
	assert_file_exists node_modules/@types/parse-json/package.json
	# And node should be able to require it
	run node -e 'require("@types/parse-json/package.json")'
	assert_success
}

@test "aube install heals a broken top-level symlink" {
	# Regression: `symlink_metadata().is_ok()` in link_all was treating
	# a dangling symlink as "already in place", so if a top-level entry
	# pointed at a .aube target that had been deleted (or a broken
	# symlink was otherwise present before install), the broken link
	# survived the reinstall and node failed to resolve the package.
	_setup_basic_fixture
	mkdir -p node_modules
	ln -s /definitely/does/not/exist node_modules/is-odd
	# Sanity: the link exists but its target does not.
	assert [ -L node_modules/is-odd ]
	assert [ ! -e node_modules/is-odd ]

	run aube install
	assert_success

	# After install, the symlink must resolve to a real package.
	assert [ -L node_modules/is-odd ]
	assert_file_exists node_modules/is-odd/package.json
	run node -e "console.log(require('is-odd')(3))"
	assert_success
	assert_output "true"
}

@test "aube install fails without package.json" {
	run aube install
	assert_failure
}

@test "aube install without lockfile resolves from scratch" {
	echo '{"name":"test","version":"1.0.0"}' >package.json
	run aube -v install
	assert_success
	assert_output --partial "No lockfile found"
	assert_file_exists aube-lock.yaml
}

@test "aube install --frozen-lockfile errors when lockfile is stale" {
	_setup_basic_fixture
	# Edit package.json to introduce drift (change is-odd's range)
	node -e '
		const fs = require("fs");
		const pkg = JSON.parse(fs.readFileSync("package.json"));
		pkg.dependencies["is-odd"] = "^99.0.0";
		fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
	'
	run aube install --frozen-lockfile
	assert_failure
	assert_output --partial "lockfile is out of date"
	assert_output --partial "is-odd"
}

@test "aube install --frozen-lockfile succeeds when lockfile is fresh" {
	_setup_basic_fixture
	run aube install --frozen-lockfile
	assert_success
}

@test "aube install --prefer-frozen-lockfile re-resolves on drift" {
	_setup_basic_fixture
	# First install to populate node_modules
	run aube install --frozen-lockfile
	assert_success
	# Edit package.json — drift detected, should re-resolve
	node -e '
		const fs = require("fs");
		const pkg = JSON.parse(fs.readFileSync("package.json"));
		pkg.dependencies["is-odd"] = "^3.0.0";
		fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
	'
	run aube -v install --prefer-frozen-lockfile
	assert_success
	assert_output --partial "Lockfile out of date"
	assert_output --partial "re-resolving"
}

@test "aube install --no-frozen-lockfile always re-resolves" {
	_setup_basic_fixture
	run aube -v install --no-frozen-lockfile
	assert_success
	# It should treat the existing lockfile as if it weren't there
	assert_output --partial "No lockfile found"
}

@test "aube install --fix-lockfile is a no-op on a fresh lockfile" {
	# Surgical heal: when nothing has drifted, the lockfile should round-trip
	# byte-for-byte. Proves we're seeding the resolver with the existing
	# lockfile instead of re-resolving everything from scratch. Normalize
	# with `--no-frozen-lockfile` first so the baseline is the current
	# writer's output (not the committed fixture, which may predate a
	# writer revision).
	_setup_basic_fixture
	run aube install --no-frozen-lockfile
	assert_success
	cp aube-lock.yaml aube-lock.yaml.before
	run aube install --fix-lockfile
	assert_success
	run cmp aube-lock.yaml aube-lock.yaml.before
	assert_success
}

@test "aube install --fix-lockfile preserves unchanged deps when new dep is added" {
	# Surgical heal: adding a brand-new dep to package.json must not disturb
	# the locked versions (or integrity hashes) of the existing entries.
	_setup_basic_fixture
	run aube install
	assert_success
	# Snapshot every is-odd / is-even entry from the lockfile so we can
	# diff them against the post-fix version.
	grep -E "(is-odd|is-even)" aube-lock.yaml >entries.before
	# Introduce drift by adding a new leaf dep that exists in the offline
	# fixture registry. isexe@^4.0.0 is a pure-JS leaf — it pulls nothing
	# else in, so any churn we see in other entries is a real regression.
	cat >package.json <<'JSON'
{
  "name": "aube-test-basic",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1",
    "is-even": "^1.0.0",
    "isexe": "^4.0.0"
  }
}
JSON
	run aube install --fix-lockfile
	assert_success
	grep -E "(is-odd|is-even)" aube-lock.yaml >entries.after
	run cmp entries.before entries.after
	assert_success
	# And the new dep actually landed in the lockfile and on disk.
	run grep -F "isexe@4.0.0" aube-lock.yaml
	assert_success
	assert_file_exists node_modules/isexe/package.json
}

@test "aube install --fix-lockfile --lockfile-only is a no-op on a fresh lockfile" {
	# Combined with --lockfile-only, a fresh lockfile should short-circuit
	# on the "lockfile already up to date" fast path, without re-writing
	# aube-lock.yaml or touching node_modules.
	_setup_basic_fixture
	run aube install --no-frozen-lockfile
	assert_success
	rm -rf node_modules
	cp aube-lock.yaml aube-lock.yaml.before
	run aube install --fix-lockfile --lockfile-only
	assert_success
	assert_output --partial "up to date"
	run cmp aube-lock.yaml aube-lock.yaml.before
	assert_success
	assert [ ! -d node_modules ]
}

@test "aube install --fix-lockfile --lockfile-only heals drift without linking" {
	# Combined with --lockfile-only, drift should trigger a surgical
	# heal (unchanged specs keep their pinned versions) and the lockfile
	# should be updated — but node_modules should stay absent.
	_setup_basic_fixture
	run aube install --no-frozen-lockfile
	assert_success
	grep -E "(is-odd|is-even)" aube-lock.yaml >entries.before
	rm -rf node_modules
	cat >package.json <<'JSON'
{
  "name": "aube-test-basic",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1",
    "is-even": "^1.0.0",
    "isexe": "^4.0.0"
  }
}
JSON
	run aube install --fix-lockfile --lockfile-only
	assert_success
	grep -E "(is-odd|is-even)" aube-lock.yaml >entries.after
	run cmp entries.before entries.after
	assert_success
	run grep -F "isexe@4.0.0" aube-lock.yaml
	assert_success
	assert [ ! -d node_modules ]
}

@test "aube install --fix-lockfile conflicts with --frozen-lockfile" {
	_setup_basic_fixture
	run aube install --fix-lockfile --frozen-lockfile
	assert_failure
	assert_output --partial "cannot be used with"
}

@test "aube install --fix-lockfile conflicts with --no-frozen-lockfile" {
	_setup_basic_fixture
	run aube install --fix-lockfile --no-frozen-lockfile
	assert_failure
	assert_output --partial "cannot be used with"
}

@test "aube install --fix-lockfile conflicts with --prefer-frozen-lockfile" {
	_setup_basic_fixture
	run aube install --fix-lockfile --prefer-frozen-lockfile
	assert_failure
	assert_output --partial "cannot be used with"
}

@test "aube install --prod skips devDependencies" {
	cat >package.json <<'JSON'
{
  "name": "prod-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  },
  "devDependencies": {
    "is-even": "^1.0.0"
  }
}
JSON
	# Generate a lockfile that records both prod and dev deps
	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
	assert_file_exists node_modules/is-even/package.json

	# Re-install with --prod: is-even should be pruned, is-odd kept
	rm -rf node_modules
	run aube -v install --prod
	assert_success
	assert_output --partial "--prod: skipping"
	assert_file_exists node_modules/is-odd/package.json
	assert [ ! -e node_modules/is-even ]
}

@test "aube install -P is a short alias for --prod" {
	cat >package.json <<'JSON'
{
  "name": "prod-test",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "is-even": "^1.0.0" }
}
JSON
	run aube install
	assert_success
	rm -rf node_modules
	run aube install -P
	assert_success
	assert [ ! -e node_modules/is-even ]
}

@test "aube run auto-reinstalls after --prod install" {
	# After `install --prod`, `node_modules` is missing devDependencies.
	# `ensure_installed` (called from `aube run`) should detect this via
	# the `prod` flag in the install state and re-install the full graph so dev
	# tooling is available — not silently skip because hashes still match.
	cat >package.json <<'JSON'
{
  "name": "prod-reinstall-test",
  "version": "1.0.0",
  "scripts": {
    "has-dev": "node -e 'require(\"is-even\"); console.log(\"ok\")'"
  },
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "is-even": "^1.0.0" }
}
JSON
	# Establish a full lockfile first.
	run aube install
	assert_success

	# Then install --prod so node_modules is missing is-even.
	rm -rf node_modules
	run aube install --prod
	assert_success
	assert [ ! -e node_modules/is-even ]

	# `aube run has-dev` should trigger an auto-reinstall (full graph)
	# before running the script, not fail with "module not found".
	run aube run has-dev
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "ok"
}

@test "aube install --ignore-scripts is accepted (no-op)" {
	_setup_basic_fixture
	run aube install --ignore-scripts
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube add --ignore-scripts is accepted (no-op)" {
	_setup_basic_fixture
	# Use a tiny package that doesn't actually have scripts; flag should
	# just pass through without erroring.
	run aube add --ignore-scripts --save-dev is-number
	assert_success
	assert_file_exists node_modules/is-number/package.json
}

@test "aube install --prefer-offline reuses cached metadata" {
	_setup_basic_fixture
	# Warm the packument cache and global store with a normal install.
	run aube install
	assert_success
	# Wipe the project so the second run has to re-resolve and re-link,
	# but the packument cache + global CAS are still populated.
	rm -rf node_modules
	run aube install --prefer-offline --no-frozen-lockfile
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube install --offline succeeds from warm cache" {
	_setup_basic_fixture
	run aube install
	assert_success
	rm -rf node_modules
	run aube install --offline
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube install --offline fails on cache miss" {
	_setup_basic_fixture
	# Fresh per-test HOME means the packument cache is empty. Force a
	# re-resolve (--no-frozen-lockfile) so we actually touch the resolver,
	# which will then fail at the first packument fetch under --offline.
	rm -rf node_modules
	run aube install --offline --no-frozen-lockfile
	assert_failure
	assert_output --partial "offline"
}

@test "aube install --offline and --prefer-offline are mutually exclusive" {
	_setup_basic_fixture
	run aube install --offline --prefer-offline
	assert_failure
}

@test "aube install --lockfile-only writes lockfile without node_modules" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube install --lockfile-only
	assert_success
	assert_file_exists aube-lock.yaml
	assert [ ! -e node_modules ]
}

@test "aube install --lockfile-only is a no-op when lockfile is fresh" {
	_setup_basic_fixture
	# Lockfile already exists in fixture; --lockfile-only should detect
	# freshness and exit without writing or linking.
	rm -rf node_modules
	run aube install --lockfile-only
	assert_success
	assert_output --partial "up to date"
	assert [ ! -e node_modules ]
}

@test "aube install --lockfile-only re-resolves on drift" {
	_setup_basic_fixture
	rm -rf node_modules
	# Introduce drift by adding a new dep that isn't in the lockfile.
	node -e '
		const fs = require("fs");
		const pkg = JSON.parse(fs.readFileSync("package.json"));
		pkg.dependencies["is-number"] = "^6.0.0";
		fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
	'
	run aube install --lockfile-only
	assert_success
	assert [ ! -e node_modules ]
	# Lockfile should now mention the new dep
	run grep -q "is-number" aube-lock.yaml
	assert_success
}

@test "aube install --lockfile-only works under CI=1" {
	# CI=1 normally flips the default to --frozen-lockfile, which would
	# hard-error on drift before the lockfile-only short-circuit runs.
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	CI=1 run aube install --lockfile-only
	assert_success
	assert_file_exists aube-lock.yaml
	assert [ ! -e node_modules ]
}

@test "aube install --lockfile-only skips preinstall hook" {
	cat >package.json <<'JSON'
{
  "name": "preinstall-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "echo PREINSTALL_RAN > preinstall.marker"
  },
  "dependencies": { "is-odd": "^3.0.1" }
}
JSON
	run aube install --lockfile-only
	assert_success
	assert_file_exists aube-lock.yaml
	assert [ ! -e preinstall.marker ]
}

@test "aube install --lockfile-only re-resolves on workspace catalog drift" {
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "ws-root", "version": "0.0.0", "private": true }
	EOF
	mkdir -p packages/lib
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF
	# Establish a fresh lockfile.
	run aube install --lockfile-only
	assert_success
	assert_file_exists aube-lock.yaml

	# Edit the catalog entry without touching any package.json.
	# --lockfile-only should detect catalog drift and re-resolve, not no-op.
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.0
	EOF
	run aube -v install --lockfile-only
	assert_success
	refute_output --partial "up to date"
}

@test "aube install --lockfile-only --no-frozen-lockfile forces re-resolve on fresh lockfile" {
	_setup_basic_fixture
	rm -rf node_modules
	# Lockfile is fresh; with --no-frozen-lockfile, --lockfile-only
	# should still re-resolve (the documented "always re-resolve"
	# semantics), not no-op.
	run aube -v install --lockfile-only --no-frozen-lockfile
	assert_success
	refute_output --partial "up to date"
	assert [ ! -e node_modules ]
}

@test "aube install --lockfile-only conflicts with --frozen-lockfile" {
	_setup_basic_fixture
	run aube install --lockfile-only --frozen-lockfile
	assert_failure
}

@test "aube install --network-concurrency caps tarball fetches" {
	# Smoke test: the flag is accepted and the install still
	# succeeds with a very low concurrency. Also covers the
	# --no-verify-store-integrity and --no-side-effects-cache
	# flags — all three settings land in the same PR.
	_setup_basic_fixture
	rm -rf node_modules aube-lock.yaml
	run aube install \
		--network-concurrency 2 \
		--no-verify-store-integrity \
		--no-side-effects-cache
	assert_success
	assert_dir_exists node_modules
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube install honors verify-store-integrity=false from .npmrc" {
	_setup_basic_fixture
	rm -rf node_modules aube-lock.yaml
	echo "verify-store-integrity=false" >.npmrc
	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "aube install ignores --network-concurrency=0 and falls back to default" {
	# 0 would create a zero-permit semaphore and deadlock the
	# fetch loop; the resolver warns and falls back to the
	# built-in default instead of wedging.
	_setup_basic_fixture
	rm -rf node_modules aube-lock.yaml
	run aube -v install --network-concurrency 0
	assert_success
	assert_output --partial "ignoring network-concurrency=0"
	assert_dir_exists node_modules
}

# Fresh resolve: `"<alias>": "npm:<real>@<ver>"` must land as
# `node_modules/<alias>/`, not `node_modules/<real>/`. The resolver
# used to clobber `task.name` to the real name at the `npm:` rewrite
# site, collapsing the alias and breaking `require("<alias>")` at
# runtime. The lockfile round-trip (via `LockedPackage.alias_of`) was
# already correct; this test guards the resolver path.
@test "aube install preserves npm-alias as folder on fresh resolve" {
	cat >package.json <<'JSON'
{
  "name": "alias-fresh",
  "version": "1.0.0",
  "dependencies": {
    "odd-alias": "npm:is-odd@3.0.1"
  }
}
JSON

	run aube install
	assert_success

	# Alias survives as the top-level folder name.
	assert_link_exists node_modules/odd-alias
	assert_not_exists node_modules/is-odd

	# Virtual store entry is keyed by the alias too — a transitive
	# consumer declaring `odd-alias` walks the .aube tree by that
	# exact string, so the folder has to match.
	alias_dir="$(find -L node_modules/.aube -maxdepth 1 -type d -name 'odd-alias@3.0.1*' 2>/dev/null | head -1)"
	assert [ -n "$alias_dir" ]
	assert_not_exists node_modules/.aube/is-odd@3.0.1

	# The emitted lockfile records the real name via `aliasOf:` so a
	# subsequent install hits the real registry entry instead of
	# re-404ing on the alias-qualified tarball URL. Also check the
	# importer still encodes the original `npm:` specifier — without
	# it, drift detection would see `odd-alias: 3.0.1` and re-resolve
	# every install.
	run grep -F "aliasOf: is-odd" aube-lock.yaml
	assert_success
	run grep -F "specifier: npm:is-odd@3.0.1" aube-lock.yaml
	assert_success

	# Round-trip: a second install from the emitted lockfile must
	# re-use the alias identity — otherwise the writer lost the
	# `aliasOf:` data and the reader would try to fetch `odd-alias`
	# from the registry (404).
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_link_exists node_modules/odd-alias
	assert_not_exists node_modules/is-odd
}
