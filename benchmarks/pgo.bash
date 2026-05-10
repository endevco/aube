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
	(cd "$run_dir" && HOME="$run_dir/home" "$INSTRUMENTED_BIN" install --ignore-scripts >/dev/null)
}

warm_run() {
	local run_dir=$1 i=$2
	echo "  train: warm install ($i)"
	(cd "$run_dir" && HOME="$run_dir/home" "$INSTRUMENTED_BIN" install --ignore-scripts >/dev/null)
}

for i in 1 2 3; do
	cold_run "$i"
done
for i in 1 2 3; do
	warm_run "$train_dir/cold.3" "$i"
done

hermetic_stop

# ---------- Phase 3a: merge ----------
echo ">>> [3/3] Merging profile data"
"$LLVM_PROFDATA" merge -o "$PGO_MERGED" "$PGO_PROFRAW_DIR"

if [ -n "${AUBE_PGO_SKIP_FINAL_BUILD:-}" ]; then
	echo ">>> Skipping final optimized build (AUBE_PGO_SKIP_FINAL_BUILD=1)"
	echo ">>> Profile ready at: $PGO_MERGED"
	exit 0
fi

# ---------- Phase 3b: optimize ----------
echo ">>> Rebuilding with -Cprofile-use"

# -Cllvm-args=-pgo-warn-missing-function: informational only —
# functions the training run didn't exercise stay un-PGO'd, which is
# fine, but the warning surfaces unexpectedly cold paths.
# shellcheck disable=SC2086 # intentional word-splitting on $target_arg
RUSTFLAGS="-Cprofile-use=$PGO_MERGED -Cllvm-args=-pgo-warn-missing-function" \
	"$PGO_BUILD_TOOL" build --profile="$PGO_PROFILE" $target_arg -p aube

echo ">>> PGO build complete: $INSTRUMENTED_BIN"
ls -lh "$INSTRUMENTED_BIN"
