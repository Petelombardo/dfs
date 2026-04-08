#!/bin/bash
# Test SQL metadata store with sparse file operations

set -e

echo "=== Testing SQL Metadata Store ==="
echo ""

# Build first
echo "Building project..."
~/.cargo/bin/cargo build --release -p dfs-server

echo ""
echo "Running SQL metadata tests..."
~/.cargo/bin/cargo test -p dfs-server metadata_sql::tests -- --nocapture

echo ""
echo "=== Test Results ==="
echo "✓ Basic operations (store/retrieve file metadata)"
echo "✓ Sparse file lookup (find chunk at offset, detect holes)"
echo ""
echo "The SQL metadata store is working correctly!"
echo ""
echo "Next steps:"
echo "1. Integrate with dfs-server (run alongside bincode)"
echo "2. Test with real filesystem operations"
echo "3. Implement sparse file read/write in FUSE layer"
