#!/usr/bin/env bash
# Profile-Guided Optimization build for aube.
#
# Three-phase rustc PGO flow:
#
#   1. Build aube with -Cprofile-generate (instrumented binary).
#   2. Train against the hermetic Verdaccio registry — a mix of cold
#      and warm installs of fixture.package.json — so the profile
#      covers the resolver / registry / store / linker hot paths and
#      the frozen-lockfile fast path.
#   3. Merge .profraw via llvm-profdata, recompile with -Cprofile-use.
#
# Local default: target/release-pgo/aube using profile=release-pgo.
#
# Holds /tmp/aube-bench.lock for the entire run because the hermetic
# registry (port 4874), throttle proxy (port 4875), and warmed cache
# (~/.cache/aube-bench/registry) are shared across worktrees,
# terminals, and agents.
#
# CI hooks (env vars):
#   AUBE_PGO_NO_LOCK=1          skip /tmp/aube-bench.lock acquisition
#                               (also auto-skipped if `flock` is missing,
#                               e.g. on macOS).
#   AUBE_PGO_PROFILE=<profile>  cargo profile for both phases (default:
#                               release-pgo). Set to `release` in CI when
#                               the final build is delegated to another
#                               step.
#   AUBE_PGO_TARGET=<triple>    cross-compilation target (default: host).
#                               Output lands at target/<triple>/<profile>/.
#   AUBE_PGO_BUILD_TOOL=<tool>  `cargo` (default) or `cross`. cross is
#                               used in CI for Linux GNU/musl targets so
#                               the resulting binary keeps cross's older
#                               glibc baseline. Cross.toml passes RUSTFLAGS
#                               through to the container.
#   AUBE_PGO_SKIP_FINAL_BUILD=1 stop after merging .profraw. Use when the
#                               final optimized build is delegated to a
#                               separate step (e.g. taiki-e action) that
#                               picks up RUSTFLAGS+CARGO_PROFILE_RELEASE_LTO
#                               from the environment.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PGO_DATA_DIR="$REPO_ROOT/target/pgo-data"
PGO_PROFRAW_DIR="$PGO_DATA_DIR/profraw"
PGO_MERGED="$PGO_DATA_DIR/merged.profdata"

PGO_PROFILE="${AUBE_PGO_PROFILE:-release-pgo}"
PGO_TARGET="${AUBE_PGO_TARGET:-}"
PGO_BUILD_TOOL="${AUBE_PGO_BUILD_TOOL:-cargo}"

# target_arg stays unquoted at expansion sites: empty string disappears,
# "--target=foo" expands to one arg. Avoids bash 3.2 (macOS) array+set -u
# unbound-variable issues with "${arr[@]}".
target_arg=""
target_dir_part=""
if [ -n "$PGO_TARGET" ]; then
	target_arg="--target=$PGO_TARGET"
	target_dir_part="$PGO_TARGET/"
fi

# Default to the same throttled hermetic registry the rest of aube's
# bench harness uses, so PGO numbers and bench numbers stay comparable.
export BENCH_HERMETIC="${BENCH_HERMETIC:-1}"
export BENCH_BANDWIDTH="${BENCH_BANDWIDTH:-500mbit}"
export BENCH_LATENCY="${BENCH_LATENCY:-50ms}"

if [ -z "${AUBE_PGO_NO_LOCK:-}" ] && command -v flock >/dev/null 2>&1; then
	echo ">>> Acquiring /tmp/aube-bench.lock (30 min timeout)"
	exec 9>/tmp/aube-bench.lock
	if ! flock -w 1800 9; then
		echo "ERROR: failed to acquire /tmp/aube-bench.lock after 30 min" >&2
		exit 1
	fi
	echo ">>> Lock acquired"
else
	echo ">>> Skipping /tmp/aube-bench.lock (AUBE_PGO_NO_LOCK or flock missing)"
fi

RUSTC_HOST="$(rustc -vV | sed -n 's|^host: ||p')"
RUSTC_SYSROOT="$(rustc --print sysroot)"
LLVM_PROFDATA="$RUSTC_SYSROOT/lib/rustlib/$RUSTC_HOST/bin/llvm-profdata"
if [ ! -x "$LLVM_PROFDATA" ]; then
	echo "ERROR: llvm-profdata not found at $LLVM_PROFDATA" >&2
	echo "  Install with: rustup component add llvm-tools-preview" >&2
	exit 1
