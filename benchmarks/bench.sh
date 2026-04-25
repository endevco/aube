#!/usr/bin/env bash
set -euo pipefail

# Benchmark script comparing aube, pnpm, yarn, npm, and bun install
# performance.
#
# Prerequisites:
#   - aube built in release mode: cargo build --release
#   - benchmark dependencies from mise (use `mise run bench` or
#     `mise run bench:bump`; missing package managers are skipped with
#     a warning rather than failing the whole run)
#
# Usage:
#   mise run bench
#
# Environment variables:
#   WARMUP       — warmup runs before timing (default: 1)
#   RUNS         — timed runs per benchmark (default: 10)
#   RESULTS_JSON — override the structured JSON output path
#
#   BENCH_HERMETIC=1 — route all registry traffic through a local
#                      Verdaccio instance pre-populated from npmjs. This
#                      is the default for mise tasks; leave it on so
#                      cold-cache numbers are not npmjs/CDN latency tests.
#                      First hermetic run warms the cache at
#                      ~/.cache/aube-bench/registry/; subsequent runs
#                      are fully offline. See benchmarks/hermetic.bash.
#   BENCH_BANDWIDTH  — optional throttle (e.g. `50mbit`, `6mbit`, bare
#                      integer bytes/s). Defaults to `500mbit` in mise
#                      tasks; routes traffic through a tiny token-bucket
#                      proxy in front of Verdaccio.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
AUBE_BIN="$REPO_DIR/target/release/aube"
PNPM_BIN="$(command -v pnpm || true)"
YARN_BIN="$(command -v yarn || true)"
NPM_BIN="$(command -v npm || true)"
BUN_BIN="$(command -v bun || true)"

BENCH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aube-bench.XXXXXX")"
WARMUP="${WARMUP:-1}"
RUNS="${RUNS:-10}"

# ── Validation ──────────────────────────────────────────────────────────────

if ! command -v hyperfine &>/dev/null; then
	echo "error: hyperfine is required. Run via: mise run bench" >&2
	exit 1
fi

if [ ! -f "$AUBE_BIN" ]; then
	echo "error: aube release binary not found at $AUBE_BIN" >&2
	echo "Run: cargo build --release" >&2
	exit 1
fi

# ── Optional hermetic registry ─────────────────────────────────────────────
# BENCH_HERMETIC=1 routes all registry traffic through a local
# Verdaccio instance (populated from npmjs on first run, offline after).
# BENCH_BANDWIDTH=<rate> puts a throttling proxy in front so cold-cache
# numbers reflect a simulated internet link rather than loopback disk
# speed. See benchmarks/hermetic.bash for the lifecycle details.

BENCH_REGISTRY_URL=""
if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/hermetic.bash"
	hermetic_start
	trap 'hermetic_stop' EXIT
fi

# ── Per-tool configuration ─────────────────────────────────────────────────
# Build up the list of tools to include dynamically so the matrix
# gracefully skips any pm that isn't installed. Each tool gets its
# own project dir, HOME, store, and cache so the scenarios are
# hermetic per-tool.

TOOLS=()
TOOL_BINS=()
TOOL_PROJECTS=()
TOOL_HOMES=()
TOOL_STORES=()
TOOL_CACHES=()

register_tool() {
	local name=$1 bin=$2
	if [ -z "$bin" ] || [ ! -x "$bin" ]; then
		echo "warning: $name not found on \$PATH — skipping" >&2
		return
	fi
	TOOLS+=("$name")
	TOOL_BINS+=("$bin")
	TOOL_PROJECTS+=("$BENCH_DIR/project-$name")
	TOOL_HOMES+=("$BENCH_DIR/home-$name")
	TOOL_STORES+=("$BENCH_DIR/store-$name")
	TOOL_CACHES+=("$BENCH_DIR/cache-$name")
}

