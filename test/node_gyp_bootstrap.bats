#!/usr/bin/env bats
#
# Tests for aube's on-demand `node-gyp` bootstrap. When a dep
# lifecycle script needs `node-gyp` and the ambient PATH has none,
# aube installs a pinned copy under
# `$XDG_CACHE_HOME/aube/tools/node-gyp/<bucket>/` and prepends its
# `.bin` to the script's PATH. The offline Verdaccio fixture ships
# node-gyp and its transitive deps under `test/registry/storage/`
# so this test runs without network access.

setup() {
	load 'test_helper/common_setup'
	_common_setup

	# Scrub any inherited `node-gyp` off PATH so the only way the
	# lifecycle script below can resolve it is through the bootstrap.
	# Verdaccio is reached via AUBE_TEST_REGISTRY; _common_setup
	# already wrote it into .npmrc, which propagates to the nested
	# `aube install` the bootstrap spawns.
	local sanitized=""
	local entry
	while IFS= read -r entry; do
		if [ -z "$entry" ]; then
			continue
		fi
		if [ -x "$entry/node-gyp" ] || [ -f "$entry/node-gyp" ]; then
			continue
		fi
		sanitized="${sanitized}${sanitized:+:}${entry}"
	done < <(printf '%s\n' "$PATH" | tr ':' '\n')
	export PATH="$sanitized"
	if command -v node-gyp >/dev/null 2>&1; then
		skip "node-gyp still on PATH after scrub ($(command -v node-gyp)); cannot exercise bootstrap"
	fi
}

teardown() {
	_common_teardown
}

@test "bootstrap installs node-gyp when missing from PATH" {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
	# Minimal project that would run an `install` lifecycle script.
	# We don't need the script to actually succeed — we just need a
	# dep whose install phase triggers `run_dep_lifecycle_scripts`,
	# which is gated on `has_dep_lifecycle_work`. Use the existing
	# `aube-test-binding-gyp` fixture: it has a binding.gyp and no
	# install/preinstall, so aube's `default_install_script`
	# fallback runs `node-gyp rebuild`. The rebuild will fail (no C
	# toolchain, no real Python wiring to this fixture), but by then
	# the bootstrap has already run — which is what we're asserting.
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-bootstrap-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-binding-gyp": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-binding-gyp": true
    }
  }
}
JSON
	# Don't `assert_success` — the `node-gyp rebuild` subprocess may
	# fail without a real native toolchain. We only care that the
	# bootstrap landed node-gyp into the aube cache.
	run aube install
	assert_dir_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12/node_modules/.bin"
	assert_file_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12/node_modules/.bin/node-gyp"
}
