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

@test "aube.supportedArchitectures widens the match set" {
	cat >package.json <<-'JSON'
		{
		  "name": "supported-arch-aube-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "aube": {
		    "supportedArchitectures": {
		      "os": ["current", "win32"]
		    }
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# `aube.*` is the native namespace — full parity with `pnpm.*`.
	assert_exists node_modules/aube-test-optional-win32
}

@test "aube.ignoredOptionalDependencies drops a named optional dep" {
	cat >package.json <<-'JSON'
		{
		  "name": "ignored-optional-aube-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "aube": {
		    "supportedArchitectures": { "os": ["current", "win32"] },
		    "ignoredOptionalDependencies": ["aube-test-optional-win32"]
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_not_exists node_modules/aube-test-optional-win32
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
