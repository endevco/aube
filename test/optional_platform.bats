#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# The fixture `aube-test-optional-win32` declares `os: ["win32"]` so on
# Linux and macOS CI it must be skipped silently rather than failing
# the install. This mirrors pnpm's "graceful failure" for optional deps
# with unsatisfiable platform constraints.

@test "optional dep with win32-only os is skipped on non-win32 host" {
	cat >package.json <<-'JSON'
		{
		  "name": "optional-platform-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# Platform-mismatched optional should not land in node_modules.
	assert_not_exists node_modules/aube-test-optional-win32
}

@test "aube-lock.yaml captures cross-platform optional natives by default" {
	# When aube writes its native lockfile format it widens the resolver's
	# platform filter so Linux / Windows / arm64 optional natives are
	# recorded alongside the host's — a lockfile resolved on one platform
	# must install cleanly on every other platform without the user
	# hand-rolling `pnpm.supportedArchitectures`.
	cat >package.json <<-'JSON'
		{
		  "name": "cross-platform-lockfile-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists aube-lock.yaml
	# win32-only optional must still not be linked on non-win32 hosts…
	assert_not_exists node_modules/aube-test-optional-win32
	# …but it MUST land in the committed lockfile so a Windows CI run
	# resolving from this file picks it up.
	run grep -F 'aube-test-optional-win32@1.0.0' aube-lock.yaml
	assert_success
}

@test "required platform-mismatched dep still gets fetched and linked" {
	# Regression guard for the catch-up fetch pass. Streaming fetch
	# defers platform-mismatched tarballs because filter_graph usually
	# drops them — but filter_graph only prunes *optional* edges. A
	# required dep that declares `os: ["win32"]` on a non-Windows host
	# survives the graph trim, so its tarball must be fetched after the
	# stream closes or the linker errors out with a missing store index.
	# This fixture is declared in `dependencies` (NOT optional) so we
	# exercise the exact code path cursor[bot] flagged. pnpm installs
	# the same package the same way (with a warning); aube matches.
	cat >package.json <<-'JSON'
		{
		  "name": "required-platform-mismatch-test",
		  "version": "0.0.0",
		  "dependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# The required-platform-mismatched dep must still land in
	# node_modules: aube honors pnpm's `packageIsInstallable` semantics
	# for required deps, and the catch-up fetch guarantees the store
	# index is present when the linker runs.
	assert_exists node_modules/aube-test-optional-win32/package.json
}

@test "pnpm-lock.yaml keeps pnpm's host-only default" {
	# Aube preserves whatever lockfile format was already on disk. For
	# pnpm-lock.yaml that means matching pnpm's default: optionals for
	# other platforms are NOT baked in unless the user opts in with
	# `pnpm.supportedArchitectures`. Otherwise aube would silently
	# diverge from what `pnpm install` would have produced in the same
	# repo.
	: >pnpm-lock.yaml
	cat >package.json <<-'JSON'
		{
		  "name": "pnpm-lock-host-only",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists pnpm-lock.yaml
	assert_not_exists aube-lock.yaml
	# The manifest's `optionalDependencies:` block round-trips into the
	# importer record regardless of platform (pnpm does the same), so
	# don't grep for the bare name. What we actually care about is the
	# resolved `packages:` entry — that's what a subsequent install on a
	# matching platform would use, and what we want aube to have skipped.
	run grep -F 'aube-test-optional-win32@1.0.0:' pnpm-lock.yaml
	assert_failure
}

@test "pnpm.supportedArchitectures widens the match set" {
	cat >package.json <<-'JSON'
		{
		  "name": "supported-arch-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "pnpm": {
		    "supportedArchitectures": {
		      "os": ["current", "win32"]
		    }
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# With win32 added to the supported set, the optional dep must be
	# installed even on Linux/macOS.
	assert_exists node_modules/aube-test-optional-win32
}

@test "pnpm.ignoredOptionalDependencies drops a named optional dep" {
	cat >package.json <<-'JSON'
		{
		  "name": "ignored-optional-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "pnpm": {
		    "supportedArchitectures": { "os": ["current", "win32"] },
		    "ignoredOptionalDependencies": ["aube-test-optional-win32"]
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# ignoredOptionalDependencies wins even when the platform filter
	# would otherwise allow it through.
	assert_not_exists node_modules/aube-test-optional-win32
}