# Order matters for the console output; keep aube first so the
# headline comparison is prominent and the rest follow alphabetically.
register_tool "aube" "$AUBE_BIN"
register_tool "bun" "$BUN_BIN"
register_tool "pnpm" "$PNPM_BIN"
register_tool "npm" "$NPM_BIN"
register_tool "yarn" "$YARN_BIN"

echo "workdir: $BENCH_DIR"
# Capture each tool's reported --version string so generate-results.js
# can fold it into results.json. Some tools print extra text around
# the semver (e.g. `aube 1.0.0-beta.3 (...)`, `bun 1.3.12+...`); the
# sed pulls out the first token that looks like a semver so the JSON
# stays clean without the consumers having to re-parse it.
versions_file="$BENCH_DIR/versions.tsv"
: >"$versions_file"
for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	bin="${TOOL_BINS[$i]}"
	raw="$($bin --version 2>/dev/null || echo 'unknown')"
	version="$(printf '%s\n' "$raw" | head -n1 | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?' | head -n1)"
	[ -z "$version" ] && version="$raw"
	printf "%s\t%s\n" "$tool" "$version" >>"$versions_file"
	printf "%-5s %s  (%s)\n" "$tool:" "$bin" "$version"
done
node_version="$(node --version 2>/dev/null | sed 's/^v//')"
if [ -n "$node_version" ]; then
	printf "%s\t%s\n" "node" "$node_version" >>"$versions_file"
	printf "%-5s %s\n" "node:" "$node_version"
fi
export BENCH_VERSIONS_FILE="$versions_file"
echo ""

# Per-tool lockfile filename (the name the pm writes into the project
# directory after `install`). Used to decide what to save after the
# populate step and where to copy it back for the "warm lockfile"
# scenarios.
lockfile_name_for() {
	case "$1" in
	aube) echo "aube-lock.yaml" ;;
	bun) echo "bun.lock" ;;
	npm) echo "package-lock.json" ;;
	pnpm) echo "pnpm-lock.yaml" ;;
	yarn) echo "yarn.lock" ;;
	*) echo "unknown" ;;
	esac
}

# ── Project setup ──────────────────────────────────────────────────────────

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	home="${TOOL_HOMES[$i]}"
	mkdir -p "$dir" "$home" "${TOOL_CACHES[$i]}"
	cp "$SCRIPT_DIR/fixture.package.json" "$dir/package.json"

	# pnpm reads storeDir / cacheDir from pnpm-workspace.yaml; the
	# other tools take them via CLI flags or env vars at command
	# time, so nothing to write on disk up front.
	if [ "$tool" = "pnpm" ]; then
		printf "storeDir: %s\ncacheDir: %s\n" "${TOOL_STORES[$i]}" "${TOOL_CACHES[$i]}" >"$dir/pnpm-workspace.yaml"
	fi

	# Hermetic mode: drop a .npmrc into both the project dir and the
	# isolated HOME so every PM resolves packages through the local
	# Verdaccio (or the throttle proxy in front of it) instead of
	# npmjs. Project-level .npmrc is honored by all five tools and
	# wins over HOME; HOME is a belt-and-suspenders fallback for any
	# command (like `aube add` after chdir) that might look there
	# first.
	if [ -n "$BENCH_REGISTRY_URL" ]; then
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$dir/.npmrc"
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$home/.npmrc"
	fi
done

# Keep a pristine copy of package.json
cp "$SCRIPT_DIR/fixture.package.json" "$BENCH_DIR/original-package.json"

