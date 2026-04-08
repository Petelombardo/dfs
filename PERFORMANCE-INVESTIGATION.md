# Write Performance Investigation Checkpoint
**Date:** 2026-03-25
**Status:** ✅ FIXED - Client runtime issue resolved, ready to continue performance testing

## Current Performance
- **Read Performance:** ✅ FIXED - 23 MB/s (was 380 KB/s)
  - Fixed by implementing chunk-level caching in FUSE client
  - Eliminated redundant chunk fetches (32 fetches per 4MB chunk → 1 fetch)

- **Write Performance:** ⚠️ BOTTLENECK - 14-15 MB/s (target: >50 MB/s)
  - Capped at precisely 14-15 MB/s despite gigabit network and low CPU usage

## Investigation Completed

### CPU Testing Results
- **NOT CPU-bound** - plenty of headroom:
  - Client: 20-35% CPU during writes (65-80% idle)
  - Servers: 10-18% CPU during writes (82-90% idle)
  - Network: Gigabit confirmed (1000 Mb/s)

### Timing Instrumentation Added
Added detailed timing logs to identify bottleneck:

**Files Modified:**
1. `dfs-server/src/server.rs` - Added timing for:
   - Chunking/hashing time
   - Quorum write time
   - Metadata storage time
   - Total write time and throughput

2. `dfs-server/src/storage.rs` - Added timing for:
   - Checksum verification
   - Directory creation
   - File write
   - fsync() time
   - Atomic rename

3. `dfs-client/src/client.rs` - Added timing for:
   - TCP connect time
   - Serialization time
   - Network send time
   - Network receive time
   - Deserialization time

### Root Cause Identified

**Timing breakdown per 4MB chunk:**
```
Total: 161-187ms
├── Chunking (BLAKE3 hash): 17-21ms (10-12% overhead) ⚠️
├── Quorum write (2 nodes): 130-160ms (70-85% of time) 🔴 BOTTLENECK
│   ├── Network serialization/transmission
│   ├── Disk write on 2 nodes (GlusterFS)
│   ├── fsync() on both nodes
│   └── Network response
└── Metadata storage: 50-110µs (negligible)
```

**Performance Math:**
- 4MB / 160ms = 25 MB/s per chunk (theoretical)
- Dual-stream writes = 2 parallel streams
- But overhead reduces to 14-15 MB/s observed

**Key Findings:**
1. **Quorum write is the primary bottleneck (130-160ms)**
   - Writing to 2 nodes takes most of the time
   - Likely: fsync() on GlusterFS is slow

2. **BLAKE3 hashing overhead is significant (17-21ms)**
   - 10-12% of total write time just computing hash
   - Opportunity for optimization

3. **GlusterFS has RF=3 on top of DFS RF=3**
   - Each DFS write → 2 DFS nodes immediately
   - Each DFS node write → Gluster RF=3 = 3 underlying copies
   - Result: 6 total disk writes per chunk!

## Next Steps (In Priority Order)

### 1. Enable DEBUG Logging (Quick Win)
Enable DEBUG level logging to see detailed disk timing:
```bash
# Modify server logging level
# See: dfs-server/src/storage.rs line 28-58
# Need to see: sync time, rename time breakdown
```

### 2. Test Direct Disk Performance (Baseline)
Test raw Gluster write performance to establish baseline:
```bash
# On each gluster node:
dd if=/dev/zero of=/mnt/gluster/test.img bs=4M count=10 oflag=direct
# Compare with DFS write performance
```

### 3. Optimize BLAKE3 Hashing (10-12% gain)
Current: 17-21ms per 4MB chunk
Options:
- Use BLAKE3 SIMD optimizations (may already be enabled)
- Consider pre-computing hash during data read (pipeline)
- Evaluate if checksum verification can be async

### 4. Analyze/Optimize Quorum Write (70-85% gain potential)
The fsync() call in storage.rs:50 is likely the culprit:
```rust
file.sync_all().context("Failed to sync chunk data")?;
```

**Options:**
- **Option A (Fast but risky):** Remove fsync() - trade durability for speed
- **Option B (Balanced):** Make fsync() async after quorum response
- **Option C (Safe):** Use write-behind caching (already implemented in client)
- **Option D (Investigate):** Check if GlusterFS is causing sync amplification

### 5. Consider Connection Pooling
Currently creating new TCP connection per write:
- Add connection pool to client
- Reuse connections across writes
- Reduces TCP handshake overhead

## Files Modified (This Session)

### Read Performance Fix:
- `dfs-client/src/fuse_impl.rs` - Added chunk caching, open() handler with FOPEN_KEEP_CACHE

### Instrumentation:
- `dfs-server/src/server.rs` - Added comprehensive timing logs
- `dfs-server/src/storage.rs` - Added disk operation timing
- `dfs-client/src/client.rs` - Added network operation timing

### Scripts:
- `scripts/deploy-all.sh` - Deployment script for all nodes

## Key Metrics

