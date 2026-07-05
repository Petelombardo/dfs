#!/bin/bash
# patch_timing_test.sh — times a small in-place patch (PatchChunk/MultiPatch path) at the
# beginning, middle, and end of two differently-sized files, averaged over several
# iterations. Run directly on a client host that already has the DFS mounted (e.g.
# rock5b's /mnt/dfs). Separates "does patch latency scale with position in the file"
# from "does it scale with total file size" — and averaging over N iterations filters
# out momentary blips from healing contention.
#
# Usage: patch_timing_test.sh [mount_point] [iterations] [keep]
#   keep = "keep" to skip final cleanup and reuse existing same-size test files instead
#   of recreating them (much faster for repeated re-testing of the same fix).
set -e

MOUNT="${1:-/mnt/dfs}"
ITERS="${2:-10}"
KEEP="${3:-}"
PATCH_SIZE=4096

RESULTS_FILE=$(mktemp)
trap 'rm -f "$RESULTS_FILE"' EXIT

create_file() {
    local path="$1" size_mb="$2"
    local want_bytes=$((size_mb * 1024 * 1024))
    if [ "$KEEP" = "keep" ] && [ -f "$path" ]; then
        local have_bytes
        have_bytes=$(stat -c %s "$path" 2>/dev/null || echo 0)
        if [ "$have_bytes" = "$want_bytes" ]; then
            echo "Reusing existing ${size_mb}MB file at $path (keep mode)"
            return
        fi
    fi
    echo "Creating ${size_mb}MB file at $path ..."
    dd if=/dev/urandom of="$path" bs=1M count="$size_mb" status=none
    sync "$MOUNT"
}

patch_and_time() {
    local path="$1" offset="$2"
    local t0 t1
    t0=$(date +%s.%N)
    dd if=/dev/urandom of="$path" bs=4096 count=1 seek=$((offset / 4096)) conv=notrunc status=none
    sync "$MOUNT"
    t1=$(date +%s.%N)
    echo "$t1 - $t0" | bc
}

run_positions() {
    local path="$1" size_bytes="$2" label="$3"
    local begin=0
    local middle=$(( (size_bytes / 2 / PATCH_SIZE) * PATCH_SIZE ))
    local end=$(( ((size_bytes - PATCH_SIZE) / PATCH_SIZE) * PATCH_SIZE ))

    for pos_name in begin middle end; do
        local off
        case $pos_name in
            begin) off=$begin ;;
            middle) off=$middle ;;
            end) off=$end ;;
        esac
        echo "  [$label] position=$pos_name offset=$off"
        local total=0
        for i in $(seq 1 "$ITERS"); do
            t=$(patch_and_time "$path" "$off")
            printf "    iter %2d: %ss\n" "$i" "$t"
            total=$(echo "$total + $t" | bc)
        done
        avg=$(echo "scale=4; $total / $ITERS" | bc -l)
        printf "  => [%s] %-6s avg over %d iters: %ss\n" "$label" "$pos_name" "$ITERS" "$avg"
        echo "$label,$pos_name,$avg" >> "$RESULTS_FILE"
    done
}

echo "=== 1GB file ==="
create_file "$MOUNT/patch_timing_1g.img" 1024
run_positions "$MOUNT/patch_timing_1g.img" $((1024*1024*1024)) "1GB"

echo ""
echo "=== 8GB file ==="
create_file "$MOUNT/patch_timing_8g.img" 8192
run_positions "$MOUNT/patch_timing_8g.img" $((8192*1024*1024)) "8GB"

echo ""
echo "=== Summary (average patch latency, seconds) ==="
printf "%-6s %-8s %s\n" "size" "position" "avg_secs"
cat "$RESULTS_FILE" | while IFS=, read -r label pos avg; do
    printf "%-6s %-8s %s\n" "$label" "$pos" "$avg"
done

if [ "$KEEP" = "keep" ]; then
    echo ""
    echo "=== Keeping test files (keep mode) ==="
else
    echo ""
    echo "=== Cleanup ==="
    rm -f "$MOUNT/patch_timing_1g.img" "$MOUNT/patch_timing_8g.img"
fi
echo "Done."