fi

mkdir -p "$PGO_PROFRAW_DIR"
rm -f "$PGO_PROFRAW_DIR"/*.profraw "$PGO_MERGED"

# With AUBE_PGO_BUILD_TOOL=cross, rustc runs inside a container that
# mounts the project at `/project` (not at the host path), so when
# phase 3b reads `-Cprofile-use=<host-path>` from RUSTFLAGS the file
# is invisible — rustc bails with "file ... does not exist" even when
# the merge step wrote it on the host. Bind-mount PGO_DATA_DIR at the
# same host path inside the container so the existing RUSTFLAGS value
# resolves. Harmless on the host-side phase 1 build (cross still
# writes the instrumented binary to target/ via its own bind mount).
if [ "$PGO_BUILD_TOOL" = "cross" ]; then
	export CROSS_CONTAINER_OPTS="${CROSS_CONTAINER_OPTS:-} -v $PGO_DATA_DIR:$PGO_DATA_DIR:rw"
fi

# ---------- Phase 1: instrumented build ----------
echo ">>> [1/3] Building instrumented binary ($PGO_BUILD_TOOL, profile=$PGO_PROFILE${PGO_TARGET:+, target=$PGO_TARGET})"
# shellcheck disable=SC2086 # intentional word-splitting on $target_arg
RUSTFLAGS="-Cprofile-generate=$PGO_PROFRAW_DIR" \
	"$PGO_BUILD_TOOL" build --profile="$PGO_PROFILE" $target_arg -p aube

INSTRUMENTED_BIN="$REPO_ROOT/target/${target_dir_part}${PGO_PROFILE}/aube"
if [ ! -x "$INSTRUMENTED_BIN" ]; then
	echo "ERROR: instrumented binary missing at $INSTRUMENTED_BIN" >&2
	exit 1
fi

# ---------- Phase 2: training ----------
echo ">>> [2/3] Training against hermetic registry"

# shellcheck disable=SC1091
source "$SCRIPT_DIR/hermetic.bash"

train_dir="$(mktemp -d "${TMPDIR:-/tmp}/aube-pgo-train.XXXXXX")"
cleanup() {
	hermetic_stop || true
	rm -rf "$train_dir"
}
trap cleanup EXIT

AUBE_BIN="$INSTRUMENTED_BIN" hermetic_start

# hermetic_start runs _hermetic_warm on the first invocation against a
# given cache dir, which executes the instrumented binary against npmjs
# uplink. In CI that warm step fires every run (no persisted cache) and
# would otherwise contribute non-representative profraw covering the
# uplink path. Drop those before the real training runs land.
rm -f "$PGO_PROFRAW_DIR"/*.profraw

# Force the instrumented binary to write profraw to a host path we
# control, regardless of what `-Cprofile-generate=<dir>` baked in at
# compile time. With AUBE_PGO_BUILD_TOOL=cross the rustc compile runs
# inside a container where the project may be mounted under a path
# that differs from the host (cross's default MOUNT_FINDER puts the
# workspace at `/project`). The host path embedded in the binary then
# doesn't resolve at runtime, profraw goes nowhere, and llvm-profdata
# silently produces no merged output. Setting LLVM_PROFILE_FILE at
# runtime sidesteps the path-translation question entirely. %m
# disambiguates per module signature; %p per process — together they
# keep the 6 training runs from colliding on the same file.
PROFRAW_PATTERN="$PGO_PROFRAW_DIR/aube-%m-%p.profraw"

# 3 cold + 3 warm. Cold runs each get a fresh dir so the resolver,
# registry, store, and linker hot paths all run end-to-end. Warm runs
# reuse the last cold dir so the frozen-lockfile fast path is also
# represented in the profile.
cold_run() {
	local i=$1
	local run_dir="$train_dir/cold.$i"
	mkdir -p "$run_dir/home"
	cp "$SCRIPT_DIR/fixture.package.json" "$run_dir/package.json"
	printf 'registry=%s\n' "$BENCH_REGISTRY_URL" >"$run_dir/.npmrc"
	printf 'registry=%s\n' "$BENCH_REGISTRY_URL" >"$run_dir/home/.npmrc"
	echo "  train: cold install ($i)"
	(cd "$run_dir" && HOME="$run_dir/home" LLVM_PROFILE_FILE="$PROFRAW_PATTERN" "$INSTRUMENTED_BIN" install --ignore-scripts >/dev/null)
}

warm_run() {
	local run_dir=$1 i=$2
	echo "  train: warm install ($i)"
	(cd "$run_dir" && HOME="$run_dir/home" LLVM_PROFILE_FILE="$PROFRAW_PATTERN" "$INSTRUMENTED_BIN" install --ignore-scripts >/dev/null)
}

for i in 1 2 3; do
	cold_run "$i"
done
for i in 1 2 3; do
	warm_run "$train_dir/cold.3" "$i"
done

hermetic_stop

# Sanity check: confirm training actually wrote profraw. Without
# LLVM_PROFILE_FILE this silently produced zero files on cross-built
# targets — llvm-profdata then merged nothing and phase 3b failed with
# "file ... merged.profdata does not exist". Fail loudly here instead.
profraw_count=$(find "$PGO_PROFRAW_DIR" -maxdepth 1 -name '*.profraw' -type f | wc -l | tr -d ' ')
if [ "$profraw_count" -eq 0 ]; then
	echo "ERROR: no .profraw files written to $PGO_PROFRAW_DIR after training" >&2
	echo "  Training ran but the instrumented binary did not record profile data." >&2
	echo "  Check LLVM_PROFILE_FILE handling and the cross host/container mount." >&2
	exit 1
fi
echo ">>> $profraw_count .profraw files collected"

# ---------- Phase 3a: merge ----------
echo ">>> [3/3] Merging profile data"
"$LLVM_PROFDATA" merge -o "$PGO_MERGED" "$PGO_PROFRAW_DIR"

# Defense in depth: confirm llvm-profdata actually wrote the merged
# file. A version mismatch between the rustc that instrumented (phase 1
# inside cross) and the host's llvm-profdata can produce a 0-exit
# silent no-op. Without this check we'd fall through to phase 3b and
# see rustc's opaque "file ... does not exist" against a path that
# really doesn't exist on the host at all.
if [ ! -f "$PGO_MERGED" ]; then
	echo "ERROR: $PGO_MERGED was not produced by llvm-profdata merge" >&2
	echo "  Check that the host's llvm-profdata version matches the rustc that built the instrumented binary." >&2
	exit 1
fi
echo ">>> merged profile written: $(stat -c %s "$PGO_MERGED" 2>/dev/null || stat -f %z "$PGO_MERGED") bytes"

if [ -n "${AUBE_PGO_SKIP_FINAL_BUILD:-}" ]; then
	echo ">>> Skipping final optimized build (AUBE_PGO_SKIP_FINAL_BUILD=1)"
	echo ">>> Profile ready at: $PGO_MERGED"
	exit 0
fi

# ---------- Phase 3b: optimize ----------
echo ">>> Rebuilding with -Cprofile-use"

# -Cllvm-args=-pgo-warn-missing-function=false: silence LLVM's per-symbol
# "no profile data available for function …" notes during phase 3b.
# Coverage gaps are expected — the training run can't exercise every
# code path — and emitting a warning per uncovered symbol drowns the CI
# build log without surfacing actionable signal. The functions still
# get compiled, just without PGO data, which is the documented fallback.
# shellcheck disable=SC2086 # intentional word-splitting on $target_arg
RUSTFLAGS="-Cprofile-use=$PGO_MERGED -Cllvm-args=-pgo-warn-missing-function=false" \
	"$PGO_BUILD_TOOL" build --profile="$PGO_PROFILE" $target_arg -p aube

# Phase 3b wrote to the same path as phase 1, so the file at
# $INSTRUMENTED_BIN is now the PGO-optimized build, not the instrumented
# one. Alias for clarity in the success log.
PGO_FINAL_BIN="$INSTRUMENTED_BIN"
echo ">>> PGO build complete: $PGO_FINAL_BIN"
ls -lh "$PGO_FINAL_BIN"
