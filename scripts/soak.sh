#!/usr/bin/env bash
# Soak harness — the 05-roadmap P6 acceptance: N hours of mixed traffic, RSS
# flat. Defaults are the real soak; SOAK_DURATION=180 is a proof run.
#
# Point it at a config whose upstream ANSWERS: `scripts/soak-fixtures` plus
# `scripts/serve_stub_upstreams.py` on the same ports. Against the CLI fixtures
# (dummy keys) it soaks the failure path, which is a legitimate leg but not an
# RSS-flatness claim about success-path traffic.
#
# Fails when: /healthz ever answers non-200, or RSS growth past the startup
# plateau exceeds SOAK_RSS_GROWTH_LIMIT_KB (default 64MB — jemalloc arena
# growth included, which is why the tuned conf ships in the binary).
#
# Usage:  python scripts/serve_stub_upstreams.py --ports 19001 &
#         scripts/soak.sh scripts/soak-fixtures            # 12h
#         SOAK_DURATION=180 scripts/soak.sh scripts/soak-fixtures   # 3-min proof

set -uo pipefail

DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG_DIR="${1:-$DIR/crates/ar-cli/tests/fixtures}"
DURATION="${SOAK_DURATION:-43200}"
WORKERS="${SOAK_CONCURRENCY:-8}"
SAMPLE_EVERY="${SOAK_SAMPLE_EVERY:-60}"
GROWTH_LIMIT="${SOAK_RSS_GROWTH_LIMIT_KB:-65536}"
LOG="${SOAK_LOG:-/tmp/ar-soak.csv}"
PORT="${SOAK_PORT:-20128}"

AR="$DIR/target/release/ar"
[ -x "$AR" ] || { echo "soak: build first (cargo build --release)"; exit 1; }
command -v curl >/dev/null || { echo "soak: curl required"; exit 1; }

cd "$CONFIG_DIR" || exit 1
unset _RJEM_MALLOC_CONF           # measure the binary's own default
"$AR" serve --port "$PORT" >/tmp/ar-soak-serve.log 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null; wait "$PID" 2>/dev/null' EXIT

# The point of the run is this binary's own RSS, so refuse to sample anything else:
# a stale server already holding the port answers /healthz while our child dies of
# EADDRINUSE, and the CSV silently records rss_kb=0 for twelve hours.
sleep 4
kill -0 "$PID" 2>/dev/null || {
    echo "soak: serve exited; log tail:"; tail -5 /tmp/ar-soak-serve.log; exit 1;
}
curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null || {
    echo "soak: serve did not come up; log tail:"; tail -5 /tmp/ar-soak-serve.log; exit 1;
}

rss_kb() { awk '/VmRSS/{print $2; exit}' "/proc/$1/status" 2>/dev/null; }

# The plateau is the FIRST sample at or after this point: startup allocations
# (config parse, catalog merge, TLS roots) finish inside it, and growth past it
# is the thing a soak exists to catch. Clamped to a quarter of the run so a
# short proof run still establishes a baseline.
plateau_after=$(( DURATION / 4 )); [ "$plateau_after" -gt 120 ] && plateau_after=120
beat() {
    local pids=()
    for w in $(seq "$WORKERS"); do
        {
            # --max-time 45: a dead upstream can hang until its provider
            # deadline; the curl must not outlive the beat by minutes.
            curl -s --max-time 45 -o /dev/null -X POST "http://127.0.0.1:$PORT/v1/chat/completions" \
                -H "Content-Type: application/json" \
                -d "{\"model\":\"m\",\"messages\":[{\"role\":\"user\",\"content\":\"soak-$w-$RANDOM\"}]}"
            curl -s --max-time 45 -o /dev/null -X POST "http://127.0.0.1:$PORT/v1/chat/completions" \
                -H "Content-Type: application/json" \
                -d "{\"model\":\"m\",\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"s$w\"}]}"
            curl -s --max-time 45 -o /dev/null -X POST "http://127.0.0.1:$PORT/v1/embeddings" \
                -H "Content-Type: application/json" -d '{"model":"m","input":"soak"}'
        } &
        pids+=($!)
    done
    # Wait for THIS beat's curls by pid. A bare `wait` would also reap the
    # script's other jobs — including the server it is meant to be measuring —
    # which is how an earlier harness version sat in `wait` for twenty-eight
    # minutes and never wrote a sample.
    local pid
    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null; done
    return 0
}

echo "elapsed_s,rss_kb,healthz,failures" > "$LOG"
failures=0
first_ss_rss=""
last_rss=0
t_last_beat=0
START=$(date +%s)
while :; do
    now=$(date +%s)
    elapsed=$((now - START))
    if [ "$elapsed" -ge "$DURATION" ]; then break; fi
    if [ $((now - t_last_beat)) -ge "$SAMPLE_EVERY" ]; then
        beat
        t_last_beat=$(date +%s)
        hz=0
        curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && hz=200 || failures=$((failures+1))
        rss=$(rss_kb "$PID")
        [ -n "$rss" ] || rss=0
        if [ -z "$first_ss_rss" ] && [ "$elapsed" -ge "$plateau_after" ]; then
            first_ss_rss=$rss
        fi
        last_rss=$rss
        echo "$elapsed,$rss,$hz,$failures" >> "$LOG"
    fi
    sleep 5
done

echo "---"
echo "soak: ${elapsed}s, log at $LOG"
echo "soak: plateau RSS ${first_ss_rss:-?}kB -> final ${last_rss}kB (limit +${GROWTH_LIMIT}kB), healthz failures: $failures"

rc=0
if [ "$failures" -gt 0 ]; then rc=1; echo "soak: FAIL — healthz not clean"; fi
if [ -n "$first_ss_rss" ] && [ "$last_rss" -gt $((first_ss_rss + GROWTH_LIMIT)) ]; then
    echo "soak: FAIL — RSS grew $((last_rss - first_ss_rss))kB past the plateau"
    rc=1
fi
exit $rc
