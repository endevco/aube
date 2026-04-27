#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "config set writes key=value to user .npmrc" {
	run aube config set autoInstallPeers false
	assert_success
	assert [ -f "$HOME/.npmrc" ]
	run cat "$HOME/.npmrc"
	assert_output --partial "autoInstallPeers=false"
}

@test "config get reads value from user .npmrc" {
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "config get resolves canonical name to .npmrc alias" {
	# Value written under the kebab-case alias; canonical lookup should
	# still find it because settings.toml declares both names.
	echo "auto-install-peers=true" >"$HOME/.npmrc"
	run aube config get autoInstallPeers
	assert_success
	assert_output "true"
}

@test "config get --location project only reads project .npmrc" {
	mkdir proj
	echo "autoInstallPeers=true" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers --location project
	assert_success
	assert_output "false"
}

@test "config get --location user ignores project .npmrc" {
	mkdir proj
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers --location user
	assert_success
	assert_output "undefined"
}

@test "config list collapses cross-alias duplicates to match get" {
	# User file writes the setting under the camelCase canonical name;
	# project file writes it under the kebab-case alias. `get` resolves
	# aliases and returns the project value; `list` must agree and show
	# exactly one row under the primary alias with that same value —
	# otherwise `list` and `get` could disagree on identical input.
	mkdir proj
	echo "autoInstallPeers=true" >"$HOME/.npmrc"
	echo "auto-install-peers=false" >proj/.npmrc
	cd proj
	run aube config get autoInstallPeers
	assert_success
	assert_output "false"
	run aube config list
	assert_success
	assert_line "auto-install-peers=false"
	refute_line "autoInstallPeers=true"
	refute_line "auto-install-peers=true"
}

@test "config list --all rejects non-merged location" {
	run aube config list --all --location project
	assert_failure
	assert_output --partial "--all is only supported with --location merged"
}

@test "config get prints undefined for missing key" {
	run aube config get autoInstallPeers
	assert_success
	assert_output "undefined"
}

@test "config set collapses aliases so a prior spelling doesn't linger" {
	echo "auto-install-peers=false" >"$HOME/.npmrc"
	run aube config set autoInstallPeers true
	assert_success
	run cat "$HOME/.npmrc"
	refute_output --partial "auto-install-peers=false"
	assert_output --partial "autoInstallPeers=true"
}

@test "config delete removes a key" {
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube config delete autoInstallPeers
	assert_success
	run cat "$HOME/.npmrc"
	refute_output --partial "autoInstallPeers"
}

@test "config delete errors when key is not set" {
	echo "registry=https://r.example.com/" >"$HOME/.npmrc"
	run aube config delete autoInstallPeers
	assert_failure
}

@test "config list prints merged entries" {
	# Project dir must be separate from HOME so user vs project .npmrc
	# don't alias to the same file.
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config list
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	# `autoInstallPeers` canonicalizes to `auto-install-peers` in list
	# output so cross-alias duplicates collapse into one row.
	assert_output --partial "auto-install-peers=false"
}

@test "config with no subcommand lists merged entries" {
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config
	assert_success
	assert_output --partial "registry=https://user.example.com/"
	assert_output --partial "auto-install-peers=false"
}

@test "config with parent --all lists defaults" {
	run aube config --all
	assert_success
	assert_output --partial "auto-install-peers=true (default)"
}

@test "config list --location project only reads project .npmrc" {
	mkdir proj
	echo "registry=https://user.example.com/" >"$HOME/.npmrc"
	echo "autoInstallPeers=false" >proj/.npmrc
	cd proj
	run aube config list --location project
	assert_success
	refute_output --partial "user.example.com"
	assert_output --partial "auto-install-peers=false"
}

@test "config set --location project writes to ./.npmrc" {
	run aube config set autoInstallPeers false --location project
	assert_success
	assert [ -f "./.npmrc" ]
	run cat "./.npmrc"
	assert_output --partial "autoInstallPeers=false"
}

@test "config preserves existing unrelated entries when setting a key" {
	echo "registry=https://r.example.com/" >"$HOME/.npmrc"
	run aube config set autoInstallPeers false
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "registry=https://r.example.com/"
	assert_output --partial "autoInstallPeers=false"
}

@test "config get returns literal \${VAR} references, not substituted values" {
	# Users inspecting their .npmrc should see exactly what's on disk.
	# Resolving ${NPM_TOKEN} here would both surprise users and risk
	# leaking secrets into shell history or logs. The single quotes
	# below are intentional: we want the literal `${...}` text written
	# to the file, not the expansion.
	export AUBE_TEST_TOKEN=super-secret
	# shellcheck disable=SC2016
	echo '//registry.example.com/:_authToken=${AUBE_TEST_TOKEN}' >"$HOME/.npmrc"
	run aube config get "//registry.example.com/:_authToken"
	assert_success
	# shellcheck disable=SC2016
	assert_output '${AUBE_TEST_TOKEN}'
	# Same answer via --location user.
	run aube config get "//registry.example.com/:_authToken" --location user
	assert_success
	# shellcheck disable=SC2016
	assert_output '${AUBE_TEST_TOKEN}'
	unset AUBE_TEST_TOKEN
}

@test "config accepts unknown (literal) keys for auth-style writes" {
	# Auth token keys like `//registry/:_authToken` are not registered
	# in settings.toml. The command should still write them verbatim.
	run aube config set "//registry.example.com/:_authToken" secret123
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "//registry.example.com/:_authToken=secret123"
}

@test "config set @scope:registry does not clobber the user's registry entry" {
	# `registries.npmrc_keys` documents `@scope:registry` and
	# `//host/:_authToken` as pattern templates alongside the literal
	# `registry` key. The alias resolver must NOT treat those templates
	# as siblings of `registry`, otherwise `config set @scope:registry …`
	# would resolve to the registries group and the stale-alias removal
	# pass would silently delete the user's existing `registry` line.
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config set @mycorp:registry https://npm.mycorp.internal/
	assert_success
	run aube config get registry
	assert_success
	assert_output "https://registry.example.com/"
	run aube config get @mycorp:registry
	assert_success
	assert_output "https://npm.mycorp.internal/"
}

@test "config get --json emits the value as a JSON string" {
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config get --json registry
	assert_success
	assert_output '"https://registry.example.com/"'
}

@test "config get --json prints undefined for a missing key" {
	run aube config get --json nonexistent-key
	assert_success
	assert_output "undefined"
}

@test "config list --json emits a JSON object" {
	run aube config set registry https://registry.example.com/
	assert_success
	run aube config set auto-install-peers true
	assert_success
	run bash -c "aube config list --json | jq -r '.registry'"
	assert_success
	assert_output "https://registry.example.com/"
	run bash -c 'aube config list --json | jq -r ".[\"auto-install-peers\"]"'
	assert_success
	assert_output "true"
}

@test "config list --all --json emits defaults as plain values (no annotation)" {
	# Nothing is set — every row in the output is a default, and the JSON
	# value must not contain the " (default)" suffix the text view uses.
	run bash -c 'aube config list --all --json | jq -r ".[\"auto-install-peers\"]"'
	assert_success
	# Plain string, not "true (default)".
	refute_output --partial "(default)"

	# The parallel text view should still annotate defaults, so the two
	# outputs stay distinguishable for humans vs. machines.
	run aube config list --all
	assert_success
	assert_output --partial "(default)"
}

@test "config find searches the generated settings reference" {
	run aube config find min package install time
	assert_success
	assert_line --partial "minimumReleaseAge (minimumReleaseAge) - Delay installation of newly published versions (minutes)."
}

@test "config explain prints sources for a known setting" {
	run aube config explain minimum-release-age
	assert_success
	assert_line "minimumReleaseAge"
	assert_line "  Default: 1440"
	assert_line "  Environment: npm_config_minimum_release_age, NPM_CONFIG_MINIMUM_RELEASE_AGE, AUBE_MINIMUM_RELEASE_AGE"
	assert_line "  .npmrc keys: minimumReleaseAge, minimum-release-age"
	assert_line "  Workspace YAML keys: minimumReleaseAge"
	assert_output --partial "Set to \`0\` to disable."
}

@test "config tui rejects non-interactive stdout" {
	run aube config tui
	assert_failure
	assert_output --partial "requires an interactive terminal"
}

# ── top-level get / set aliases ──────────────────────────────────────

@test "get delegates to config get" {
	echo "autoInstallPeers=false" >"$HOME/.npmrc"
	run aube get autoInstallPeers
	assert_success
	assert_output "false"
}

@test "set delegates to config set" {
	run aube set autoInstallPeers false
	assert_success
	run cat "$HOME/.npmrc"
	assert_output --partial "autoInstallPeers=false"
}