**Before optimization:**
- Read: 380 KB/s
- Write: 14-15 MB/s

**After read optimization:**
- Read: 23 MB/s ✅ (60x improvement!)
- Write: 14-15 MB/s (unchanged - still investigating)

**Target:**
- Read: 23 MB/s ✅ ACHIEVED
- Write: >50 MB/s (need 3-4x improvement)

## Outstanding Issues

1. **Write Performance** - Root cause identified, solutions available
2. **Cluster Resilience** - Phase 8: Cluster Join Protocol not implemented
   - Nodes don't auto-rejoin after restart
   - Requires manual restart of all nodes when seed node reboots
3. **Client Memory Management** - Future optimization needed
   - Unbounded metadata caches will grow with large file counts
   - See Future Optimizations section below

## Bug Fixed (2026-03-25)

**Problem:** Client would mount but `readdir` calls failed with I/O errors.

**Root Causes:**
1. **Tokio runtime threading issue**: Using `#[tokio::main]` + `block_on` from FUSE threads caused "Cannot start a runtime from within a runtime" panic
2. **Empty filename bug**: Server's `list_directory` returned root directory `/` as an entry, which split to empty filename, causing FUSE to fail

**Solutions:**
1. Created dedicated runtime thread with `tokio::time::sleep` loop to keep it alive
2. Added check to skip entries with empty filenames in `readdir`
3. Changed from `tokio::task::block_in_place` to direct `runtime.block_on()` for non-tokio threads

**Files Modified:**
- `dfs-client/src/main.rs` - Runtime thread setup
- `dfs-client/src/fuse_impl.rs` - Empty filename check, runtime handling

## Commands for Next Session

### Deploy instrumented binaries:
```bash
cd /home/petelombardo/distributefilesystem
./scripts/deploy-all.sh
```

### Run write test:
```bash
ssh root@nanopir3 "cd /mnt/test && dd if=/dev/zero of=test.img bs=1M count=100 oflag=direct"
```

### Check timing logs:
```bash
# Server logs:
ssh root@gluster1 "journalctl -u dfs-server --since '1 minute ago' --no-pager | grep -E '(quorum write|complete in)' | tail -20"

# Client logs would be in the terminal where dfs-client mount is running
```

### Test raw Gluster performance:
```bash
ssh root@gluster1 "cd /mnt/gluster && dd if=/dev/zero of=direct_test.img bs=4M count=25 oflag=direct conv=fdatasync"
```

## Future Optimizations (Not Urgent)

### 1. Client Metadata Cache LRU Eviction
**Problem:** Client maintains unbounded HashMaps for metadata caching
- `metadata_cache: HashMap<u64, FileMetadata>` - grows forever
- `path_to_inode: HashMap<String, u64>` - grows forever

**Impact:** With 100k+ files accessed, could grow to ~200MB+ over time

**Solution:** Implement LRU eviction policy
```rust
// Option 1: Use lru crate
use lru::LruCache;
metadata_cache: Arc<Mutex<LruCache<u64, FileMetadata>>>, // Cap at 10k entries

// Option 2: Implement time-based eviction
// Track last access time and periodically evict entries older than 1 hour
```

**Priority:** Low - only matters for very large file counts (>100k files)
**Effort:** ~2 hours
**Location:** `dfs-client/src/fuse_impl.rs`

### 2. Server-Side Write Optimizations
**From investigation above, choose one:**

**Option A: Remove fsync() (Fast but risky)**
- Remove `file.sync_all()` from `dfs-server/src/storage.rs:50`
- Risk: Data loss on crash before OS flushes to disk
- Gain: Potentially 50-70% write speedup
- Best for: Non-critical data, high-performance needs

**Option B: Async fsync() (Balanced)**
- Return success to client before fsync completes
- fsync in background, log errors
- Risk: Client thinks write succeeded but might fail
- Gain: ~40-60% write speedup
- Best for: Balancing safety and performance

**Option C: Batch fsync() (Safe)**
- Accumulate multiple writes, fsync once per batch
- Keep durability guarantees
- Gain: ~20-30% write speedup
- Best for: Production systems

**Priority:** Medium - write performance is 3x slower than target
**Effort:** 2-4 hours depending on option
**Location:** `dfs-server/src/storage.rs`

### 3. Connection Pooling
**Problem:** Client creates new TCP connection per write operation
- Adds TCP handshake overhead (~1-2ms per write)
- Wastes time establishing/tearing down connections

**Solution:** Maintain persistent connection pool
```rust
// In dfs-client/src/client.rs
connection_pool: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,
```

**Priority:** Low-Medium
**Effort:** ~3 hours
**Gain:** 5-10% reduction in write latency
**Location:** `dfs-client/src/client.rs`

## References
- Session logs: See conversation before this checkpoint
- Code location: /home/petelombardo/distributefilesystem
- Deployment: servers=gluster1,2,3 client=nanopir3
- FUSE mount: /mnt/test on nanopir3
