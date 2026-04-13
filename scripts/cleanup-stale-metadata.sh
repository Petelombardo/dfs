#!/usr/bin/env bash
# cleanup-stale-metadata.sh
# Purges stale duplicate file metadata records from the DFS cluster.
#
# For each path that has multiple metadata records, keeps only the newest
# (by modified_at timestamp) and purges the rest by file ID.
#
# Usage: ./scripts/cleanup-stale-metadata.sh [server_addr]
#   server_addr: optional, e.g. 10.25.1.58:8900 (auto-detected if omitted)

set -euo pipefail

SERVER_ARGS=""
if [ -n "${1:-}" ]; then
    SERVER_ARGS="-c $1"
fi

DFS_ADMIN="dfs-admin $SERVER_ARGS"

echo "=== DFS Stale Metadata Cleanup ==="
echo ""

# Fetch full file list
echo "Fetching file list..."
FILE_LIST=$($DFS_ADMIN file list 2>/dev/null | grep -v "^Auto\|^All\|^===\|^Total\|^File ID\|^---\|^$") || {
    echo "ERROR: Failed to fetch file list. Is dfs-admin in PATH and cluster reachable?"
    exit 1
}

TOTAL=$(echo "$FILE_LIST" | wc -l)
UNIQUE=$(echo "$FILE_LIST" | awk '{print $2}' | sort -u | wc -l)
STALE=$((TOTAL - UNIQUE))

echo "Total records:    $TOTAL"
echo "Unique paths:     $UNIQUE"
echo "Stale duplicates: $STALE"
echo ""

if [ "$STALE" -eq 0 ]; then
    echo "Nothing to clean up."
    exit 0
fi

# For each path, keep newest record (sort by modified_at desc), collect the rest
STALE_IDS=$(echo "$FILE_LIST" | \
    awk '{print $1, $2, $NF}' | \
    sort -k2,2 -k3,3rn | \
    awk '{
        id = $1; path = $2
        if (seen[path]) { print id }
        else { seen[path] = 1 }
    }')

echo "Purging $STALE stale records..."
echo ""

count=0
fail=0
while IFS= read -r id; do
    if $DFS_ADMIN file purge-by-id --yes "$id" > /dev/null 2>&1; then
        count=$((count + 1))
    else
        echo "  WARN: Failed to purge $id"
        fail=$((fail + 1))
    fi
    if [ $((count % 100)) -eq 0 ] && [ "$count" -gt 0 ]; then
        echo "  Purged $count / $STALE..."
    fi
done <<< "$STALE_IDS"

echo ""
echo "Done: purged $count records, $fail failures."
echo ""

# Final state
REMAINING=$($DFS_ADMIN file list 2>/dev/null | grep -v "^Auto\|^All\|^===\|^Total\|^File ID\|^---\|^$" | wc -l)
echo "Remaining records: $REMAINING (was $TOTAL)"

HEALING=$($DFS_ADMIN healing status 2>/dev/null | grep "Pending Count" | awk '{print $NF}')
echo "Healing pending:   $HEALING"
