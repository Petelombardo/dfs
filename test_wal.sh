#!/bin/bash
# SQLite WAL mode stress test on DFS
# Tests the specific failure modes most likely to break on a distributed filesystem:
# - WAL file creation and management
# - Shared memory (-shm) file coordination
# - Checkpoint behavior
# - Data durability across open/close cycles
# - Concurrent readers + writer
# - Recovery after simulated crash (no checkpoint)

set -euo pipefail

DB_DIR="/mnt/dfs/sqlite_wal_test"
DB="$DB_DIR/test.db"
PASS=0
FAIL=0
WARN=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { echo -e "${GREEN}PASS${NC} $1"; ((PASS++)); }
fail() { echo -e "${RED}FAIL${NC} $1"; ((FAIL++)); }
warn() { echo -e "${YELLOW}WARN${NC} $1"; WARN=$((WARN+1)); }
section() { echo ""; echo "--- $1 ---"; }

cleanup() {
    rm -rf "$DB_DIR"
}
trap cleanup EXIT

mkdir -p "$DB_DIR"

# ─── TEST 1: WAL mode setup and file creation ───────────────────────────────
section "Test 1: WAL mode file creation"
sqlite3 "$DB" "PRAGMA journal_mode=WAL;" > /dev/null
sqlite3 "$DB" "CREATE TABLE t (id INTEGER PRIMARY KEY, val TEXT);"

ls "$DB_DIR/"
if [ -f "${DB}-wal" ]; then
    pass "WAL file exists after write"
else
    warn "WAL file not found (may have been checkpointed immediately)"
fi
if [ -f "${DB}-shm" ]; then
    pass "SHM file exists"
else
    warn "SHM file not found"
fi

# ─── TEST 2: WAL durability — data visible in new connection ─────────────────
section "Test 2: WAL durability across connections"
sqlite3 "$DB" "INSERT INTO t VALUES (1, 'hello');"
sqlite3 "$DB" "INSERT INTO t VALUES (2, 'world');"

COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM t;")
if [ "$COUNT" -eq 2 ]; then
    pass "Rows visible in same connection"
else
    fail "Expected 2 rows, got $COUNT"
fi

# Open fresh connection — WAL reader must replay WAL
COUNT2=$(sqlite3 "$DB" "SELECT COUNT(*) FROM t;")
if [ "$COUNT2" -eq 2 ]; then
    pass "Rows visible in fresh connection (WAL replay OK)"
else
    fail "Fresh connection: expected 2 rows, got $COUNT2"
fi

# ─── TEST 3: Writes accumulate in WAL, checkpoint flushes ───────────────────
section "Test 3: WAL checkpoint"
# Write enough to ensure WAL has content
for i in $(seq 3 50); do
    sqlite3 "$DB" "INSERT INTO t VALUES ($i, 'val$i');"
done

WAL_SIZE_BEFORE=$(stat -c%s "${DB}-wal" 2>/dev/null || echo 0)
echo "WAL size before checkpoint: $WAL_SIZE_BEFORE bytes"

sqlite3 "$DB" "PRAGMA wal_checkpoint(FULL);"

WAL_SIZE_AFTER=$(stat -c%s "${DB}-wal" 2>/dev/null || echo 0)
echo "WAL size after checkpoint: $WAL_SIZE_AFTER bytes"

COUNT3=$(sqlite3 "$DB" "SELECT COUNT(*) FROM t;")
if [ "$COUNT3" -eq 50 ]; then
    pass "All 50 rows present after checkpoint ($COUNT3)"
else
    fail "After checkpoint: expected 50 rows, got $COUNT3"
fi

# ─── TEST 4: Integrity check ─────────────────────────────────────────────────
section "Test 4: Integrity check"
INTEGRITY=$(sqlite3 "$DB" "PRAGMA integrity_check;")
if [ "$INTEGRITY" = "ok" ]; then
    pass "integrity_check: ok"
else
    fail "integrity_check: $INTEGRITY"
fi

# ─── TEST 5: Index behavior in WAL mode ──────────────────────────────────────
section "Test 5: Index + WAL"
DB2="$DB_DIR/indexed.db"
sqlite3 "$DB2" "PRAGMA journal_mode=WAL;"
sqlite3 "$DB2" <<'EOF'
CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, score INTEGER);
CREATE INDEX idx_name ON items(name);
CREATE INDEX idx_score ON items(score);
INSERT INTO items VALUES (1, 'alpha', 10);
INSERT INTO items VALUES (2, 'beta', 20);
INSERT INTO items VALUES (3, 'gamma', 30);
EOF

# Read back via index in fresh connection
NAME=$(sqlite3 "$DB2" "SELECT name FROM items WHERE score=20;")
if [ "$NAME" = "beta" ]; then
    pass "Index lookup via fresh connection correct"
