#!/bin/bash
# Reproduce (or clear) the 2026-07-29 VM-111 "bad shim signature" silent-corruption
# incident: a fresh Debian install completed successfully but wouldn't boot. Forensics
# on the actual staging disk (qemu-nbd read-only mount + `dpkg -V`) confirmed TWO
# unrelated files were silently content-corrupted during the install (vmlinuz +
# an unrelated /usr/share/doc file from a different package) — same size, valid
# headers, wrong bytes. ext4 metadata (fsck) stayed clean throughout, since fsck
# never checks file content. See memory: project_vm111_bad_shim_signature_20260729.md
#
# What a real OS install actually does to the filesystem, that this mirrors:
# thousands of small files (deb package contents, mostly under /usr/share/doc,
# /usr/share/man, etc) opened/written/closed in rapid succession, INTERLEAVED with
# a couple of large files (vmlinuz ~12MB, initrd ~36MB) — all under real concurrency
# (dpkg trigger processing, not one file at a time). This script builds that same
# write shape from real files already on this box (no network/debootstrap dependency)
# against BOTH a plain local directory (control — must always be clean) and a local
# 5-node DFS cluster, then verifies every single byte via sha256 manifests. Any file
# that differs from the pristine source, on the DFS side but not the control side,
# is a genuine DFS write-path corruption — not a coincidence, not a test bug.
#
# Usage: bash scripts/repro_vm_install_byte_correctness.sh [iterations] [workers]

set -u

REPO=$(cd "$(dirname "$0")/.." && pwd)
BASE=/tmp/dfs-test
MOUNT=/tmp/dfs-vminstall-mount
CONTROL=/tmp/dfs-vminstall-control
LOG=/tmp/dfs-vminstall-logs
CLUSTER="127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902,127.0.0.1:8903,127.0.0.1:8904"
BIN="$REPO/target/release"

ITERATIONS=${1:-1}
WORKERS=${2:-16}

# Real source material: /usr/share/doc gives ~5800 small files (~97MB) matching the
# doc-file corruption seen on VM-111. The four large files stand in for vmlinuz/initrd
# (12-30MB range, same order of magnitude), copied under a boot/ prefix so they get
# interleaved with the small-file burst instead of segregated at the start or end.
SRC=/usr/share/doc
LARGE_FILES=(
    /usr/bin/gdb
    /builds/dfs/target/release/dfs-server
    /usr/bin/qemu-aarch64_be
    /usr/bin/aarch64-linux-gnu-lto-dump-14
)

cleanup_all() {
    pkill -f "dfs-server start --config $BASE" 2>/dev/null || true
    pkill -f "dfs-client mount $MOUNT" 2>/dev/null || true
    sleep 0.5
    fusermount -u "$MOUNT" 2>/dev/null || true
}

echo "=== Cleaning up any previous run ==="
cleanup_all
rm -rf "$MOUNT" "$CONTROL" "$LOG" 2>/dev/null || true
mkdir -p "$MOUNT" "$CONTROL" "$LOG"

echo "=== Starting 5-node cluster ==="
cd "$REPO"
bash "$REPO/scripts/setup-cluster.sh" 5 2>/dev/null
for i in 1 2 3 4 5; do
    RUST_LOG=info "$BIN/dfs-server" start --config "$BASE/node${i}/config.toml" \
        > "$LOG/server${i}.log" 2>&1 &
done
sleep 3

env RUST_LOG=debug "$BIN/dfs-client" mount "$MOUNT" --cluster "$CLUSTER" \
    --log-file "$LOG/client.log" --allow-other --log-level debug &
sleep 2
mountpoint -q "$MOUNT" || { echo "MOUNT FAILED"; tail -30 "$LOG/client.log"; exit 1; }
echo "Mounted."

echo "=== Building file list (real files from $SRC + ${#LARGE_FILES[@]} large stand-ins) ==="
( cd "$SRC" && find . -type f | sed 's|^\./||' ) > "$LOG/filelist_small.txt"
NUM_SMALL=$(wc -l < "$LOG/filelist_small.txt")
> "$LOG/filelist_full.txt"
> "$LOG/large_map.txt"
i=0
for lf in "${LARGE_FILES[@]}"; do
    rel="boot/large_${i}_$(basename "$lf")"
    echo "$rel" >> "$LOG/filelist_full.txt"
    printf '%s\t%s\n' "$rel" "$lf" >> "$LOG/large_map.txt"
    i=$((i + 1))
done
cat "$LOG/filelist_small.txt" >> "$LOG/filelist_full.txt"
shuf "$LOG/filelist_full.txt" -o "$LOG/filelist_full.txt"
echo "$NUM_SMALL small files + ${#LARGE_FILES[@]} large files = $(wc -l < "$LOG/filelist_full.txt") total"

# Bash arrays can't be exported to xargs's child subshells, so large-file source
# paths are looked up from this plain map file instead (avoids a real bug found in
# an earlier version of this script: silent empty-path `cp` failures).
export LARGE_MAP="$LOG/large_map.txt"
export SRC

resolve_src() {
    local rel="$1"
    if [[ "$rel" == boot/large_* ]]; then
        grep -P "^${rel}\t" "$LARGE_MAP" | cut -f2
    else
        echo "$SRC/$rel"
    fi
}
export -f resolve_src

