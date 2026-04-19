#!/usr/bin/env bash
# Hermetic benchmark registry lifecycle.
#
# Sourced by benchmarks/bench.sh when BENCH_HERMETIC=1. Exposes:
#
#   hermetic_start    — ensures the registry cache is warm, starts
#                       Verdaccio (no uplink), optionally starts a
#                       throttling proxy in front, and exports
#                       BENCH_REGISTRY_URL.
#   hermetic_stop     — tears both processes down. Idempotent; safe to
#                       call from an EXIT trap.
#
# Configuration (all env-driven):
#
#   BENCH_HERMETIC_CACHE  — persistent cache dir (default:
#                           ~/.cache/aube-bench/registry). Holds the
#                           Verdaccio storage and a `.warmed` sentinel.
#                           Wipe it to force a re-warm from npmjs.
#   BENCH_VERDACCIO_PORT  — localhost port for Verdaccio
#                           (default: 4874; distinct from test/registry's
#                           4873 so the two can coexist).
#   BENCH_BANDWIDTH       — optional throttle, e.g. `50mbit`, `6mbit`,
#                           `6250000` (bytes/s as a bare integer).
#                           When set, traffic goes through
#                           throttle-proxy.mjs instead of direct.
#   BENCH_PROXY_PORT      — localhost port for the throttle proxy
#                           (default: 4875).
#
# Shellcheck disables are scoped tight — this file is sourced by
# bench.sh, so top-level vars are intentionally not local.

# Resolve this file's directory. Prefer bench.sh's $SCRIPT_DIR when
# we're being sourced from it, and fall back to $BASH_SOURCE otherwise
# (manual `source benchmarks/hermetic.bash` from the repo root, CI ad
# hoc checks, etc.). $BASH_SOURCE is array-indexed in bash but some
# harnesses deliver it as a plain string, so we defensively coalesce.
if [ -n "${SCRIPT_DIR:-}" ] && [ -f "$SCRIPT_DIR/hermetic.bash" ]; then
	HERMETIC_DIR="$SCRIPT_DIR"
else
	_hermetic_src="${BASH_SOURCE[0]:-${BASH_SOURCE:-}}"
	if [ -n "$_hermetic_src" ] && [ -f "$_hermetic_src" ]; then
		# shellcheck disable=SC2155
		HERMETIC_DIR="$(cd "$(dirname "$_hermetic_src")" && pwd)"
	else
		echo "ERROR: hermetic.bash could not resolve its own directory." >&2
		echo "  Set SCRIPT_DIR to the benchmarks/ dir before sourcing." >&2
		# shellcheck disable=SC2317  # reachable via source-from-stdin or unusual harness invocations
		return 1 2>/dev/null || exit 1
	fi
	unset _hermetic_src
fi

BENCH_HERMETIC_CACHE="${BENCH_HERMETIC_CACHE:-$HOME/.cache/aube-bench/registry}"
BENCH_VERDACCIO_PORT="${BENCH_VERDACCIO_PORT:-4874}"
BENCH_PROXY_PORT="${BENCH_PROXY_PORT:-4875}"

HERMETIC_STORAGE="$BENCH_HERMETIC_CACHE/storage"
HERMETIC_WARMED_SENTINEL="$BENCH_HERMETIC_CACHE/.warmed"
HERMETIC_LOG="$BENCH_HERMETIC_CACHE/verdaccio.log"
HERMETIC_CONFIG_WARM="$HERMETIC_DIR/registry/config.warm.yaml"
HERMETIC_CONFIG_COLD="$HERMETIC_DIR/registry/config.yaml"

# Install verdaccio globally if it isn't on PATH. Pinned to v6 to match
# test/registry/start.bash — both are meant to track the same upstream.
_hermetic_ensure_verdaccio() {
	if command -v verdaccio >/dev/null 2>&1; then
		return 0
	fi
	echo "Installing verdaccio..." >&2
	npm install --global verdaccio@6 2>&1 | tail -1 >&2
}

# Wait for Verdaccio to answer HTTP on $port. Returns 1 after ~30s.
_hermetic_wait_ready() {
	local port=$1
	local retries=60
	while ! curl -s "http://127.0.0.1:${port}/" >/dev/null 2>&1; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: Verdaccio failed to start on port $port" >&2
			return 1
		fi
		sleep 0.5
	done
}

# Start Verdaccio with the given config file on BENCH_VERDACCIO_PORT.
# Writes PID into $HERMETIC_VERDACCIO_PID (exported so hermetic_stop
# can see it from an EXIT trap).
_hermetic_start_verdaccio() {
	local config=$1
	mkdir -p "$HERMETIC_STORAGE"

	# Work inside the cache dir so Verdaccio's `storage: ./storage`
	# resolves to $HERMETIC_STORAGE. We can't point at the in-repo
	# config directly because Verdaccio resolves `storage:` relative
	# to the config file, so we copy both configs next to the storage
	# dir at startup.
	cp "$config" "$BENCH_HERMETIC_CACHE/config.yaml"

	verdaccio \
		--config "$BENCH_HERMETIC_CACHE/config.yaml" \
		--listen "127.0.0.1:$BENCH_VERDACCIO_PORT" \
		>"$HERMETIC_LOG" 2>&1 &
	HERMETIC_VERDACCIO_PID=$!
	export HERMETIC_VERDACCIO_PID

	if ! _hermetic_wait_ready "$BENCH_VERDACCIO_PORT"; then
		echo "ERROR: Verdaccio log ($HERMETIC_LOG):" >&2
		tail -40 "$HERMETIC_LOG" >&2 || true
		kill "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		return 1
	fi
}

