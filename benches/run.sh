#!/usr/bin/env bash
# Runs the MCP benchmark for Agora and for the legacy Bun server, both through
# benches/mcp_bench.py. Builds nothing. See benches/README.md.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH_DIR="$REPO_ROOT/benches"
DRIVER="$BENCH_DIR/mcp_bench.py"
AGORA_BIN="$REPO_ROOT/target/release/agora"
LEGACY_DIR="$REPO_ROOT/legacy"
QUICK=0

usage() {
  cat <<'EOF'
Usage: benches/run.sh [--agora-bin PATH] [--legacy-dir DIR] [--quick]

  --agora-bin PATH   agora binary (default: target/release/agora)
  --legacy-dir DIR   legacy tree with src/server.ts and node_modules (default: legacy)
  --quick            smoke-run sizes: 4 sessions, 20 messages, 20 per writer, 5 s idle window
EOF
}

# Resolve a path given on the command line against the caller's directory.
abs_path() {
  case "$1" in
    /*) printf '%s\n' "$1" ;;
    *) printf '%s/%s\n' "$PWD" "$1" ;;
  esac
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --agora-bin)
      [[ $# -ge 2 ]] || { echo "error: --agora-bin needs a value" >&2; exit 2; }
      AGORA_BIN="$(abs_path "$2")"; shift 2 ;;
    --legacy-dir)
      [[ $# -ge 2 ]] || { echo "error: --legacy-dir needs a value" >&2; exit 2; }
      LEGACY_DIR="$(abs_path "$2")"; shift 2 ;;
    --quick) QUICK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ ! -x "$AGORA_BIN" ]]; then
  echo "error: agora binary not found or not executable: $AGORA_BIN" >&2
  echo "hint: cargo build --release -p agora   (or pass --agora-bin PATH)" >&2
  exit 1
fi

LEGACY_SERVER="$LEGACY_DIR/src/server.ts"
LEGACY_SKIP=""
if ! command -v bun >/dev/null 2>&1; then
  LEGACY_SKIP="bun is not on PATH"
elif [[ ! -f "$LEGACY_SERVER" ]]; then
  LEGACY_SKIP="$LEGACY_SERVER is missing"
elif [[ ! -d "$LEGACY_DIR/node_modules" ]]; then
  LEGACY_SKIP="$LEGACY_DIR/node_modules is missing"
fi
if [[ -n "$LEGACY_SKIP" ]]; then
  echo "skipping legacy: $LEGACY_SKIP" >&2
  echo "to install it hermetically, run:" >&2
  echo "  cd $LEGACY_DIR && HOME=\$(mktemp -d) BUN_INSTALL_CACHE_DIR=/tmp/agora-bench-bun-cache bun install --frozen-lockfile" >&2
fi

if (( QUICK )); then
  SESSIONS=4; MESSAGES=20; PER_WRITER=20; IDLE_SECONDS=5
else
  SESSIONS=20; MESSAGES=200; PER_WRITER=250; IDLE_SECONDS=60
fi
WRITERS=4

# Server commands as the driver shell-splits them. %q quotes paths with spaces.
printf -v AGORA_CMD '%q mcp' "$AGORA_BIN"
printf -v LEGACY_CMD 'bun run %q' "$LEGACY_SERVER"

TMP="$(mktemp -d /tmp/agora-bench.XXXX)"
trap 'rm -rf "$TMP"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

RESULTS_DIR="$BENCH_DIR/results"
mkdir -p "$RESULTS_DIR"
OUT="$RESULTS_DIR/$(date -u +%Y%m%dT%H%M%SZ)-$(hostname -s).json"

PARAMS=(--writers "$WRITERS" --sessions "$SESSIONS" --messages "$MESSAGES"
        --per-writer "$PER_WRITER" --idle-seconds "$IDLE_SECONDS")
FRAGMENTS=()
RAN=0

# Scenarios outermost, so each scenario runs for agora, then legacy.
for scenario in startup idle latency throughput; do
  for backend in agora legacy; do
    if [[ $backend == legacy && -n "$LEGACY_SKIP" ]]; then
      continue
    fi
    # A fresh data home per run: the cold startup sample needs an empty daemon state.
    home="$(mktemp -d "$TMP/$backend-home.XXXX")"
    frag="$TMP/$backend-$scenario.json"
    echo "== $backend / $scenario ==" >&2
    if [[ $backend == agora ]]; then
      AGORA_HOME="$home" python3 -I "$DRIVER" --backend agora --server-cmd "$AGORA_CMD" \
        --scenario "$scenario" "${PARAMS[@]}" --out "$frag"
    else
      CLAUDETALK_HOME="$home" python3 -I "$DRIVER" --backend legacy --server-cmd "$LEGACY_CMD" \
        --scenario "$scenario" "${PARAMS[@]}" --out "$frag"
    fi
    FRAGMENTS+=("$frag")
    RAN=$((RAN + 1))
  done
done

if (( RAN == 0 )); then
  echo "error: no benchmark ran" >&2
  exit 1
fi

python3 -I "$DRIVER" --merge "${FRAGMENTS[@]}" --out "$OUT"
echo "" >&2
python3 -I "$DRIVER" --summarize "$OUT"
echo "" >&2
echo "results: $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)" >&2
