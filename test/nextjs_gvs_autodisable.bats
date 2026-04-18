#!/usr/bin/env bats

# Auto-disable of the global virtual store when any package listed in
# `disableGlobalVirtualStoreForPackages` is present in an importer's
# deps. Default list is `["next"]` — Next.js's Turbopack canonicalizes
# every `node_modules/<pkg>` symlink and rejects targets outside the
# project root, which aube's gvs layout produces by default. The
# setting is the extension point: add any tool with the same
# restriction, or set to `[]` to disable the heuristic.
#
# These tests use a `link:` local dep so detection fires without
# needing a real tarball — the scan only reads dependency names, not
# versions.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_fake_dep() {
	# $1 = name on disk (directory) and package name
	local name="$1"
	mkdir -p "fake-$name"
	cat >"fake-$name/package.json" <<JSON
{"name":"$name","version":"0.0.0-fake","main":"index.js"}
JSON
	cat >"fake-$name/index.js" <<JS
module.exports = "fake-$name for bats";
JS
}

@test "aube install warns and disables global virtual store when next is in dependencies" {
	_make_fake_dep next
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
	assert_output --partial "disableGlobalVirtualStoreForPackages"
	assert_output --partial "\`next\`"

	# The whole point of the auto-disable: `.aube/<pkg>` must be a
	# real directory, not a symlink into
	# `~/.cache/aube/virtual-store/`. A symlink here is what trips
	# Turbopack's filesystem-root check.
	[ -d node_modules/.aube/is-odd@3.0.1 ]
	[ ! -L node_modules/.aube/is-odd@3.0.1 ]
	[ -L node_modules/next ]
}

@test "aube install warns when next is in devDependencies" {
	_make_fake_dep next
	mkdir -p app
	cd app
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","devDependencies":{"next":"link:../fake-next"}}
JSON

	run aube install
	assert_success
	assert_output --partial "disableGlobalVirtualStoreForPackages"
	assert_output --partial "\`next\`"
}

@test "aube install does not warn when no listed package is present" {
	_setup_basic_fixture

	run aube install
	assert_success
	refute_output --partial "disableGlobalVirtualStoreForPackages"
	refute_output --partial "disabling global virtual store"
}

@test "disableGlobalVirtualStoreForPackages=[] opts out of the auto-disable" {
	_make_fake_dep next
	mkdir -p app
	cd app
	cat >.npmrc <<'RC'
disableGlobalVirtualStoreForPackages=[]
RC
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"next":"link:../fake-next","is-odd":"3.0.1"}}
JSON

	run aube install
	assert_success
	refute_output --partial "disableGlobalVirtualStoreForPackages"

	# With the opt-out, gvs stays on — `.aube/<pkg>` should be a
	# symlink into `~/.cache/aube/virtual-store/`. This is the
	# inverse of the default-behavior test above and confirms the
	# setting actually reaches the linker.
	[ -L node_modules/.aube/is-odd@3.0.1 ]
}

@test "CI=1 suppresses the gvs-disable warning because gvs is already off" {
	# Under CI, Linker::new already picks per-project materialization,
	# so the warning would be noise. Detection is still correct —
	# this test just pins the "no double-warn in CI" contract.
	_make_fake_dep next
	mkdir -p app
	cd app
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"next":"link:../fake-next"}}
JSON

	CI=1 run aube install
	assert_success
	refute_output --partial "disableGlobalVirtualStoreForPackages"
}

@test "custom entry in disableGlobalVirtualStoreForPackages triggers the disable" {
	# The whole point of making this a list: users can add packages
	# they discover have the same filesystem-root problem. Use a
	# made-up name to prove the heuristic doesn't hardcode `next`.
	_make_fake_dep turbo-clone
	mkdir -p app
	cd app
	cat >.npmrc <<'RC'
disableGlobalVirtualStoreForPackages=[next,turbo-clone]
RC
	cat >package.json <<'JSON'
{"name":"app","version":"0.0.0","dependencies":{"turbo-clone":"link:../fake-turbo-clone","is-odd":"3.0.1"}}
JSON

	run aube install
	assert_success
	assert_output --partial "disableGlobalVirtualStoreForPackages"
	assert_output --partial "\`turbo-clone\`"
	[ ! -L node_modules/.aube/is-odd@3.0.1 ]
}
