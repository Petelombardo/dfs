#!/bin/bash
# Test script to reproduce SQLite issues on DFS
# This script creates a fresh SQLite database and performs various operations
# to test data integrity and consistency

set -e

# Configuration
TEST_DIR="/mnt/dfs/sqlite_test"
DB_PATH="${TEST_DIR}/test.db"
LOG_FILE="/tmp/sqlite_test_$(date +%Y%m%d_%H%M%S).log"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log() {
    echo -e "${GREEN}[$(date +%H:%M:%S)]${NC} $1" | tee -a "$LOG_FILE"
}

error() {
    echo -e "${RED}[$(date +%H:%M:%S)] ERROR:${NC} $1" | tee -a "$LOG_FILE"
}

warn() {
    echo -e "${YELLOW}[$(date +%H:%M:%S)] WARNING:${NC} $1" | tee -a "$LOG_FILE"
}

# Cleanup function
cleanup() {
    log "Cleaning up test directory..."
    rm -rf "$TEST_DIR"
}

# Set up trap to cleanup on exit
trap cleanup EXIT

# Check if DFS is mounted
if ! mountpoint -q /mnt/dfs; then
    error "DFS is not mounted at /mnt/dfs"
    exit 1
fi

log "Starting SQLite test on DFS"
log "Test directory: $TEST_DIR"
log "Database: $DB_PATH"
log "Log file: $LOG_FILE"

# Create test directory
log "Creating test directory..."
mkdir -p "$TEST_DIR"

# Test 1: Create a fresh database
log ""
log "=== Test 1: Create fresh database ==="
sqlite3 "$DB_PATH" <<EOF
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_users_email ON users(email);

-- Insert some test data
INSERT INTO users (name, email) VALUES
    ('Alice', 'alice@example.com'),
    ('Bob', 'bob@example.com'),
    ('Charlie', 'charlie@example.com');

-- Verify data
SELECT COUNT(*) as count FROM users;
EOF

if [ $? -eq 0 ]; then
    log "✓ Database created successfully"
else
    error "✗ Failed to create database"
    exit 1
fi

# Test 2: Verify database integrity
log ""
log "=== Test 2: Check database integrity ==="
INTEGRITY=$(sqlite3 "$DB_PATH" "PRAGMA integrity_check;")
if [ "$INTEGRITY" = "ok" ]; then
    log "✓ Database integrity check passed"
else
    error "✗ Database integrity check failed: $INTEGRITY"
fi

# Test 3: Concurrent writes with transactions
log ""
log "=== Test 3: Concurrent writes with transactions ==="
for i in {1..10}; do
    sqlite3 "$DB_PATH" <<EOF
BEGIN TRANSACTION;
INSERT INTO users (name, email) VALUES ('User${i}', 'user${i}@example.com');
COMMIT;
EOF
    if [ $? -ne 0 ]; then
        error "✗ Transaction $i failed"
        exit 1
    fi
done
log "✓ All transactions completed"

# Test 4: Verify data after transactions
log ""
log "=== Test 4: Verify data consistency ==="
COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM users;")
log "Total users in database: $COUNT"
if [ "$COUNT" -eq 13 ]; then
    log "✓ Data count is correct (13 users)"
else
    error "✗ Expected 13 users, found $COUNT"
fi

# Test 5: Attach database test (like pihole does)
log ""
log "=== Test 5: Attach database test ==="
DB_TEMP="${TEST_DIR}/temp.db"

# Create temp database
sqlite3 "$DB_TEMP" <<EOF
CREATE TABLE query_storage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    query TEXT NOT NULL
);

INSERT INTO query_storage (timestamp, query) VALUES
    ($(date +%s), 'example.com'),
    ($(date +%s), 'google.com');
EOF

# Attach and copy data
sqlite3 "$DB_PATH" <<EOF
ATTACH DATABASE '$DB_TEMP' AS disk;
CREATE TABLE IF NOT EXISTS query_storage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    query TEXT NOT NULL
);
INSERT INTO query_storage SELECT * FROM disk.query_storage WHERE timestamp > 0;
DETACH DATABASE disk;
SELECT COUNT(*) FROM query_storage;
EOF

if [ $? -eq 0 ]; then
    log "✓ Database attach/detach test passed"
else
    error "✗ Database attach/detach test failed"
fi

# Test 6: Write-heavy workload with fsync
log ""
log "=== Test 6: Write-heavy workload (simulating pihole-FTL) ==="
sqlite3 "$DB_PATH" <<EOF
PRAGMA synchronous = FULL;
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    message TEXT
);

-- Insert 100 records in separate transactions (forces many fsyncs)
EOF

for i in {1..100}; do
    sqlite3 "$DB_PATH" "INSERT INTO logs (timestamp, message) VALUES ($(date +%s), 'Log entry $i');"
done

log "✓ Write-heavy workload completed"

# Test 7: Final integrity check
log ""
log "=== Test 7: Final integrity check ==="
INTEGRITY=$(sqlite3 "$DB_PATH" "PRAGMA integrity_check;")
if [ "$INTEGRITY" = "ok" ]; then
    log "✓ Final database integrity check passed"
else
    error "✗ Final database integrity check failed: $INTEGRITY"
fi

# Test 8: WAL checkpoint
log ""
log "=== Test 8: WAL checkpoint test ==="
sqlite3 "$DB_PATH" <<EOF
PRAGMA wal_checkpoint(FULL);
EOF

if [ $? -eq 0 ]; then
    log "✓ WAL checkpoint successful"
else
    error "✗ WAL checkpoint failed"
fi

# Test 9: Close and reopen database
log ""
log "=== Test 9: Close and reopen database ==="
log "Reading data after close/reopen..."
COUNT=$(sqlite3 "$DB_PATH" "SELECT COUNT(*) FROM users;")
log "User count after reopen: $COUNT"

INTEGRITY=$(sqlite3 "$DB_PATH" "PRAGMA integrity_check;")
if [ "$INTEGRITY" = "ok" ]; then
    log "✓ Database still valid after close/reopen"
else
    error "✗ Database corrupted after close/reopen: $INTEGRITY"
fi

# Summary
log ""
log "==================================="
log "SQLite Test Summary"
log "==================================="
log "All tests completed successfully!"
log "Database location: $DB_PATH"
log "Log file: $LOG_FILE"
log ""
log "To inspect the database manually, run:"
log "  sqlite3 $DB_PATH"
log ""
log "To check integrity:"
log "  sqlite3 $DB_PATH 'PRAGMA integrity_check;'"
