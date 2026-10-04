#!/usr/bin/env bash
# mink runtime benchmark runner.
#
# Collects machine/OS/commit/toolchain metadata, runs the cargo bench suite
# (`crates/mink-core/benches/runtime_bench.rs`) and stores a JSON + text report
# under target/bench/.
#
# Usage:
#   ./scripts/bench-runtime.sh [--quick] [--cases a,b] [--reps N] [--out DIR] [--list]
#
#   --quick   run the reduced matrix (skips idle_1000/replay_100k/turns_2000/...)
#   --cases   comma-separated case prefixes (e.g. mock_turn,sse,replay_10k)
#   --reps    repetition override for latency cases
#   --out     report directory (default: target/bench)
#   --list    print the case list and exit
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Session-density benchmarks open many session files; raise the soft fd limit
# when possible (macOS default 256 truncates idle_1000 early).
ulimit -n 8192 2>/dev/null || ulimit -n 4096 2>/dev/null || true
FD_LIMIT="$(ulimit -n)"
export MINK_BENCH_FD_LIMIT="$FD_LIMIT"

QUICK=0
CASES=""
REPS=""
OUT_DIR="target/bench"
LIST_ONLY=0

usage() {
  sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --quick) QUICK=1 ;;
    --cases) CASES="${2:?--cases needs a value}"; shift ;;
    --reps) REPS="${2:?--reps needs a value}"; shift ;;
    --out) OUT_DIR="${2:?--out needs a value}"; shift ;;
    --list) LIST_ONLY=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

# cargo runs bench binaries with cwd = package root, so report paths must be absolute.
case "$OUT_DIR" in
  /*) ;;
  *) OUT_DIR="$ROOT/$OUT_DIR" ;;
esac

# ── machine / toolchain metadata ────────────────────────────────────
OS_NAME="$(uname -s)"
if [[ "$OS_NAME" == "Darwin" ]]; then
  MACHINE="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m)"
  OS_VERSION="macOS $(sw_vers -productVersion 2>/dev/null || echo unknown)"
  KERNEL="$(uname -r)"
  CORES="$(sysctl -n hw.ncpu 2>/dev/null || echo unknown)"
  MEM_BYTES="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"
  RSS_METRIC="ps_rss_kb (current) + 20ms sampler peak; macOS RSS overstates footprint"
else
  MACHINE="$(awk -F: '/model name/ {sub(/^ +/, "", $2); print $2; exit}' /proc/cpuinfo 2>/dev/null || uname -m)"
  OS_VERSION="$( (grep -E '^PRETTY_NAME=' /etc/os-release 2>/dev/null | cut -d= -f2- | tr -d '"') || uname -s)"
  KERNEL="$(uname -r)"
  CORES="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo unknown)"
  MEM_BYTES="$(awk '/MemTotal/ {print $2 * 1024; exit}' /proc/meminfo 2>/dev/null || echo 0)"
  RSS_METRIC="VmRSS (current) from /proc/self/status + 20ms sampler peak"
fi

if [[ "$MEM_BYTES" =~ ^[0-9]+$ ]] && [[ "$MEM_BYTES" -gt 0 ]]; then
  MEM_GB="$((MEM_BYTES / 1024 / 1024 / 1024))GB"
else
  MEM_GB="unknown"
fi

COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
if [[ -n "$(git status --porcelain --untracked-files=no 2>/dev/null | head -1)" ]]; then
  COMMIT="${COMMIT}+dirty"
fi
RUSTC_VERSION="$(rustc -V 2>/dev/null || echo unknown)"
CARGO_VERSION="$(cargo -V 2>/dev/null || echo unknown)"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

export MINK_BENCH_COMMIT="$COMMIT"
export MINK_BENCH_MACHINE="$MACHINE"
export MINK_BENCH_KERNEL="$KERNEL"
export MINK_BENCH_RUSTC="$RUSTC_VERSION"

if [[ "$LIST_ONLY" == "1" ]]; then
  args=(--bench runtime_bench -- --list)
  [[ "$QUICK" == "1" ]] && args=(--bench runtime_bench -- --quick --list)
  exec cargo bench -p mink-core "${args[@]}"
fi

mkdir -p "$OUT_DIR"
JSON_REPORT="$OUT_DIR/runtime-$COMMIT-$STAMP.json"
TEXT_REPORT="$OUT_DIR/runtime-$COMMIT-$STAMP.txt"

{
  echo "# mink runtime benchmark"
  echo "# machine:  $MACHINE (${CORES} cores, ${MEM_GB})"
  echo "# os:       $OS_VERSION (kernel $KERNEL)"
  echo "# commit:   $COMMIT"
  echo "# rustc:    $RUSTC_VERSION"
  echo "# cargo:    $CARGO_VERSION"
  echo "# rss:      $RSS_METRIC"
  echo "# fd limit: $FD_LIMIT"
  echo "# quick:    $QUICK"
  echo "# reps:     ${REPS:-per-case default}"
  echo
} > "$TEXT_REPORT"

BENCH_ARGS=(--bench runtime_bench -- --json "$JSON_REPORT")
if [[ "$QUICK" == "1" ]]; then BENCH_ARGS+=(--quick); fi
if [[ -n "$CASES" ]]; then BENCH_ARGS+=(--case "$CASES"); fi
if [[ -n "$REPS" ]]; then BENCH_ARGS+=(--reps "$REPS"); fi

echo "── mink runtime benchmark ──────────────────────────────────"
echo "machine : $MACHINE (${CORES} cores, ${MEM_GB})"
echo "os      : $OS_VERSION"
echo "commit  : $COMMIT"
echo "rustc   : $RUSTC_VERSION"
echo "rss     : $RSS_METRIC"
echo "fd limit: $FD_LIMIT"
echo "report  : $JSON_REPORT"
echo "────────────────────────────────────────────────────────────"

cargo bench -p mink-core "${BENCH_ARGS[@]}" 2>&1 | tee -a "$TEXT_REPORT"

echo
echo "json report: $JSON_REPORT"
echo "text report: $TEXT_REPORT"
