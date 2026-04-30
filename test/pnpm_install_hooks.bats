#!/usr/bin/env bats
#
# Ported from pnpm/test/install/hooks.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Coverage focus: .pnpmfile.cjs hook behavior — readPackage (sync/async),
# afterAllResolved (async), and pnpmfile load-time error paths.
# @pnpm.e2e/* fixtures aren't mirrored yet, so tests that require them
# (most of hooks.ts uses `@pnpm.e2e/pkg-with-1-dep` + addDistTag) are
# substituted with packages already in test/registry/storage/ where the
# behavior is package-agnostic, or deferred until Phase 0 fixtures land.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "pnpmfile: async readPackage hook mutates transitive dependencies" {
	# Ported from pnpm/test/install/hooks.ts:43 ('readPackage async hook').
	# Substitution: pnpm's @pnpm.e2e/pkg-with-1-dep + addDistTag → is-even
	# (pulls in is-odd transitively). The hook strips is-odd's deps.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-readpackage-async",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.dependencies = {}
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	# The hook cleared is-odd's dependencies; the snapshot entry should
	# be empty rather than pulling in is-number / kind-of.
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep '^  is-odd@'"
	assert_output --partial 'is-odd@0.1.2: {}'
	# Belt-and-braces: confirm the transitive chain is-odd → is-number
	# → kind-of was actually short-circuited, not just hidden in the
	# snapshot. A regression that empties the snapshot entry while still
	# resolving the deps would otherwise sneak through.
	run grep 'is-number' aube-lock.yaml
	assert_failure
	run grep 'kind-of' aube-lock.yaml
	assert_failure
}

@test "pnpmfile: async afterAllResolved hook runs and is awaited" {
	# Ported from pnpm/test/install/hooks.ts:498 ('pnpmfile: run async
	# afterAllResolved hook'). pnpm asserts on ndjson reporter logs;
	# aube's reporter pipeline doesn't surface context.log to stdout the
	# same way, so we assert the install completes (i.e. the async hook
	# is awaited and its return value reused) and that the lockfile got
	# written.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-afterallresolved-async",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async afterAllResolved (lockfile, context) {
      // No mutation; just exercise the async path. Return a promise
      // that resolves on the next tick so we know aube actually
      // awaited it rather than dropping it on the floor.
      await new Promise((resolve) => setImmediate(resolve))
      return lockfile
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	assert_file_exists node_modules/is-odd/index.js
}

@test "pnpmfile: syntax error in .pnpmfile.cjs fails the install" {
	# Ported from pnpm/test/install/hooks.ts:292 ('prints meaningful error
	# when there is syntax error in .pnpmfile.cjs').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-syntax-error",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	# `/boom` parses as the start of a regex literal with no closing /.
	echo '/boom' >.pnpmfile.cjs

	run aube install
	assert_failure
	# The exact wording may differ from pnpm; just assert aube surfaces
	# *some* signal that the pnpmfile is the culprit.
	assert_output --partial 'pnpmfile'
}

@test "pnpmfile: require() of a missing module fails the install" {
	# Ported from pnpm/test/install/hooks.ts:303 ('fails when .pnpmfile.cjs
	# requires a non-existed module').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-require-missing",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	echo 'module.exports = require("./this-does-not-exist")' >.pnpmfile.cjs

	run aube install
	assert_failure
	assert_output --partial 'pnpmfile'
}

@test "pnpmfile: readPackage hook can set optionalDependencies / peerDependencies / devDependencies on a transitive" {
	# Ported from pnpm/test/install/hooks.ts:528 ('readPackage hook
	# normalizes the package manifest'). Substitution: pnpm's
	# @pnpm.e2e/dep-of-pkg-with-1-dep → is-odd; is-positive / is-negative
	# → is-number (already in test/registry/storage/). Verifies the
	# resolver doesn't choke when the hook sets optional/peer/dev fields
	# on a transitive that doesn't originally declare them.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-normalize",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.optionalDependencies = pkg.optionalDependencies || {}
        pkg.optionalDependencies['is-number'] = '*'
        pkg.peerDependencies = pkg.peerDependencies || {}
        pkg.peerDependencies['is-number'] = '*'
        pkg.devDependencies = pkg.devDependencies || {}
        pkg.devDependencies['is-number'] = '*'
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/is-even/index.js
	# is-odd is transitive (via is-even), so it lands in the virtual
	# store rather than at node_modules root. The peer-context suffix
	# (`_is-number@3.0.0`) is what tells us aube actually applied the
	# hook's peerDependencies edit before resolving.
	assert_dir_exists node_modules/.aube/is-odd@0.1.2_is-number@3.0.0/node_modules/is-odd
}

@test "pnpmfile: readPackage that returns undefined fails the install" {
	skip "aube divergence: aube continues with the original manifest when readPackage returns undefined; pnpm fails the install. File a Discussion before un-skipping."
	# Ported from pnpm/test/install/hooks.ts:68 ('readPackage hook makes
	# installation fail if it does not return the modified package
	# manifests'). Skipped pending https://github.com/endevco/aube/discussions
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-undef-return",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {}
  }
}
EOF

	run aube install
	assert_failure
}

@test "pnpmfile: readPackage hook can mutate the root project's dependencies" {
	skip "aube divergence: aube does not run readPackage on the root project's manifest, so deps added by the hook are not installed. pnpm does. File a Discussion before un-skipping."
	# Ported from pnpm/test/install/hooks.ts:551 ('readPackage hook
	# overrides project package'). Skipped pending
	# https://github.com/endevco/aube/discussions
	cat >package.json <<'JSON'
{
  "name": "test-read-package-hook",
  "version": "0.0.0"
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'test-read-package-hook') {
        pkg.dependencies = { 'is-odd': '3.0.1' }
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	# package.json on disk should NOT have been written — the hook only
	# mutates the in-memory manifest.
	run cat package.json
	refute_output --partial '"dependencies"'
}
