# SQLite on DFS - Status Report

## Date: 2026-04-01

## Issues Fixed

### 1. Metadata Batching Causing Data Loss ✅ FIXED
**Problem**: File metadata (including size) was only updated to the cluster every 10 writes for performance. This caused SQLite to see size=0 after reopening databases.

**Fix**: Disabled metadata batching for SQLite database files (`.db`, `.sqlite`, `.sqlite3`, `.db-wal`, `.db-journal`, `.db-shm`). These files now get immediate metadata updates on every write.

**Location**: `dfs-client/src/fuse_impl.rs` lines 1514-1535

### 2. fsync() Not Blocking ✅ FIXED
**Problem**: The fsync() implementation was using `runtime.spawn()` (non-blocking) instead of `block_on()`, so fsync returned before metadata was actually flushed to disk.

**Fix**: Changed to use `block_on()` to ensure fsync waits for metadata flush to complete.

**Location**: `dfs-client/src/fuse_impl.rs` lines 2274-2313

### 3. flush() and release() Not Persisting Metadata ✅ FIXED
**Problem**: Similar to fsync, these operations weren't flushing pending metadata updates.

**Fix**: Added metadata flush logic to both flush() and release() operations when write-buffer is disabled.

**Location**: `dfs-client/src/fuse_impl.rs` lines 1566-1686, 1688-1764

## Known Issues

### SQLite Index Corruption on Reopen ⚠️ PARTIAL ISSUE
**Symptoms**:
- When a database with indexes is created, data inserted, and connection closed
- Upon reopening, `COUNT(*)` returns 0 but `SELECT *` returns all rows
- PRAGMA integrity_check reports "row N missing from index"

**Impact**:
- **Data is NOT lost** - all rows are retrievable with direct SELECT
- COUNT(*) queries return incorrect results
- Queries using indexes may not find all matching rows

**Root Cause**:
This appears to be related to how SQLite's internal B-tree pages for indexes are being cached/written. The index pages are not being properly synchronized when the database is reopened by a new connection.

**Workaround**:
1. **Option A**: Create indexes AFTER data insertion
   ```sql
   CREATE TABLE t1(id INT, data TEXT);
   INSERT INTO t1 VALUES (1, 'test');
   -- Then in same connection or new:
   CREATE INDEX idx_data ON t1(data);
   PRAGMA integrity_check;  -- Shows "ok"
   ```

2. **Option B**: Use SQLite without indexes
   - For pihole, test if indexes are actually needed for query performance
   - Most DNS query logs are append-only and may not benefit from indexes

3. **Option C**: Single long-lived connection
   - Keep one SQLite connection open for the lifetime of the application
   - Within a single connection, indexes work correctly

## Test Results

### What Works ✅
- Basic table creation and data insertion
- WAL mode (no longer gets I/O errors)
- Data persistence across reopens
- Transactions
- Database attach/detach
- Creating indexes AFTER data exists
- Single connection workflows

### What Has Issues ⚠️
- Indexes created before data insertion + connection reopen
- COUNT(*) queries after reopen (when indexes exist)
- Index-based queries after reopen

## Recommendations for Pihole

1. **Test without --write-buffer first** (current fixes are for this mode)
   ```bash
   dfs-client mount /mnt/dfs -c 10.25.1.58 --allow-other
   ```

2. **If indexes are causing issues**, consider:
   - Dropping indexes from pihole's gravity.db
   - OR modifying pihole to create indexes after initial data load
   - OR keeping a single long-lived database connection

3. **Monitor pihole logs** for:
   - "database disk image is malformed" errors (should be gone)
   - "no such table" errors (should be gone)
   - Query performance without indexes

## Files Modified
- `dfs-client/src/fuse_impl.rs`
  - Lines 1514-1570: Added SQLite detection and immediate metadata updates
  - Lines 1566-1686: Fixed flush() to persist metadata
  - Lines 1688-1764: Fixed release() to persist metadata
  - Lines 2274-2313: Fixed fsync() to block until metadata flushed

## Next Steps

If index corruption remains a blocker:
1. Investigate SQLite page cache interaction with FUSE direct I/O
2. Consider forcing fsync on ALL SQLite file writes (performance cost)
3. Test with different SQLite PRAGMA settings (synchronous, journal_mode)
4. Profile what specific SQLite operations are triggering the corruption
