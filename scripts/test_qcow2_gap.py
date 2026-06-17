#!/usr/bin/env python3
"""
Simulate the qcow2 sparse-write + gap-read pattern that causes bad block bitmap checksum.

The bug hypothesis: after a FRESH WRITE with sparse dirty_ranges (like qcow2 L2 table writes),
zero_gap entries are seeded for the gaps. If a SUBSEQUENT WRITE then fills part of a gap,
and a READ comes before the zero_gap is cleared, the read returns zeros instead of written data.

This test:
1. Writes sparse data to a file (simulating qcow2 preallocation=metadata L2 table writes)
2. Fsyncs (triggers flush + zero_gap seeding)
3. Writes to a gap region (simulating mkfs data cluster write)
4. Reads back the gap region IMMEDIATELY (before flush)
5. Reads back again AFTER fsync
6. Verifies all reads return the correct (non-zero) data
"""

import os
import sys
import struct
import time

MOUNT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/dfs-mount"
TEST_FILE = os.path.join(MOUNT, "gap_test_file")

# Pattern from the actual log: chunk=1 (file_offset=4194304) with these dirty_ranges:
# [(0,1396736),(1400832,1404928),(1409024,1413120),(1417216,1421312),
#  (1425408,1429504),(1454080,1458176),(1519616,1523712)]
#
# The gaps are:
# (1396736, 1400832) = 4KB gap at absolute file offsets 5591040-5595136
# (1404928, 1409024) = 4KB gap
# (1413120, 1417216) = 4KB gap
# (1421312, 1425408) = 4KB gap
# (1429504, 1454080) = 24KB gap
# (1458176, 1519616) = 60KB gap
#
# We'll simulate this with a smaller version for speed.

CHUNK_SIZE = 4 * 1024 * 1024  # 4MB DFS chunk

# Simulate sparse dirty_ranges: write a big region, then a few small ones with gaps
# This mimics qcow2 writing L2 table clusters at specific offsets with gaps between them

# Simplified pattern (fits in 128KB for quick testing):
# Write at [0, 64K], [72K, 76K], [80K, 84K] — gaps at [64K, 72K] and [76K, 80K]
RANGE1_START = 0
RANGE1_END = 64 * 1024  # 64KB
RANGE2_START = 72 * 1024  # 72KB
RANGE2_END = 76 * 1024  # 76KB
RANGE3_START = 80 * 1024  # 80KB
RANGE3_END = 84 * 1024  # 84KB

GAP1_START = RANGE1_END  # 64KB
GAP1_END = RANGE2_START  # 72KB = 8KB gap

GAP2_START = RANGE2_END  # 76KB
GAP2_END = RANGE3_START  # 80KB = 4KB gap

def verify_read(f, offset, length, expected, label):
    f.seek(offset)
    data = f.read(length)
    if data == expected:
        print(f"  PASS [{label}] offset={offset} len={length}: correct data")
        return True
    else:
        print(f"  FAIL [{label}] offset={offset} len={length}")
        print(f"    Expected first 16 bytes: {expected[:16].hex()}")
        print(f"    Got first 16 bytes:      {data[:16].hex()}")
        return False