# ── Populate stores and caches ─────────────────────────────────────────────
# One warm install per tool so the lockfile + cache + store are all
# populated before the scenario matrix runs. Everything is hermetic
# (isolated HOME / cache / store), so this is safe to run in parallel
# in the future but we keep it serial for clear console output.

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	bin="${TOOL_BINS[$i]}"
	home="${TOOL_HOMES[$i]}"
	store="${TOOL_STORES[$i]}"
	cache="${TOOL_CACHES[$i]}"
	lockfile_name=$(lockfile_name_for "$tool")
	echo "Populating store and cache for $tool..."
	# Wipe every known lockfile so an earlier failed run doesn't
	# leave a stale one behind that would fool the pm into a
	# different code path.
	rm -rf "$dir/node_modules" \
		"$dir/pnpm-lock.yaml" \
		"$dir/aube-lock.yaml" \
		"$dir/package-lock.json" \
		"$dir/yarn.lock" \
		"$dir/bun.lock" \
		"$dir/bun.lockb"

	case "$tool" in
	aube)
		cd "$dir" && HOME="$home" XDG_CACHE_HOME="$cache" XDG_DATA_HOME="$home/.local/share" "$bin" install
		;;
	npm)
		# `--legacy-peer-deps` is the only way npm tolerates the
		# fixture's mixed peer-dep ranges (eslint 9 vs 8, etc.).
		# pnpm/aube handle this via `autoInstallPeers=true` by
		# default; using npm's strict mode here would just make
		# the populate step fail before we even reach the
		# scenarios. Yes, this is the classic "npm is stricter"
		# caveat you read in every benchmark footnote.
		cd "$dir" && HOME="$home" npm_config_cache="$cache" "$bin" install \
			--ignore-scripts --no-audit --no-fund --legacy-peer-deps
		;;
	pnpm)
		cd "$dir" && HOME="$home" "$bin" install --ignore-scripts --no-frozen-lockfile
		;;
	yarn)
		cd "$dir" && HOME="$home" YARN_CACHE_FOLDER="$cache" "$bin" install \
			--ignore-scripts --ignore-engines --no-progress
		;;
	bun)
		# Bun takes `--cache-dir` as a CLI flag and `BUN_INSTALL` as
		# the global install prefix. Point both at the hermetic temp
		# to keep it from touching `~/.bun`.
		cd "$dir" && HOME="$home" BUN_INSTALL="$home/.bun" "$bin" install \
			--cache-dir "$cache" --ignore-scripts --no-summary --force
		;;
	esac

	if [ ! -f "$dir/$lockfile_name" ]; then
		echo "error: $lockfile_name was not created for $tool in $dir" >&2
		exit 1
	fi
	cp "$dir/$lockfile_name" "$BENCH_DIR/saved-lockfile-$tool"
done

# ── Helper ─────────────────────────────────────────────────────────────────
#
# Each bench scenario is driven by:
#
#   - one shared `prepare_tpl` that sets the on-disk state for the
#     tool's project dir (wiping `node_modules`, dropping back the
#     saved lockfile, etc.)
#   - a per-tool command template looked up in the `CMDS_<scenario>`
#     associative array
#
# Template placeholders:
#   {project}       — project directory
#   {bin}           — tool binary
#   {home}          — isolated HOME directory
#   {store}         — store directory (pnpm/aube)
#   {cache}         — cache directory
#   {lockfile}      — saved lockfile path (source of the copy)
#   {lockfile_dest} — per-tool lockfile destination in the project
#                     directory (matches the pm's native filename)

expand_template() {
	local tpl=$1 project=$2 bin=$3 home=$4 store=$5 cache=$6 lockfile=$7 lockfile_dest=$8
	tpl="${tpl//\{project\}/$project}"
	tpl="${tpl//\{bin\}/$bin}"
	tpl="${tpl//\{home\}/$home}"
	tpl="${tpl//\{store\}/$store}"
	tpl="${tpl//\{cache\}/$cache}"
	tpl="${tpl//\{lockfile\}/$lockfile}"
	tpl="${tpl//\{lockfile_dest\}/$lockfile_dest}"
	echo "$tpl"
}

# Per-tool install invocations, keyed by `<scenario>:<tool>`.
declare -A CMDS

