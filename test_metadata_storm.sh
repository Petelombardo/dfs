#!/bin/bash
# Test: metadata write storm — simulates rsync of many small files.
# Measures whether the cluster stays responsive under concurrent small-file load.
#
# Usage:
#   ./test_metadata_storm.sh [mountpoint]
#
# Run once against the pre-fix release binary, then again against the post-fix
# debug binary to compare behaviour.

set -euo pipefail

MOUNTPOINT="${1:-/tmp/dfs-mount}"
TESTDIR="$MOUNTPOINT/storm_test_$$"
FILE_COUNT=2000      # total files to write
CONCURRENCY=100      # files in flight at once
FILE_SIZE_MIN=0      # touch only — pure metadata, no chunk data
FILE_SIZE_MAX=0
PROBE_INTERVAL=2     # seconds between liveness probes
RESULTS=()

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC} $*"; }
fail() { echo -e "${RED}FAIL${NC} $*"; }
info() { echo -e "${YELLOW}INFO${NC} $*"; }

# ── Preflight ──────────────────────────────────────────────────────────────────

if ! mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "ERROR: $MOUNTPOINT is not mounted. Start the cluster and mount first."
    echo "  ./start_local_cluster.sh"
    echo "  ./mount_local_cluster.sh $MOUNTPOINT"
    exit 1
fi

info "Cluster liveness check..."
if ! timeout 5 ls "$MOUNTPOINT" >/dev/null 2>&1; then
    fail "Cluster not responding before test even started."
    exit 1
fi
pass "Cluster responsive before storm."

mkdir -p "$TESTDIR"
info "Touching $FILE_COUNT files ($CONCURRENCY concurrent) to $TESTDIR (pure metadata storm)..."
echo

# ── Storm writer ──────────────────────────────────────────────────────────────
# Write FILE_COUNT files with CONCURRENCY in parallel.
# Each file gets a random size to mimic real source trees.

WRITTEN=0
FAILED=0
START_TIME=$(date +%s%3N)  # ms

write_file() {
    local idx="$1"
    local path="$TESTDIR/file_$(printf '%05d' "$idx").bin"
    if touch "$path" 2>/dev/null; then
        echo "ok"
    else
        echo "fail"
    fi
}
export -f write_file
export TESTDIR

# Run with xargs for concurrency control
seq 1 "$FILE_COUNT" | \
    xargs -P "$CONCURRENCY" -I{} bash -c 'write_file "$@"' _ {} \
    > /tmp/storm_results_$$.txt 2>&1 &
STORM_PID=$!

# ── Liveness probe ────────────────────────────────────────────────────────────
# Every PROBE_INTERVAL seconds, check that the cluster still responds.
# Record latency of each probe. A stall shows up as a multi-second gap.

info "Probing cluster liveness every ${PROBE_INTERVAL}s while storm runs..."
echo

PROBE_NUM=0
MAX_LATENCY_MS=0
STALL_DETECTED=0

while kill -0 "$STORM_PID" 2>/dev/null; do
    sleep "$PROBE_INTERVAL"
    PROBE_NUM=$((PROBE_NUM + 1))
    PROBE_START=$(date +%s%3N)

    if timeout 10 ls "$TESTDIR" >/dev/null 2>&1; then
        PROBE_END=$(date +%s%3N)
        LAT=$((PROBE_END - PROBE_START))
        if [ "$LAT" -gt "$MAX_LATENCY_MS" ]; then
            MAX_LATENCY_MS=$LAT
        fi
        if [ "$LAT" -gt 5000 ]; then
            info "  probe #${PROBE_NUM}: ${LAT}ms  ← HIGH LATENCY"
        else
            info "  probe #${PROBE_NUM}: ${LAT}ms"
        fi
    else
        PROBE_END=$(date +%s%3N)
        LAT=$((PROBE_END - PROBE_START))
        fail "  probe #${PROBE_NUM}: TIMEOUT after ${LAT}ms — cluster seized!"
        STALL_DETECTED=1
    fi
done

wait "$STORM_PID" 2>/dev/null || true

# ── Results ───────────────────────────────────────────────────────────────────

END_TIME=$(date +%s%3N)
ELAPSED_MS=$((END_TIME - START_TIME))
ELAPSED_S=$(echo "scale=1; $ELAPSED_MS / 1000" | bc)

WRITTEN=$(grep -c "^ok$" /tmp/storm_results_$$.txt 2>/dev/null || echo 0)
FAILED=$(grep -c "^fail$" /tmp/storm_results_$$.txt 2>/dev/null || echo 0)
rm -f /tmp/storm_results_$$.txt

echo
echo "══════════════════════════════════════════════════════"
echo "  Storm test results"
echo "══════════════════════════════════════════════════════"
info "  Files written:     $WRITTEN / $FILE_COUNT"
info "  Write failures:    $FAILED"
info "  Elapsed:           ${ELAPSED_S}s"
info "  Max probe latency: ${MAX_LATENCY_MS}ms"
echo

# Final liveness check
FINAL_START=$(date +%s%3N)
if timeout 10 ls "$TESTDIR" >/dev/null 2>&1; then
    FINAL_LAT=$(( $(date +%s%3N) - FINAL_START ))
    pass "Cluster responsive after storm (${FINAL_LAT}ms)."
else
    fail "Cluster unresponsive after storm."
    STALL_DETECTED=1
fi

# Count files actually visible on the FS
VISIBLE=$(ls "$TESTDIR" 2>/dev/null | wc -l)
info "  Files visible on FS: $VISIBLE"

echo

if [ "$STALL_DETECTED" -eq 1 ]; then
    fail "VERDICT: cluster SEIZED during storm. ✗"
    EXIT=1
elif [ "$MAX_LATENCY_MS" -gt 5000 ]; then
    fail "VERDICT: cluster survived but showed severe latency (${MAX_LATENCY_MS}ms). ✗"
    EXIT=1
elif [ "$WRITTEN" -lt $((FILE_COUNT * 90 / 100)) ]; then
    fail "VERDICT: too many write failures ($FAILED / $FILE_COUNT). ✗"
    EXIT=1
else
    pass "VERDICT: cluster survived storm with max probe latency ${MAX_LATENCY_MS}ms. ✓"
    EXIT=0
fi

# Cleanup
info "Cleaning up test files..."
rm -rf "$TESTDIR" 2>/dev/null || true

exit $EXIT