copy_one() {
    local rel="$1" destroot="$2"
    local realsrc
    realsrc=$(resolve_src "$rel")
    local dest="$destroot/$rel"
    mkdir -p "$(dirname "$dest")"
    cp "$realsrc" "$dest"
}
export -f copy_one

FAIL=0
for iter in $(seq 1 "$ITERATIONS"); do
    echo
    echo "=== Iteration $iter/$ITERATIONS ==="
    DFS_DEST="$MOUNT/install_$iter"
    CTRL_DEST="$CONTROL/install_$iter"
    mkdir -p "$DFS_DEST" "$CTRL_DEST"

    echo "--- Writing $(wc -l < "$LOG/filelist_full.txt") files concurrently to DFS and to plain local disk ($WORKERS workers each) ---"
    T0=$(date +%s)
    ( cat "$LOG/filelist_full.txt" | xargs -P "$WORKERS" -I{} bash -c 'copy_one "$1" "$2"' _ {} "$DFS_DEST" ) &
    DFS_PID=$!
    ( cat "$LOG/filelist_full.txt" | xargs -P "$WORKERS" -I{} bash -c 'copy_one "$1" "$2"' _ {} "$CTRL_DEST" ) &
    CTRL_PID=$!
    wait "$DFS_PID" "$CTRL_PID"
    T1=$(date +%s)
    echo "Write phase done in $((T1 - T0))s. Syncing DFS mount..."
    sync "$MOUNT"
    sleep 1

    echo "--- Verifying byte-for-byte against pristine source ---"
    # Manifests are written as "path<TAB>hash" (path first) so they can be joined by
    # path below — a positional paste/diff would silently misalign every row after
    # the first missing/extra file (e.g. one dropped by a real ENOENT/EIO), instead
    # of reporting that specific file as missing.
    manifest() {
        local root="$1"
        # sha256sum's format is fixed-width: 64 hex chars, a space, a mode flag
        # (space/*), then the filename — sliced positionally so filenames
        # containing spaces don't get mis-split by awk field splitting.
        ( cd "$root" && find . -type f -print0 | sort -z | xargs -0 sha256sum ) \
            | awk '{print substr($0,67)"\t"substr($0,1,64)}' | sort -k1,1
    }
    ref_manifest() {
        while IFS= read -r rel; do
            local realsrc
            realsrc=$(resolve_src "$rel")
            printf './%s\t%s\n' "$rel" "$(sha256sum "$realsrc" | awk '{print $1}')"
        done < "$LOG/filelist_full.txt" | sort -k1,1
    }

    ref_manifest > "$LOG/ref_${iter}.tsv"
    manifest "$DFS_DEST" > "$LOG/dfs_${iter}.tsv"
    manifest "$CTRL_DEST" > "$LOG/ctrl_${iter}.tsv"

    # join -t $'\t' -a1 -a2 keeps rows unique to either side (missing-file cases)
    # as well as rows present in both, so nothing is silently dropped.
    CTRL_ISSUES=$(join -t $'\t' -a1 -a2 -e MISSING -o 0,1.2,2.2 "$LOG/ref_${iter}.tsv" "$LOG/ctrl_${iter}.tsv" | awk -F'\t' '$2 != $3')
    if [[ -n "$CTRL_ISSUES" ]]; then
        echo "!! CONTROL (plain local disk) copy itself doesn't match source — test harness bug, not a DFS finding. Investigate before trusting this run:"
        echo "$CTRL_ISSUES" | head -20
        FAIL=1
        continue
    fi

    DFS_ISSUES=$(join -t $'\t' -a1 -a2 -e MISSING -o 0,1.2,2.2 "$LOG/ref_${iter}.tsv" "$LOG/dfs_${iter}.tsv" | awk -F'\t' '$2 != $3')
    if [[ -z "$DFS_ISSUES" ]]; then
        echo "PASS: all $(wc -l < "$LOG/ref_${iter}.tsv") files byte-identical on DFS, iteration $iter."
    else
        FAIL=1
        N_BAD=$(echo "$DFS_ISSUES" | grep -c .)
        echo "FAIL: $N_BAD file(s) wrong on DFS in iteration $iter (control copy was clean):"
        while IFS=$'\t' read -r rel exp_hash act_hash; do
            rel="${rel#./}"
            if [[ "$act_hash" == "MISSING" ]]; then
                echo "  $rel  MISSING on DFS (expected hash $exp_hash)"
            else
                exp_size=$(stat -c%s "$(resolve_src "$rel")" 2>/dev/null)
                act_size=$(stat -c%s "$DFS_DEST/$rel" 2>/dev/null)
                echo "  $rel  content mismatch (expected size=$exp_size, actual size=$act_size)"
            fi
        done <<< "$DFS_ISSUES"
        echo "  Full manifests: $LOG/ref_${iter}.tsv vs $LOG/dfs_${iter}.tsv"
        echo "  Client log: $LOG/client.log ; server logs: $LOG/server{1..5}.log"
    fi
done

echo
if [[ "$FAIL" -eq 0 ]]; then
    echo "=== RESULT: PASS — no corruption reproduced across $ITERATIONS iteration(s) ==="
else
    echo "=== RESULT: FAIL — corruption reproduced, see details above ==="
fi

echo "=== Leaving cluster running for investigation. Run cleanup manually: ==="
echo "  pkill -f 'dfs-server start --config $BASE'; fusermount -u $MOUNT"

exit "$FAIL"
