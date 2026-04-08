#!/bin/bash
# Test SQLite database on sparse-file-enabled DFS

set -e

MOUNT_POINT="/tmp/dfs-test-mount"
DB_FILE="$MOUNT_POINT/test.db"

echo "=== Testing SQLite on Sparse-File DFS ==="
echo ""

# Check if mounted
if ! mountpoint -q "$MOUNT_POINT"; then
    echo "ERROR: DFS not mounted at $MOUNT_POINT"
    exit 1
fi

# Remove old database
rm -f "$DB_FILE"

# Create a simple database
echo "Creating SQLite database..."
sqlite3 "$DB_FILE" <<EOF
CREATE TABLE users (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL
);

INSERT INTO users (name, email) VALUES ('Alice', 'alice@example.com');
INSERT INTO users (name, email) VALUES ('Bob', 'bob@example.com');
INSERT INTO users (name, email) VALUES ('Charlie', 'charlie@example.com');
EOF

echo "✓ Database created successfully"

# Query the database
echo ""
echo "Querying database..."
RESULT=$(sqlite3 "$DB_FILE" "SELECT COUNT(*) FROM users;")
echo "User count: $RESULT"

if [ "$RESULT" -eq 3 ]; then
    echo "✓ Database query successful"
else
    echo "✗ FAILED: Expected 3 users, got $RESULT"
    exit 1
fi

# Add more data
echo ""
echo "Adding more users..."
sqlite3 "$DB_FILE" <<EOF
INSERT INTO users (name, email) VALUES ('David', 'david@example.com');
INSERT INTO users (name, email) VALUES ('Eve', 'eve@example.com');
EOF

# Verify
RESULT=$(sqlite3 "$DB_FILE" "SELECT COUNT(*) FROM users;")
echo "User count after insert: $RESULT"

if [ "$RESULT" -eq 5 ]; then
    echo "✓ Insert successful"
else
    echo "✗ FAILED: Expected 5 users, got $RESULT"
    exit 1
fi

# Test UPDATE
echo ""
echo "Updating user..."
sqlite3 "$DB_FILE" "UPDATE users SET email='alice.updated@example.com' WHERE name='Alice';"

UPDATED_EMAIL=$(sqlite3 "$DB_FILE" "SELECT email FROM users WHERE name='Alice';")
echo "Alice's email: $UPDATED_EMAIL"

if [ "$UPDATED_EMAIL" = "alice.updated@example.com" ]; then
    echo "✓ Update successful"
else
    echo "✗ FAILED: Update didn't work"
    exit 1
fi

# Test DELETE
echo ""
echo "Deleting user..."
sqlite3 "$DB_FILE" "DELETE FROM users WHERE name='Bob';"

RESULT=$(sqlite3 "$DB_FILE" "SELECT COUNT(*) FROM users;")
echo "User count after delete: $RESULT"

if [ "$RESULT" -eq 4 ]; then
    echo "✓ Delete successful"
else
    echo "✗ FAILED: Expected 4 users after delete, got $RESULT"
    exit 1
fi

# Check database integrity
echo ""
echo "Checking database integrity..."
INTEGRITY=$(sqlite3 "$DB_FILE" "PRAGMA integrity_check;")

if [ "$INTEGRITY" = "ok" ]; then
    echo "✓ Database integrity check PASSED"
else
    echo "✗ FAILED: Integrity check failed: $INTEGRITY"
    exit 1
fi

# Cleanup
rm -f "$DB_FILE"

echo ""
echo "=== All SQLite Tests Passed! ==="
echo ""
echo "SQLite works correctly on sparse-file-enabled DFS:"
echo "  ✓ CREATE TABLE"
echo "  ✓ INSERT"
echo "  ✓ SELECT"
echo "  ✓ UPDATE"
echo "  ✓ DELETE"
echo "  ✓ Database integrity verified"
