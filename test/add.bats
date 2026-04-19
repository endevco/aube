#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube add: adds a package to dependencies" {
	# Start with a minimal package.json
	cat >package.json <<'EOF'
{
  "name": "test-add",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add is-odd
	assert_success

	# package.json should now have is-odd
	run cat package.json
	assert_output --partial '"is-odd"'

	# Lockfile should exist
	assert_file_exists aube-lock.yaml

	# node_modules should be populated
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube add: adds dev dependency with -D" {
	cat >package.json <<'EOF'
{
  "name": "test-add-dev",
  "version": "0.0.0"
}
EOF

	run aube add -D is-odd
	assert_success

	# Should be in devDependencies, not dependencies
	run cat package.json
	assert_output --partial '"devDependencies"'
	assert_output --partial '"is-odd"'
	# Empty dependencies should not be serialized (skip_serializing_if)
	refute_output --partial '"dependencies"'
}

@test "aube add: adds specific version" {
	cat >package.json <<'EOF'
{
  "name": "test-add-version",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add is-odd@^3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"is-odd": "^3.0.0"'
}

@test "aube add -E: pins exact version without a caret" {
	cat >package.json <<'EOF'
{
  "name": "test-save-exact",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add -E is-odd@^3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"is-odd": "3.0.1"'
	refute_output --partial '"is-odd": "^'
}

@test "aube add -E latest: pins exact version for dist-tags" {
	cat >package.json <<'EOF'
{
  "name": "test-save-exact-latest",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add --save-exact is-odd
	assert_success

	run cat package.json
	# No caret prefix; a plain version number.
	refute_output --partial '"is-odd": "^'
	assert_output --regexp '"is-odd": "[0-9]+\.[0-9]+\.[0-9]+"'
}

@test "aube add -E npm:pkg: preserves bare npm: prefix when pinning exact" {
	# Regression: the save_exact branch used to only check spec.alias and
	# silently dropped the `npm:` prefix for bare `npm:pkg@range` specs,
	# writing `"3.0.1"` instead of `"npm:is-odd@3.0.1"`.
	cat >package.json <<'EOF'
{
  "name": "test-save-exact-npm-bare",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add -E npm:is-odd@^3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"is-odd": "npm:is-odd@3.0.1"'
}

@test "aube add jsr: resolves against the @jsr scope and writes jsr:<range>" {
	# The fixture package @jsr/std__collections@1.0.0 in
	# test/registry/storage/@jsr/ is the npm-compat name that JSR serves
	# `@std/collections` under. We point the `@jsr` scope at the local
	# Verdaccio so the resolver's default https://npm.jsr.io redirect
	# doesn't fire and the test stays offline.
	cat >package.json <<'EOF'
{
  "name": "test-add-jsr",
  "version": "0.0.0",
  "dependencies": {}
}
EOF
	echo "@jsr:registry=http://localhost:4873/" >>.npmrc

	run aube add jsr:@std/collections@^1.0.0
	assert_success

	run cat package.json
	# Manifest key is the JSR-style name; specifier uses the `jsr:` prefix
	# with only the range (matches pnpm behavior when alias == JSR name).
	assert_output --partial '"@std/collections": "jsr:^1.0.0"'

	# The install materializes under the JSR-style name while registry IO
	# still uses the npm-compat name recorded below.
	assert_dir_exists node_modules/.aube/@std+collections@1.0.0
	assert_file_exists node_modules/@std/collections/index.js

	# JSR's real npm-compatible registry uses opaque dist.tarball paths,
	# so the lockfile must preserve the packument URL for cold installs.
	run grep "jsr-tarball-1.0.0.tgz" aube-lock.yaml
	assert_success
	run grep "aliasOf: '@jsr/std__collections'" aube-lock.yaml
	assert_success

	rm -rf node_modules "$HOME/.aube-store"
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/@std+collections@1.0.0
	assert_file_exists node_modules/@std/collections/index.js
}

@test "aube add jsr: rejects non-scoped specs up front" {
	cat >package.json <<'EOF'
{
  "name": "test-add-jsr-bad",
  "version": "0.0.0",
  "dependencies": {}
}
EOF
	run aube add jsr:collections
	assert_failure
	assert_output --partial 'JSR packages must be scoped'
}

@test "aube add: adds multiple packages" {
	cat >package.json <<'EOF'
{
  "name": "test-add-multi",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	run aube add is-odd is-even
	assert_success

	run cat package.json
	assert_output --partial '"is-odd"'
	assert_output --partial '"is-even"'

	assert_file_exists node_modules/is-odd/index.js
	assert_file_exists node_modules/is-even/index.js
}

@test "aube add -D: moves dep from dependencies to devDependencies" {
	cat >package.json <<'EOF'
{
  "name": "test-dedup",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.0"
  }
}
EOF

	run aube add -D is-odd
	assert_success

	# Should be in devDependencies now
	run cat package.json
	assert_output --partial '"devDependencies"'
	assert_output --partial '"is-odd"'
	# Should NOT remain in dependencies (dedup across sections)
	refute_output --partial '"dependencies"'
}

@test "aube add --save-peer writes only peerDependencies and does not install" {
	# `--save-peer` alone is a metadata-only declaration. pnpm treats
	# this as "consumers need X" and does not install it locally.
	cat >package.json <<'EOF'
{
  "name": "test-save-peer-only",
  "version": "0.0.0"
}
EOF

	run aube add --save-peer is-odd@^3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"peerDependencies"'
	assert_output --partial '"is-odd": "^3.0.0"'
	# No dev/regular section for a peer-only add.
	refute_output --partial '"dependencies"'
	refute_output --partial '"devDependencies"'

	# And no top-level node_modules entry — the peer isn't installed.
	run test -e node_modules/is-odd
	assert_failure
}

@test "aube add --save-peer --save-dev writes to both sections and installs" {
	# pnpm's conventional pairing: declare the peer for consumers AND
	# install it locally via devDependencies so tests/tooling work.
	cat >package.json <<'EOF'
{
  "name": "test-save-peer-dev",
  "version": "0.0.0"
}
EOF

	run aube add --save-peer -D is-odd@^3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"peerDependencies"'
	assert_output --partial '"devDependencies"'
	assert_output --partial '"is-odd": "^3.0.0"'

	# Both sections have the entry, and devDependencies drove the install.
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube remove strips peer entries too" {
	# A package added with --save-peer must be removable in one go.
	cat >package.json <<'EOF'
{
  "name": "test-remove-peer",
  "version": "0.0.0"
}
EOF

	run aube add --save-peer -D is-odd@^3.0.0
	assert_success

	run aube remove is-odd
	assert_success

	run cat package.json
	refute_output --partial '"peerDependencies"'
	refute_output --partial '"devDependencies"'
	refute_output --partial 'is-odd'
}

@test "aube add: refuses to add to workspace root without -W" {
	cat >package.json <<'EOF'
{
  "name": "ws-root",
  "version": "0.0.0",
  "private": true
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - 'packages/*'
EOF

	run aube add is-odd
	assert_failure
	assert_output --partial 'workspace root'

	# package.json untouched
	run cat package.json
	refute_output --partial 'is-odd'
	assert_file_not_exists aube-lock.yaml
}

@test "aube add: refuses to add to workspace root with aube-workspace.yaml" {
	cat >package.json <<'EOF'
{
  "name": "ws-root",
  "version": "0.0.0",
  "private": true
}
EOF
	cat >aube-workspace.yaml <<'EOF'
packages:
  - 'packages/*'
EOF

	run aube add is-odd
	assert_failure
	assert_output --partial 'workspace root'
}

@test "aube add: catalog-only workspace file does not trip the root check" {
	# A pnpm-workspace.yaml with only a catalog: section is not
	# actually a workspace root — no packages: list means no
	# sub-projects — so `add` must still work.
	cat >package.json <<'EOF'
{
  "name": "solo",
  "version": "0.0.0"
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
catalog:
  is-odd: ^3.0.1
EOF

	run aube add is-even@^1.0.0
	assert_success
}

@test "aube add -W: adds to workspace root when opted in" {
	cat >package.json <<'EOF'
{
  "name": "ws-root",
  "version": "0.0.0",
  "private": true
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - 'packages/*'
EOF

	run aube add -W is-odd
	assert_success

	run cat package.json
	assert_output --partial '"is-odd"'
	assert_file_exists aube-lock.yaml
}

@test "aube add --ignore-workspace-root-check: long form works too" {
	cat >package.json <<'EOF'
{
  "name": "ws-root",
  "version": "0.0.0",
  "private": true
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - 'packages/*'
EOF

	run aube add --ignore-workspace-root-check is-odd
	assert_success

	run cat package.json
	assert_output --partial '"is-odd"'
}

@test "aube add -w: adds to workspace root from a nested cwd" {
	cat >package.json <<'EOF'
{
  "name": "root",
  "version": "0.0.0",
  "private": true
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/app
	cat >packages/app/package.json <<'EOF'
{
  "name": "app",
  "version": "0.0.0"
}
EOF

	root="$PWD"
	cd packages/app
	run aube add -w is-odd@^3.0.0
	assert_success

	# Root manifest got the dep, not the nested one.
	run cat "$root/package.json"
	assert_output --partial '"is-odd": "^3.0.0"'

	run cat package.json
	refute_output --partial 'is-odd'
}

@test "aube add -w: errors outside a workspace" {
	cat >package.json <<'EOF'
{
  "name": "lonely",
  "version": "0.0.0"
}
EOF

	run aube add -w is-odd
	assert_failure
	assert_output --partial 'aube-workspace.yaml'
}

@test "aube add --no-save: links the package without modifying project state" {
	cat >package.json <<'EOF2'
{
  "name": "test-no-save",
  "version": "0.0.0",
  "dependencies": {}
}
EOF2
	# Snapshot the manifest bytes so we can assert restore is byte-exact.
	original="$(cat package.json)"

	run aube add --no-save is-odd
	assert_success
	assert_output --partial 'Restored package.json and lockfile (--no-save)'

	# Manifest restored byte-for-byte and no lockfile leaked into the
	# project — matches pnpm `--no-save` semantics.
	[ "$(cat package.json)" = "$original" ]
	run test -e aube-lock.yaml
	assert_failure

	# But the package itself is linked into node_modules.
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube add --no-save: restores a non-aube lockfile (package-lock.json)" {
	# Regression: --no-save used to assume aube-lock.yaml regardless of
	# the project's actual lockfile kind, which silently mutated
	# package-lock.json / pnpm-lock.yaml / yarn.lock projects.
	cat >package.json <<'EOF2'
{
  "name": "test-no-save-npm-lock",
  "version": "0.0.0",
  "dependencies": {}
}
EOF2
	# Minimal valid package-lock.json — empty graph is enough to make
	# `detect_existing_lockfile_kind` route through the npm writer, so
	# the snapshot/restore step targets package-lock.json instead of
	# silently writing aube-lock.yaml alongside.
	cat >package-lock.json <<'EOF2'
{
  "name": "test-no-save-npm-lock",
  "version": "0.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "test-no-save-npm-lock",
      "version": "0.0.0"
    }
  }
}
EOF2
	original_manifest="$(cat package.json)"
	original_lock="$(cat package-lock.json)"

	run aube add --no-save is-odd
	assert_success

	# Both files restored byte-for-byte; aube-lock.yaml never created.
	[ "$(cat package.json)" = "$original_manifest" ]
	[ "$(cat package-lock.json)" = "$original_lock" ]
	run test -e aube-lock.yaml
	assert_failure

	# Linking still happened.
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube add --no-save -g: errors out (incompatible)" {
	cat >package.json <<'EOF2'
{
  "name": "test-no-save-global",
  "version": "0.0.0"
}
EOF2

	run aube add --no-save -g is-odd
	assert_failure
	# clap-level conflict — surfaces the mutual exclusion in `--help`
	# and shell completions, not just at runtime.
	assert_output --partial "the argument '--no-save' cannot be used with '--global'"
}
