#!/bin/bash
set -e

echo "=== Building ==="
bash build.sh > /dev/null 2>&1

echo "=== Cleaning up old cluster ==="
pkill -9 dfs-server || true
pkill -9 dfs-client || true
fusermount -u /tmp/dfs-mount 2>/dev/null || true
sudo rm -rf /tmp/dfs-test /tmp/dfs-mount
sleep 1

echo "=== Starting 3-node cluster ==="
./scripts/setup-cluster.sh 3 > /dev/null 2>&1
sleep 2

for i in 1 2 3; do
    RUST_LOG=error ./target/release/dfs-server start --config /tmp/dfs-test/node$i/config.toml > /dev/null 2>&1 &
done
sleep 2

mkdir -p /tmp/dfs-mount
RUST_LOG=error ./target/release/dfs-client mount /tmp/dfs-mount \
    --cluster 127.0.0.1:8900,127.0.0.1:8901,127.0.0.1:8902 \
    --allow-other > /dev/null 2>&1 &
sleep 2

echo "=== Creating test files ==="
# Create 200MB file (50 chunks of 4MB)
dd if=/dev/urandom of=/tmp/dfs-mount/test_200mb.bin bs=1M count=200 2>/dev/null
echo "Created 200MB test file"

sync
sleep 1

echo ""
echo "=== Read Performance Tests ==="
echo ""

# Test 1: Cold read (drop all caches)
echo "Test 1: Cold read (first time, no cache)"
sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
time dd if=/tmp/dfs-mount/test_200mb.bin of=/dev/null bs=1M 2>&1 | grep -E "copied|bytes"

sleep 1

# Test 2: Warm client cache
echo ""
echo "Test 2: Warm cache (second read, client cache hot)"
time dd if=/tmp/dfs-mount/test_200mb.bin of=/dev/null bs=1M 2>&1 | grep -E "copied|bytes"

sleep 1

# Test 3: Cold kernel cache, warm Moka cache
echo ""
echo "Test 3: Drop kernel cache only (Moka chunk cache still hot)"
sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
time dd if=/tmp/dfs-mount/test_200mb.bin of=/dev/null bs=1M 2>&1 | grep -E "copied|bytes"

sleep 1

# Test 4: Different block sizes
echo ""
echo "Test 4: Read with 4MB blocks (aligned with chunk size)"
sync
sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
time dd if=/tmp/dfs-mount/test_200mb.bin of=/dev/null bs=4M 2>&1 | grep -E "copied|bytes"

echo ""
echo "=== Cleanup ==="
fusermount -u /tmp/dfs-mount
pkill -9 dfs-server
pkill -9 dfs-client
sudo rm -rf /tmp/dfs-test /tmp/dfs-mount

echo ""
echo "=== Baseline established ==="
