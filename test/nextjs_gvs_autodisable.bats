#!/usr/bin/env bats

# Auto-disable of the global virtual store when Next.js is present in
# the dep graph. Without this guard, `next dev` / `next build` fail
# with "Symlink node_modules/<pkg> is invalid, it points out of the
# filesystem root" because Turbopack canonicalizes every
# `node_modules/` symlink and aube's gvs layout makes `.aube/<pkg>`
# an absolute symlink into `~/.cache/aube/virtual-store/`.
#
# These tests use a `link:` local dep named `next` so the detection
# fires without needing a real Next.js tarball from the registry —
# detection only reads `dependencies` / `devDependencies` /
# `optionalDependencies` keys, not the version specifier.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_fake_next() {
	# Minimal local package named `next` so the detection's name-based
	# match fires. Version is irrelevant — detection ignores the
	# specifier.
	mkdir -p fake-next
	cat >fake-next/package.json <<'JSON'
{"name":"next","version":"0.0.0-fake","main":"index.js"}
JSON
	cat >fake-next/index.js <<'JS'
module.exports = "fake-next for bats";
JS
}

@test "aube install warns and disables global virtual store when next is in dependencies" {
	_make_fake_next
	mkdir -p app
	cd app
	# Pair `next` (fake, local) with a real registry dep so the
	# `.aube/<pkg>` layout assertion below has something to inspect —
	# link: deps skip `.aube/` entirely.
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"next":"link:../fake-next","is-odd":"3.0.1"}}
JSON

	run aube install
	assert_success
	assert_output --partial "Next.js detected"
	assert_output --partial "disabling global virtual store"

	# The whole point of the auto-disable: `.aube/<pkg>` must be a
	# real directory, not a symlink into
	# `~/.cache/aube/virtual-store/`. A symlink here is what trips
	# Turbopack's filesystem-root check.
	[ -d node_modules/.aube/is-odd@3.0.1 ]
	[ ! -L node_modules/.aube/is-odd@3.0.1 ]
	[ -L node_modules/next ]
}

@test "aube install warns when next is in devDependencies" {
	_make_fake_next
	mkdir -p app
	cd app
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","devDependencies":{"next":"link:../fake-next"}}
JSON

	run aube install
	assert_success
	assert_output --partial "Next.js detected"
}

@test "aube install does not warn when next is absent" {
	_setup_basic_fixture

	run aube install
	assert_success
	refute_output --partial "Next.js detected"
	refute_output --partial "disabling global virtual store"
}

@test "autoDisableGlobalVirtualStoreForNextjs=false opts out of the auto-disable" {
	_make_fake_next
	mkdir -p app
	cd app
	cat >.npmrc <<'RC'
autoDisableGlobalVirtualStoreForNextjs=false
RC
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"next":"link:../fake-next","is-odd":"3.0.1"}}
JSON

	run aube install
	assert_success
	refute_output --partial "Next.js detected"
	refute_output --partial "disabling global virtual store"

	# With the opt-out, gvs stays on — `.aube/<pkg>` should be a
	# symlink into `~/.cache/aube/virtual-store/`. This is the
	# inverse of the default-behavior test above and confirms the
	# setting actually reaches the linker.
	[ -L node_modules/.aube/is-odd@3.0.1 ]
}

@test "CI=1 suppresses the Next.js warning because gvs is already off" {
	# Under CI, Linker::new already picks per-project materialization,
	# so the warning would be noise. Detection is still correct —
	# this test just pins the "no double-warn in CI" contract.
	_make_fake_next
	mkdir -p app
	cd app
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"next":"link:../fake-next"}}
JSON

	CI=1 run aube install
	assert_success
	refute_output --partial "Next.js detected"
}