else
    fail "Index lookup: expected 'beta', got '$NAME'"
fi

IC=$(sqlite3 "$DB2" "PRAGMA integrity_check;")
if [ "$IC" = "ok" ]; then
    pass "Indexed DB integrity_check: ok"
else
    fail "Indexed DB integrity_check: $IC"
fi

# ─── TEST 6: Concurrent reader + writer (background writer) ──────────────────
section "Test 6: Concurrent reader + writer"
DB3="$DB_DIR/concurrent.db"
sqlite3 "$DB3" "PRAGMA journal_mode=WAL; CREATE TABLE c (id INTEGER PRIMARY KEY, v TEXT);"

# Background writer inserts 200 rows
(
    for i in $(seq 1 200); do
        sqlite3 "$DB3" "INSERT INTO c VALUES ($i, 'row$i');" 2>/dev/null || true
        sleep 0.01
    done
) &
WRITER_PID=$!

# Concurrent reader — just shouldn't crash or corrupt
sleep 0.5
READ_COUNT=$(sqlite3 "$DB3" "SELECT COUNT(*) FROM c;" 2>/dev/null || echo "ERROR")
echo "Mid-write read count: $READ_COUNT"

wait $WRITER_PID

FINAL_COUNT=$(sqlite3 "$DB3" "SELECT COUNT(*) FROM c;")
if [ "$FINAL_COUNT" -eq 200 ]; then
    pass "Concurrent writes: all 200 rows present"
else
    fail "Concurrent writes: expected 200 rows, got $FINAL_COUNT"
fi

IC3=$(sqlite3 "$DB3" "PRAGMA integrity_check;")
if [ "$IC3" = "ok" ]; then
    pass "Concurrent DB integrity_check: ok"
else
    fail "Concurrent DB integrity_check: $IC3"
fi

# ─── TEST 7: WAL left dirty — recovery on next open ─────────────────────────
section "Test 7: Dirty WAL recovery (no checkpoint before close)"
DB4="$DB_DIR/dirty_wal.db"
sqlite3 "$DB4" "PRAGMA journal_mode=WAL; CREATE TABLE d (id INTEGER PRIMARY KEY, v TEXT);"

# Write without checkpointing — WAL stays dirty
for i in $(seq 1 20); do
    sqlite3 "$DB4" "INSERT INTO d VALUES ($i, 'dirty$i');"
done

WAL4=$(stat -c%s "${DB4}-wal" 2>/dev/null || echo 0)
echo "Dirty WAL size: $WAL4 bytes"

# New connection must auto-recover from WAL
COUNT4=$(sqlite3 "$DB4" "SELECT COUNT(*) FROM d;")
if [ "$COUNT4" -eq 20 ]; then
    pass "Dirty WAL recovery: all 20 rows readable"
else
    fail "Dirty WAL recovery: expected 20 rows, got $COUNT4"
fi

IC4=$(sqlite3 "$DB4" "PRAGMA integrity_check;")
if [ "$IC4" = "ok" ]; then
    pass "Dirty WAL DB integrity_check: ok"
else
    fail "Dirty WAL DB integrity_check: $IC4"
fi

# ─── TEST 8: Large transaction (forces WAL growth) ───────────────────────────
section "Test 8: Large WAL transaction"
DB5="$DB_DIR/large_wal.db"
sqlite3 "$DB5" "PRAGMA journal_mode=WAL;"
sqlite3 "$DB5" <<'EOF'
CREATE TABLE big (id INTEGER PRIMARY KEY, blob TEXT);
BEGIN;
EOF

# Single big transaction
python3 -c "
import subprocess, sys
rows = '\n'.join(f\"INSERT INTO big VALUES ({i}, '{'x'*1000}');\" for i in range(1, 1001))
sql = f'PRAGMA journal_mode=WAL;\nBEGIN;\n{rows}\nCOMMIT;'
r = subprocess.run(['sqlite3', sys.argv[1]], input=sql, capture_output=True, text=True)
print(r.stdout, r.stderr)
" "$DB5"

COUNT5=$(sqlite3 "$DB5" "SELECT COUNT(*) FROM big;")
if [ "$COUNT5" -eq 1000 ]; then
    pass "Large WAL transaction: all 1000 rows committed"
else
    fail "Large WAL transaction: expected 1000 rows, got $COUNT5"
fi

IC5=$(sqlite3 "$DB5" "PRAGMA integrity_check;")
if [ "$IC5" = "ok" ]; then
    pass "Large WAL DB integrity_check: ok"
else
    fail "Large WAL DB integrity_check: $IC5"
fi

# ─── Summary ─────────────────────────────────────────────────────────────────
echo ""
echo "============================================"
echo "  Results: PASS=$PASS  FAIL=$FAIL  WARN=$WARN"
echo "============================================"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