# Per-tool boilerplate factored out of the `CMDS` declarations below.
# Every bun invocation threads the same hermetic environment
# (isolated `HOME`, `BUN_INSTALL`, `--cache-dir`, `--ignore-scripts`,
# `--no-summary`) so the scenarios only have to spell out the
# install-mode flags that actually vary per scenario.
BUN_BASE="HOME={home} BUN_INSTALL={home}/.bun {bin} install --cache-dir {cache} --ignore-scripts --no-summary"

# aube reads the global store root from `$XDG_DATA_HOME/aube/store`
# (falling back to `$HOME/.local/share/aube/store`). We must pin
# `XDG_DATA_HOME` alongside `HOME` and `XDG_CACHE_HOME` — otherwise
# a host that already has `XDG_DATA_HOME` set in its environment
# would leak the benchmark's store out of the isolated `{home}`,
# and `COLD_WIPE` wouldn't find it to clean up between iterations.
AUBE_ENV="HOME={home} XDG_CACHE_HOME={cache} XDG_DATA_HOME={home}/.local/share"

# Per-scenario AUBE_ENV variants that pin the global virtual store mode
# via the `enableGlobalVirtualStore` setting's auto-synthesized env-var
# alias (`npm_config_<snake_case>` — see `aube-settings/build.rs`).
# Using an env var rather than `--enable-gvs` / `--disable-gvs` means
# scenarios that go through `aube test` and `aube add` (which trigger
# auto-install internally) get the same forcing as direct `aube install`
# calls. The setting wins over `Linker::new`'s `CI` heuristic, so
# GitHub Actions' inherited `CI=true` cannot silently flip the mode.
AUBE_ENV_GVS_ON="$AUBE_ENV npm_config_enable_global_virtual_store=true"
AUBE_ENV_GVS_OFF="$AUBE_ENV npm_config_enable_global_virtual_store=false"

# Scenario keys describe what's on disk before the run. Every install
# scenario assumes a committed lockfile is present; the axes are
# cache/store warmth and whether aube's global virtual store is enabled.
# Plus an "add" scenario that exercises the incremental add path and an
# "install-test" scenario that measures install + script dispatch end-to-end.

