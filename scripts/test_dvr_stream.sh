#!/bin/bash
# Three-way DVR stream integrity test using /dev/urandom as source.
#   WRITE_REF  = tee copy of what the writer sent (ground truth)
#   DFS_FILE   = what landed on DFS
#   READ_COPY  = what a concurrent reader got from DFS (simulates Kodi read-behind)
#
# Compares all three pairwise to isolate write-path vs read-path corruption.

DFS_MOUNT="/tmp/dfs-mount"
LOCAL_DIR="/tmp/dfs_stream_test"
DFS_FILE="$DFS_MOUNT/stream_test_$$.mpg"
WRITE_REF="$LOCAL_DIR/write_reference.mpg"
DFS_READBACK="$LOCAL_DIR/dfs_readback.mpg"
READ_COPY="$LOCAL_DIR/read_copy.mpg"
CHUNK_SIZE=$((4 * 1024 * 1024))
CAPTURE_SIZE="32M"
RATE="5M"
READ_DELAY=6

mkdir -p "$LOCAL_DIR"
rm -f "$WRITE_REF" "$DFS_READBACK" "$READ_COPY"

echo "DFS file:     $DFS_FILE"
echo "Write ref:    $WRITE_REF"
echo "Read copy:    $READ_COPY"
echo "DFS readback: $DFS_READBACK"
echo "Rate: $RATE  Size: $CAPTURE_SIZE  Read delay: ${READ_DELAY}s"
echo ""

echo "=== Starting writer ==="
cat /dev/urandom \
    | pv -S -s "$CAPTURE_SIZE" -L "$RATE" \
    | tee "$WRITE_REF" > "$DFS_FILE" &
WRITER_PID=$!

echo "Writer PID: $WRITER_PID — waiting ${READ_DELAY}s before reader starts..."
sleep "$READ_DELAY"

echo "=== Starting concurrent reader ==="
(
    BYTES_READ=0
    while kill -0 "$WRITER_PID" 2>/dev/null; do
        DFS_SIZE=$(stat -c%s "$DFS_FILE" 2>/dev/null || echo 0)
        AVAILABLE=$(( DFS_SIZE - BYTES_READ ))
        if [ "$AVAILABLE" -ge 4096 ]; then
            PAGES=$(( AVAILABLE / 4096 ))
            BEFORE=$(stat -c%s "$READ_COPY" 2>/dev/null || echo 0)
            dd if="$DFS_FILE" bs=4096 skip=$(( BYTES_READ / 4096 )) count="$PAGES" 2>/dev/null \
                | pv -q -L "$RATE" >> "$READ_COPY"
            AFTER=$(stat -c%s "$READ_COPY" 2>/dev/null || echo 0)
            ACTUALLY_READ=$(( AFTER - BEFORE ))
            BYTES_READ=$(( BYTES_READ + ACTUALLY_READ ))
        else
            sleep 0.05
        fi
    done
    # Drain tail after writer exits
    sleep 1
    DFS_SIZE=$(stat -c%s "$DFS_FILE" 2>/dev/null || echo 0)
    REMAINING=$(( DFS_SIZE - BYTES_READ ))
    if [ "$REMAINING" -gt 0 ]; then
        dd if="$DFS_FILE" bs=4096 skip=$(( BYTES_READ / 4096 )) \
           count=$(( (REMAINING + 4095) / 4096 )) 2>/dev/null >> "$READ_COPY"
    fi
) &
READER_PID=$!

wait "$WRITER_PID"
echo ""
echo "=== Writer done. Waiting for reader... ==="
wait "$READER_PID"

echo "=== Reading back DFS file in full (post-write) ==="
cp "$DFS_FILE" "$DFS_READBACK"
rm -f "$DFS_FILE"

echo ""
WRITE_SIZE=$(stat -c%s "$WRITE_REF")
READ_SIZE=$(stat -c%s "$READ_COPY")
DFS_SIZE=$(stat -c%s "$DFS_READBACK")
echo "Write ref size:    $(( WRITE_SIZE / 1024 / 1024 ))MB"
echo "DFS readback size: $(( DFS_SIZE / 1024 / 1024 ))MB"
echo "Read copy size:    $(( READ_SIZE / 1024 / 1024 ))MB"
echo ""

compare_files() {
    local label="$1" fa="$2" fb="$3"
    local sa=$(stat -c%s "$fa") sb=$(stat -c%s "$fb")
    local cmp_size=$(( sa < sb ? sa : sb ))
    cmp_size=$(( (cmp_size / CHUNK_SIZE) * CHUNK_SIZE ))
    local MA=$(dd if="$fa" bs="$CHUNK_SIZE" count=$(( cmp_size / CHUNK_SIZE )) 2>/dev/null | md5sum | cut -d' ' -f1)
    local MB=$(dd if="$fb" bs="$CHUNK_SIZE" count=$(( cmp_size / CHUNK_SIZE )) 2>/dev/null | md5sum | cut -d' ' -f1)
    echo "--- $label ($(( cmp_size / 1024 / 1024 ))MB compared) ---"
    if [ "$MA" = "$MB" ]; then
        echo "  PASS: identical"
        return 0
    fi
    echo "  FAIL: mismatch — scanning chunks..."
    local chunks=$(( cmp_size / CHUNK_SIZE ))
    local first_bad=""
    for i in $(seq 0 $(( chunks - 1 ))); do
        local W=$(dd if="$fa" bs="$CHUNK_SIZE" skip="$i" count=1 2>/dev/null | md5sum | cut -d' ' -f1)
        local R=$(dd if="$fb" bs="$CHUNK_SIZE" skip="$i" count=1 2>/dev/null | md5sum | cut -d' ' -f1)
        local off_mb=$(( i * CHUNK_SIZE / 1024 / 1024 ))
        if [ "$W" = "$R" ]; then
            printf "    chunk %3d (%4dMB): OK\n" "$i" "$off_mb"
        else
            printf "    chunk %3d (%4dMB): MISMATCH\n" "$i" "$off_mb"
            first_bad="${first_bad:-$i}"
        fi
    done
    if [ -n "$first_bad" ]; then
        echo ""
        echo "  First divergence: chunk $first_bad ($(( first_bad * CHUNK_SIZE / 1024 / 1024 ))MB)"
        echo "  Scanning 4096-byte pages within chunk $first_bad..."
        for page in $(seq 0 1023); do
            local skip=$(( first_bad * 1024 + page ))
            local W=$(dd if="$fa" bs=4096 skip="$skip" count=1 2>/dev/null | md5sum | cut -d' ' -f1)
            local R=$(dd if="$fb" bs=4096 skip="$skip" count=1 2>/dev/null | md5sum | cut -d' ' -f1)
            if [ "$W" != "$R" ]; then
                echo "  First bad 4096-byte page: $page"
                echo "  Byte offset: $(( first_bad * CHUNK_SIZE + page * 4096 ))"
                break
            fi
        done
    fi
    return 1
}

RESULT=0
echo "=== Comparison 1: write path (write_ref vs dfs_readback) ==="
compare_files "write_ref vs dfs_readback" "$WRITE_REF" "$DFS_READBACK" || RESULT=1
echo ""
echo "=== Comparison 2: live read path (write_ref vs read_copy) ==="
compare_files "write_ref vs read_copy" "$WRITE_REF" "$READ_COPY" || RESULT=1
echo ""
echo "=== Comparison 3: dfs vs concurrent read (dfs_readback vs read_copy) ==="
compare_files "dfs_readback vs read_copy" "$DFS_READBACK" "$READ_COPY" || RESULT=1
echo ""
echo "Done. Files in $LOCAL_DIR"
exit $RESULT
