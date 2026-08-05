#!/bin/bash
# Repairs chunk files whose mtime was corrupted by the ms/seconds unit-mismatch
# bug in handle_push_chunk_to (fixed in dfs-server commit 4de0bfe, 2026-08-05).
# Before that fix, every chunk replica created via a healing push
# (PushChunkTo -> ReplicateChunk with a carried-over written_at) got its mtime
# stamped from a millisecond ChunkLocation.written_at value treated as Unix
# SECONDS by storage::set_chunk_mtime — landing hundreds of years in the
# future (confirmed live on staging: chunks written 2026-08-04 got an mtime of
# 2446-05-10). The fix stops new corruption; this script repairs chunk files
# that already got stamped before the fix was deployed.
#
# Strategy: don't try to recompute the "real" written_at from the metadata DB.
# Every corrupted file inspected live had a completely normal, correct birth
# time (statx btime) and ctime — only mtime (the one field the buggy code
# path actually touched) was wrong. So: find any chunk file whose mtime is
# absurdly far in the future, and reset mtime to that same file's own birth
# time (falling back to ctime if the filesystem doesn't report birth time).
# No DB access, no cluster RPCs, no risk of touching anything that wasn't
# actually corrupted — files with a normal mtime are left completely alone.
#
# Run once per storage node, locally, after the mtime fix has been deployed.
#
# Usage: ./fix_future_chunk_mtimes.sh [--dry-run] [chunks_dir]
#   --dry-run     report what would change without touching anything
#   chunks_dir    defaults to /mnt/gluster/dfs/data/chunks

set -uo pipefail

DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
    DRY_RUN=1
    shift
fi
CHUNKS_DIR="${1:-/mnt/gluster/dfs/data/chunks}"

if [ ! -d "$CHUNKS_DIR" ]; then
    echo "ERROR: $CHUNKS_DIR does not exist" >&2
    exit 1
fi

NOW=$(date +%s)
# Anything mtime'd more than a year ahead of "now" cannot be a legitimate
# write (real clock skew is seconds to minutes, not years) — this bug's real
# damage was ~420 years, so 1 year of margin has zero chance of a false hit.
FUTURE_THRESHOLD=$(( NOW + 365*24*3600 ))

CHECKED=0
FIXED=0
SKIPPED=0

fmt_ts() { date -d "@$1" '+%Y-%m-%d %H:%M:%S' 2>/dev/null || date -r "$1" '+%Y-%m-%d %H:%M:%S'; }

echo "Scanning $CHUNKS_DIR for chunk files with a corrupted future mtime..."
echo "(flagging anything mtime'd after $(fmt_ts "$FUTURE_THRESHOLD"))"
[ "$DRY_RUN" -eq 1 ] && echo "--dry-run: no files will be modified"
echo ""

# Let find's own single traversal do the mtime filtering (-newermt matches
# files with mtime strictly after the given timestamp) instead of forking an
# external `stat` process per file just to make the same comparison — on a
# 139k-file directory that fork-per-file cost was the dominant expense,
# competing hard enough with concurrent benchmark I/O to look like a real
# regression (confirmed live 2026-08-05: a dry-run of this loop coincided
# with a ~20% sequential-write drop in a kdiskmark run on the same node).
# find requires GNU findutils' -newermt (confirmed present, same 4.9.0, on
# all 5 nodes) with "@epoch" time syntax. Only files find already confirmed
# are in the future reach the per-file stat/touch work below.
while IFS= read -r -d '' f; do
    CHECKED=$((CHECKED + 1))
    if [ $(( CHECKED % 20000 )) -eq 0 ]; then
        echo "  ...${CHECKED} corrupted candidates processed so far (${FIXED} fixed)"
    fi

    mtime=$(stat -c %Y "$f" 2>/dev/null) || continue

    # Prefer birth time (%W); not every filesystem reports it (0 or empty
    # means unsupported), so fall back to ctime (%Z) — also a real kernel
    # timestamp the buggy code path never touched.
    fallback=$(stat -c %W "$f" 2>/dev/null || echo 0)
    if [ -z "$fallback" ] || [ "$fallback" = "0" ] || [ "$fallback" -gt "$FUTURE_THRESHOLD" ]; then
        fallback=$(stat -c %Z "$f" 2>/dev/null || echo 0)
    fi

    if [ -z "$fallback" ] || [ "$fallback" -le 0 ] || [ "$fallback" -gt "$FUTURE_THRESHOLD" ]; then
        echo "  SKIP (no sane fallback timestamp): $f"
        SKIPPED=$((SKIPPED + 1))
        continue
    fi

    echo "  $f: mtime $(fmt_ts "$mtime") -> $(fmt_ts "$fallback")"
    if [ "$DRY_RUN" -eq 0 ]; then
        if touch -d "@$fallback" "$f" 2>/dev/null; then
            FIXED=$((FIXED + 1))
        else
            echo "    FAILED to touch $f" >&2
            SKIPPED=$((SKIPPED + 1))
        fi
    else
        FIXED=$((FIXED + 1))
    fi
done < <(find "$CHUNKS_DIR" -type f -newermt "@$FUTURE_THRESHOLD" -print0)

echo ""
echo "Corrupted candidates found: $CHECKED   Fixed: $FIXED   Skipped/failed: $SKIPPED"
[ "$DRY_RUN" -eq 1 ] && echo "(dry-run — re-run without --dry-run to actually apply)"
