# Thread Pool Implementation Plan

## Problem Summary
DFS client with `--write-buffer` experiences DVR glitching because FUSE operations serialize via `runtime.block_on()`. When DVR does simultaneous read+write on the SAME client, they block each other.

## Test Results
- **Different clients**: Write 11MB/s, read works, NO glitching ✓
- **Same client** (DVR): Read blocks write, write blocks read = glitching ✗

## Root Cause
FUSE trait requires synchronous functions → all callbacks use `runtime.block_on()` → operations serialize → DVR can't read while writing

## Solution: Async Task Spawning on Tokio Runtime

Use `tokio::spawn` to run FUSE async operations concurrently on the tokio runtime.

### Implementation Strategy
1. Wrap each FUSE callback body in `tokio::spawn` (NOT `spawn_blocking`)
2. Convert `runtime.block_on()` calls to direct `.await`
3. Reply to FUSE from within the spawned async task
4. This allows true concurrent async IO operations

### Key FUSE Operations to Parallelize
- `read()` - DVR reads to verify recording
- `write()` - DVR writes video stream
- `flush()` - Periodic buffer flush
- `getattr()` - Already fast (cache-only), but include for consistency
- `lookup()` - Directory operations

### Benefits
- **No new dependencies** - tokio already included
- **Automatic thread pool management** - tokio handles sizing
- **Minimal code changes** - wrap existing logic
- **Solves DVR issue** - read+write can run concurrently

### Implementation Example
```rust
fn read(&mut self, req, ino, offset, size, reply) {
    let ctx = self.clone();  // Clone Arc-wrapped data
    self.runtime.spawn_blocking(move || {
        let result = ctx.runtime.block_on(ctx.read_impl(ino, offset, size));
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(libc::EIO),
        }
    });
}
```

## Implementation Complexity Analysis

After analyzing the code, implementing thread pools for FUSE operations is more complex than initially expected:

### Challenges
1. **write()** function is 200+ lines with complex async logic for:
   - Write-behind buffering
   - Flush buffer management
   - Sequential vs random write detection
   - Metadata updates

2. **flush()** calls `flush_buffer_async()` which has:
   - Complex async logic
   - Metadata cache synchronization
   - Cluster metadata updates

### Simpler Alternative: Multi-Mount Architecture

Based on your test results showing **NO glitching when using different clients**, a simpler solution is:

**Run TWO dfs-client instances on the same machine:**
1. Mount 1: `/mnt/dfs-write` - dedicated for DVR writing
2. Mount 2: `/mnt/dfs-read` - dedicated for DVR verification reads

**Benefits:**
- No code changes needed
- Each mount has separate FUSE thread
- Write and read naturally run on different threads
- Proven to work (your SCP test)
- Can implement TODAY

**Implementation:**
```bash
# Terminal 1 - Write mount
dfs-client mount /mnt/dfs-write --cluster 10.25.1.58:8900 --write-buffer

# Terminal 2 - Read mount
dfs-client mount /mnt/dfs-read --cluster 10.25.1.58:8900

# DVR writes to: /mnt/dfs-write/recording.mp4
# DVR reads from: /mnt/dfs-read/recording.mp4
```

## Next Steps (if thread pool approach is still desired)
1. ✓ Document plan (this file)
2. ✓ Analyzed implementation complexity
3. Recommend multi-mount solution
4. If thread pool still needed:
   - Start with `read()` only
   - Then tackle `write()` refactoring
   - Then `flush()` refactoring

## Expected Outcome
DVR can write video stream while simultaneously reading to verify, with both operations running concurrently without blocking each other.