# Scenario 1: Fresh install, warm cache (aube GVS enabled) ------------------
CMDS["gvs-warm:aube"]="cd {project} && $AUBE_ENV_GVS_ON {bin} install --frozen-lockfile >/dev/null 2>&1"
CMDS["gvs-warm:bun"]="cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1"
CMDS["gvs-warm:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps --prefer-offline >/dev/null 2>&1"
CMDS["gvs-warm:pnpm"]="cd {project} && HOME={home} {bin} install --frozen-lockfile --ignore-scripts >/dev/null 2>&1"
CMDS["gvs-warm:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} install --frozen-lockfile --ignore-scripts --ignore-engines --no-progress --prefer-offline >/dev/null 2>&1"

# Scenario 2: Fresh install, cold cache (aube GVS enabled) ------------------
CMDS["gvs-cold:aube"]="cd {project} && $AUBE_ENV_GVS_ON {bin} install --frozen-lockfile >/dev/null 2>&1"
CMDS["gvs-cold:bun"]="cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1"
CMDS["gvs-cold:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps >/dev/null 2>&1"
CMDS["gvs-cold:pnpm"]="cd {project} && HOME={home} {bin} install --frozen-lockfile --ignore-scripts >/dev/null 2>&1"
CMDS["gvs-cold:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} install --frozen-lockfile --ignore-scripts --ignore-engines --no-progress >/dev/null 2>&1"

# Scenario 3: CI install, warm cache (aube GVS disabled) --------------------
CMDS["ci-warm:aube"]="cd {project} && $AUBE_ENV_GVS_OFF {bin} install --frozen-lockfile >/dev/null 2>&1"
CMDS["ci-warm:bun"]="cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1"
CMDS["ci-warm:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps --prefer-offline >/dev/null 2>&1"
CMDS["ci-warm:pnpm"]="cd {project} && HOME={home} {bin} install --frozen-lockfile --ignore-scripts >/dev/null 2>&1"
CMDS["ci-warm:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} install --frozen-lockfile --ignore-scripts --ignore-engines --no-progress --prefer-offline >/dev/null 2>&1"

# Scenario 4: CI install, cold cache (aube GVS disabled) --------------------
CMDS["ci-cold:aube"]="cd {project} && $AUBE_ENV_GVS_OFF {bin} install --frozen-lockfile >/dev/null 2>&1"
CMDS["ci-cold:bun"]="cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1"
CMDS["ci-cold:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps >/dev/null 2>&1"
CMDS["ci-cold:pnpm"]="cd {project} && HOME={home} {bin} install --frozen-lockfile --ignore-scripts >/dev/null 2>&1"
CMDS["ci-cold:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} install --frozen-lockfile --ignore-scripts --ignore-engines --no-progress >/dev/null 2>&1"

# Scenario 5: install + run test (warm store+cache, wiped node_modules) -----
# Each tool does "install then run the `test` script" via its own idiomatic
# command. aube has no install-test alias because `aube test` auto-installs
# before running the script; pnpm and npm ship `install-test`; bun and yarn
# need an explicit chain. The fixture's `test` script is the POSIX shell
# no-op `:`, so this measures install + script dispatch, not test-runtime work.
CMDS["install-test:aube"]="cd {project} && $AUBE_ENV_GVS_ON {bin} test >/dev/null 2>&1"
CMDS["install-test:bun"]="cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1 && HOME={home} BUN_INSTALL={home}/.bun {bin} run test >/dev/null 2>&1"
# `npm install-test` (the `install` variant, not `ci`) is the right
# command for the "already installed" semantics this scenario uses —
# `npm install` skips work when node_modules matches package-lock,
# whereas `npm ci` deletes node_modules every run by design.
CMDS["install-test:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} install-test --ignore-scripts --no-audit --no-fund --legacy-peer-deps --prefer-offline >/dev/null 2>&1"
CMDS["install-test:pnpm"]="cd {project} && HOME={home} {bin} install-test --frozen-lockfile --ignore-scripts >/dev/null 2>&1"
CMDS["install-test:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} install --frozen-lockfile --ignore-scripts --ignore-engines --no-progress --prefer-offline >/dev/null 2>&1 && HOME={home} YARN_CACHE_FOLDER={cache} {bin} test >/dev/null 2>&1"

# Scenario 6: add a dependency (warm store+cache, existing lockfile) --------
CMDS["add:aube"]="cd {project} && $AUBE_ENV_GVS_ON {bin} add is-odd >/dev/null 2>&1"
CMDS["add:bun"]="cd {project} && HOME={home} BUN_INSTALL={home}/.bun {bin} add is-odd --cache-dir {cache} --ignore-scripts --no-summary >/dev/null 2>&1"
CMDS["add:npm"]="cd {project} && HOME={home} npm_config_cache={cache} {bin} install --ignore-scripts --no-audit --no-fund --legacy-peer-deps is-odd >/dev/null 2>&1"
CMDS["add:pnpm"]="cd {project} && HOME={home} {bin} add is-odd --ignore-scripts >/dev/null 2>&1"
CMDS["add:yarn"]="cd {project} && HOME={home} YARN_CACHE_FOLDER={cache} {bin} add is-odd --ignore-scripts --ignore-engines --no-progress >/dev/null 2>&1"

run_bench() {
	local bench_name=$1
	local prepare_tpl=$2

	for i in "${!TOOLS[@]}"; do
		local tool="${TOOLS[$i]}"
		local project="${TOOL_PROJECTS[$i]}"
		local bin="${TOOL_BINS[$i]}"
		local home="${TOOL_HOMES[$i]}"
		local store="${TOOL_STORES[$i]}"
		local cache="${TOOL_CACHES[$i]}"
		local lockfile="$BENCH_DIR/saved-lockfile-$tool"
		local lockfile_dest
		lockfile_dest="$project/$(lockfile_name_for "$tool")"

		local cmd_tpl=${CMDS["$bench_name:$tool"]:-}
		if [ -z "$cmd_tpl" ]; then
			echo "warning: no $bench_name command for $tool — skipping" >&2
			continue
		fi

		local prepare
		prepare=$(expand_template "$prepare_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local cmd
		cmd=$(expand_template "$cmd_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		echo ""
		echo "  $tool:"
		hyperfine \
			--warmup "$WARMUP" \
			--runs "$RUNS" \
			--ignore-failure \
			--prepare "$prepare" \
			--command-name "$tool" \
			"$cmd" \
			--export-json "$BENCH_DIR/${bench_name}-${tool}.json" ||
			true
	done
}

# Like `run_bench`, but times the *second* invocation of the tool's
# command — the prepare step wipes node_modules, restores the saved
# lockfile, and runs the same command once so the timed iteration
# starts from a "node_modules is already valid" state.
#
# Used by the install-test scenario to measure the "I've installed,
# now I just want to re-run my tests" developer loop rather than the
# "fresh checkout + install" loop (which `gvs-warm` and `ci-warm`
# already cover).
run_bench_preinstall() {
	local bench_name=$1

	for i in "${!TOOLS[@]}"; do
		local tool="${TOOLS[$i]}"
		local project="${TOOL_PROJECTS[$i]}"
		local bin="${TOOL_BINS[$i]}"
		local home="${TOOL_HOMES[$i]}"
		local store="${TOOL_STORES[$i]}"
		local cache="${TOOL_CACHES[$i]}"
		local lockfile="$BENCH_DIR/saved-lockfile-$tool"
		local lockfile_dest
		lockfile_dest="$project/$(lockfile_name_for "$tool")"

		local cmd_tpl=${CMDS["$bench_name:$tool"]:-}
		if [ -z "$cmd_tpl" ]; then
			echo "warning: no $bench_name command for $tool — skipping" >&2
			continue
		fi

		local cmd
		cmd=$(expand_template "$cmd_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local warm_prep
		warm_prep=$(expand_template "$WARM_PREP" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		# Prepare: wipe + restore lockfile, then run the same command
		# once untimed so the tool's install phase populates
		# node_modules (and `.aube-state` for aube). The timed
		# iteration then re-runs the command against the settled
		# state — the developer-loop "run my tests again" case.
		local prepare="$warm_prep && $cmd"

		echo ""
		echo "  $tool:"
		hyperfine \
			--warmup "$WARMUP" \
			--runs "$RUNS" \
			--ignore-failure \
			--prepare "$prepare" \
			--command-name "$tool" \
			"$cmd" \
			--export-json "$BENCH_DIR/${bench_name}-${tool}.json" ||
			true
	done
}

# Directories to wipe in cold scenarios. Each pm has its own cache /
# store layout, so we reset everything we know about to guarantee
# a fresh download on every iteration.
COLD_WIPE='{store} {cache} {home}/.pnpm-store {home}/.local/share/aube {home}/.npm {home}/.yarn {home}/.bun {home}/.cache/aube {home}/.cache/yarn {home}/.cache/bun'

# Warm-cache lockfile restore: wipe the project-local state (lockfile
# + node_modules) and drop the saved lockfile back. Uses the per-tool
# `lockfile_dest` placeholder so each pm gets its native filename.
WARM_PREP="rm -rf {project}/node_modules {project}/pnpm-lock.yaml {project}/aube-lock.yaml {project}/package-lock.json {project}/yarn.lock {project}/bun.lock {project}/bun.lockb && cp {lockfile} {lockfile_dest}"

# ── Benchmark 1: Fresh install, warm cache, GVS enabled ────────────────────
# Lockfile present, node_modules deleted, store and cache warm.
# Forces aube's global virtual store on so GitHub Actions' inherited
# CI=true environment cannot silently turn this into per-project mode.

echo ""
echo "━━━ Benchmark 1: Fresh install (warm cache, GVS) ━━━"
run_bench "gvs-warm" "$WARM_PREP"

# ── Benchmark 2: Fresh install, cold cache, GVS enabled ────────────────────
# Lockfile present, but store and cache are empty.
# Measures fetch-from-registry + import + GVS link path.

echo ""
echo "━━━ Benchmark 2: Fresh install (cold cache, GVS) ━━━"
run_bench "gvs-cold" \
	"rm -rf {project}/node_modules {project}/pnpm-lock.yaml {project}/aube-lock.yaml {project}/package-lock.json {project}/yarn.lock {project}/bun.lock {project}/bun.lockb $COLD_WIPE && mkdir -p {home} && cp {lockfile} {lockfile_dest}"

# ── Benchmark 3: CI install, warm cache ────────────────────────────────────
# Lockfile present, node_modules deleted, store and cache warm.
# Forces aube's global virtual store off to model real CI defaults without
# relying on runner-provided environment variables.

echo ""
echo "━━━ Benchmark 3: CI install (warm cache, GVS disabled) ━━━"
run_bench "ci-warm" "$WARM_PREP"

# ── Benchmark 4: CI install, cold cache ────────────────────────────────────
# Lockfile present, but store and cache are empty.
# Forces aube's global virtual store off to model real CI defaults without
# relying on runner-provided environment variables.

echo ""
echo "━━━ Benchmark 4: CI install (cold cache, GVS disabled) ━━━"
run_bench "ci-cold" \
	"rm -rf {project}/node_modules {project}/pnpm-lock.yaml {project}/aube-lock.yaml {project}/package-lock.json {project}/yarn.lock {project}/bun.lock {project}/bun.lockb $COLD_WIPE && mkdir -p {home} && cp {lockfile} {lockfile_dest}"

# ── Benchmark 5: install + run test (developer loop) ───────────────────────
# Warm store+cache, lockfile present, node_modules *already* populated.
# Models the developer-loop case: "I've installed, now I keep re-running
# my tests." Each iteration's prepare runs the full install-test command
# once (untimed) so node_modules and any tool-specific state files are
# valid, then the timed iteration re-runs the same command. Tools with a
# state-based short-circuit (aube's .aube-state) skip install entirely
# on the timed run; tools without one still pay for lockfile revalidation.

echo ""
echo "━━━ Benchmark 5: install + run test (already installed) ━━━"
run_bench_preinstall "install-test"

# ── Benchmark 6: Add dependency ────────────────────────────────────────────
# Lockfile present, add a new dependency to trigger re-resolution.
# Store and cache warm. Exercises the incremental resolution path.
# Kept last because the other scenarios are install-shaped and this
# one is edit-shaped — it doesn't belong in the "install at various
# warmth levels" progression above.

echo ""
echo "━━━ Benchmark 6: Add dependency ━━━"
run_bench "add" \
	"$WARM_PREP && cp $BENCH_DIR/original-package.json {project}/package.json"

# ── Summary ────────────────────────────────────────────────────────────────

RESULTS_MD="$BENCH_DIR/results.md"

echo ""
echo "━━━ Results ━━━"
TOOLS_CSV=$(
	IFS=,
	echo "${TOOLS[*]}"
)
BENCH_TOOLS="$TOOLS_CSV" node "$SCRIPT_DIR/generate-results.js" "$BENCH_DIR" "$RESULTS_MD"
echo ""
echo "Results saved to: $RESULTS_MD"
echo ""
echo "Temp directory kept at: $BENCH_DIR"
echo "Remove with: rm -rf $BENCH_DIR"