_hermetic_stop_verdaccio() {
	if [ -n "${HERMETIC_VERDACCIO_PID:-}" ]; then
		kill "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		wait "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		unset HERMETIC_VERDACCIO_PID
	fi
}

# Populate the Verdaccio storage from npmjs on first use. Idempotent
# via the `.warmed` sentinel. Running the warm step requires network;
# subsequent benchmark runs are fully offline.
#
# We use `npm install` rather than aube so warming is bootstrap-safe —
# this script has to work even when `cargo build --release` hasn't
# finished yet (some CI flows warm the cache before building aube to
# parallelize).
_hermetic_warm() {
	if [ -f "$HERMETIC_WARMED_SENTINEL" ]; then
		return 0
	fi

	echo "Warming hermetic registry cache at $HERMETIC_STORAGE ..." >&2
	echo "  (one-time network fetch; subsequent runs are offline)" >&2

	_hermetic_ensure_verdaccio
	if ! _hermetic_start_verdaccio "$HERMETIC_CONFIG_WARM"; then
		return 1
	fi

	local warm_dir
	warm_dir=$(mktemp -d "${TMPDIR:-/tmp}/aube-bench-warm.XXXXXX")
	cp "$HERMETIC_DIR/fixture.package.json" "$warm_dir/package.json"

	# Hit the registry directly with npm. --ignore-scripts and
	# --legacy-peer-deps match how the benchmark's own populate step
	# treats the fixture.
	if ! (cd "$warm_dir" && HOME="$warm_dir/home" \
		npm_config_cache="$warm_dir/cache" \
		npm_config_registry="http://127.0.0.1:$BENCH_VERDACCIO_PORT" \
		npm install --ignore-scripts --no-audit --no-fund --legacy-peer-deps >"$warm_dir/npm.log" 2>&1); then
		echo "ERROR: warm step failed. Last lines of npm log:" >&2
		tail -40 "$warm_dir/npm.log" >&2 || true
		_hermetic_stop_verdaccio
		rm -rf "$warm_dir"
		return 1
	fi

	rm -rf "$warm_dir"
	_hermetic_stop_verdaccio
	: >"$HERMETIC_WARMED_SENTINEL"
	echo "Hermetic registry cache warmed." >&2
}

# Optional throttling proxy lifecycle. The proxy is a ~100-line Node
# script with zero deps (benchmarks/throttle-proxy.mjs). We invoke it
# with the upstream URL + rate; it prints "ready" to stdout once
# listening so we know when to return.
_hermetic_start_proxy() {
	local rate=$1
	local upstream="http://127.0.0.1:$BENCH_VERDACCIO_PORT"

	node "$HERMETIC_DIR/throttle-proxy.mjs" \
		--port "$BENCH_PROXY_PORT" \
		--upstream "$upstream" \
		--rate "$rate" \
		>"$BENCH_HERMETIC_CACHE/proxy.log" 2>&1 &
	HERMETIC_PROXY_PID=$!
	export HERMETIC_PROXY_PID

	# Wait for the proxy to come up (it proxies to Verdaccio, which is
	# already ready, so this is usually instantaneous).
	local retries=40
	while ! curl -s "http://127.0.0.1:$BENCH_PROXY_PORT/" >/dev/null 2>&1; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: throttle proxy failed to start on port $BENCH_PROXY_PORT" >&2
			tail -40 "$BENCH_HERMETIC_CACHE/proxy.log" >&2 || true
			kill "$HERMETIC_PROXY_PID" 2>/dev/null || true
			return 1
		fi
		sleep 0.25
	done
}

_hermetic_stop_proxy() {
	if [ -n "${HERMETIC_PROXY_PID:-}" ]; then
		kill "$HERMETIC_PROXY_PID" 2>/dev/null || true
		wait "$HERMETIC_PROXY_PID" 2>/dev/null || true
		unset HERMETIC_PROXY_PID
	fi
}

hermetic_start() {
	mkdir -p "$BENCH_HERMETIC_CACHE"
	_hermetic_ensure_verdaccio
	if ! _hermetic_warm; then
		return 1
	fi

	if ! _hermetic_start_verdaccio "$HERMETIC_CONFIG_COLD"; then
		return 1
	fi

	if [ -n "${BENCH_BANDWIDTH:-}" ]; then
		if ! _hermetic_start_proxy "$BENCH_BANDWIDTH"; then
			_hermetic_stop_verdaccio
			return 1
		fi
		export BENCH_REGISTRY_URL="http://127.0.0.1:$BENCH_PROXY_PORT"
		echo "Hermetic registry: $BENCH_REGISTRY_URL (throttled to $BENCH_BANDWIDTH via proxy, upstream :$BENCH_VERDACCIO_PORT)" >&2
	else
		export BENCH_REGISTRY_URL="http://127.0.0.1:$BENCH_VERDACCIO_PORT"
		echo "Hermetic registry: $BENCH_REGISTRY_URL (unthrottled)" >&2
	fi
}

# Stop proxy before Verdaccio — the proxy holds keep-alive connections
# into it and will emit noise if Verdaccio disappears first.
hermetic_stop() {
	_hermetic_stop_proxy
	_hermetic_stop_verdaccio
}