def main():
    print(f"Testing on: {MOUNT}")
    print()

    # Clean up from previous run
    if os.path.exists(TEST_FILE):
        os.unlink(TEST_FILE)

    all_pass = True

    # ============================================================
    # Phase 1: Write sparse data (like qcow2 L2 table preallocation)
    # ============================================================
    print("Phase 1: Writing sparse data (simulating qcow2 L2 table preallocation)...")
    with open(TEST_FILE, "wb") as f:
        # Write range 1: 0 to 64KB with pattern 0xAA
        f.seek(RANGE1_START)
        f.write(b"\xAA" * (RANGE1_END - RANGE1_START))

        # Write range 2: 72KB to 76KB with pattern 0xBB (leaving gap at 64K-72K)
        f.seek(RANGE2_START)
        f.write(b"\xBB" * (RANGE2_END - RANGE2_START))

        # Write range 3: 80KB to 84KB with pattern 0xCC (leaving gap at 76K-80K)
        f.seek(RANGE3_START)
        f.write(b"\xCC" * (RANGE3_END - RANGE3_START))

        # Flush to server — this should:
        # 1. Send fresh write to server with dirty_ranges [(0,65536),(73728,77824),(81920,86016)]
        # 2. Seed zero_gap_table with gaps [(65536,73728),(77824,81920)]
        print("  Fsyncing to flush writes and seed zero_gap_table...")
        f.flush()
        os.fsync(f.fileno())

    print(f"  Wrote sparse data: range1=[0,{RANGE1_END}], range2=[{RANGE2_START},{RANGE2_END}], range3=[{RANGE3_START},{RANGE3_END}]")
    print(f"  Zero gaps should be: [{GAP1_START},{GAP1_END}] and [{GAP2_START},{GAP2_END}]")
    print()

    # ============================================================
    # Phase 2: Write to gap region (like mkfs writing a data cluster)
    # ============================================================
    print("Phase 2: Writing to gap region (simulating mkfs.ext4 data cluster write)...")
    GAP_WRITE_OFFSET = GAP1_START  # Write to the start of gap 1
    GAP_WRITE_DATA = b"\xFF" * 4096  # 4KB of 0xFF (like a bitmap with all bits set)

    with open(TEST_FILE, "r+b") as f:
        f.seek(GAP_WRITE_OFFSET)
        f.write(GAP_WRITE_DATA)
        # Don't fsync yet — this mimics mkfs writing before the flush cycle
        # The data is in dirty_ranges but NOT yet flushed to server
        # Zero_gap for this chunk should be CLEARED by this write
        print(f"  Wrote 4KB to gap at offset {GAP_WRITE_OFFSET}")

        # Phase 2a: Read back immediately (before flush) — should return 0xFF
        print()
        print("Phase 2a: Reading back from gap region BEFORE fsync...")
        all_pass &= verify_read(f, GAP_WRITE_OFFSET, 4096, GAP_WRITE_DATA, "pre-flush read of just-written gap data")

        # Phase 2b: Read from original dirty range — should return 0xAA
        all_pass &= verify_read(f, RANGE1_START + 1024, 4096, b"\xAA" * 4096, "pre-flush read of range1 data")

        # Flush to server
        print()
        print("  Fsyncing gap write to server...")
        f.flush()
        os.fsync(f.fileno())

    # ============================================================
    # Phase 3: Read back after flush (zero_gap should be cleared, chunk_cache has new data)
    # ============================================================
    print()
    print("Phase 3: Reading back after fsync...")
    with open(TEST_FILE, "rb") as f:
        # Gap 1 should have 0xFF (the data we just wrote)
        all_pass &= verify_read(f, GAP_WRITE_OFFSET, 4096, GAP_WRITE_DATA, "post-flush read of gap1 written data")

        # Range 1 should have 0xAA (from the initial write)
        all_pass &= verify_read(f, RANGE1_START + 1024, 4096, b"\xAA" * 4096, "post-flush read of range1 data")

        # Range 2 should have 0xBB (from the initial write)
        all_pass &= verify_read(f, RANGE2_START, 4096, b"\xBB" * 4096, "post-flush read of range2 data")

        # Gap 2 (never written) should have 0x00 zeros
        all_pass &= verify_read(f, GAP2_START, 4096, b"\x00" * 4096, "post-flush read of gap2 (should be zeros)")

    # ============================================================
    # Phase 4: Wait for zero_gap to potentially expire and read again
    # ============================================================
    print()
    print("Phase 4: Re-reading after 2 seconds (checking for stale zero_gap)...")
    time.sleep(2)
    with open(TEST_FILE, "rb") as f:
        all_pass &= verify_read(f, GAP_WRITE_OFFSET, 4096, GAP_WRITE_DATA, "stale-zero_gap read of written gap1")
        all_pass &= verify_read(f, RANGE1_START + 1024, 4096, b"\xAA" * 4096, "stale check of range1")
        all_pass &= verify_read(f, GAP2_START, 4096, b"\x00" * 4096, "stale check of unwritten gap2")

    # Clean up
    os.unlink(TEST_FILE)

    print()
    if all_pass:
        print("ALL TESTS PASSED")
        return 0
    else:
        print("TESTS FAILED — gap data corruption detected!")
        return 1

if __name__ == "__main__":
    sys.exit(main())
