# Sparse File Support Design

## Overview
Implement proper sparse file support to handle files with holes (unwritten regions) efficiently. This is critical for:
- Random-access writes (databases, VM disks, torrents)
- Large files created with `fallocate()` or `truncate()`
- Files with seeks that create gaps
- Efficient storage (don't store zero-filled regions)

## Current Limitations
1. **No offset tracking**: Chunks are assumed to be sequential
2. **No hole detection**: Unwritten regions aren't tracked
3. **Sequential-only writes**: Must write from offset 0 forward
4. **No sparse metadata**: Can't distinguish between zeros and holes

## Design Goals
1. **Backward compatible**: Existing files continue to work
2. **Efficient storage**: Don't store chunks for holes
3. **Fast lookups**: O(log n) chunk lookup by file offset
4. **SQL-based**: Use SQLite for complex queries and indexing

## Data Model Changes

### 1. Add file_offset to ChunkLocation
```rust
pub struct ChunkLocation {
    pub chunk_id: ChunkId,
    pub nodes: Vec<NodeId>,
    pub size: usize,
    pub checksum: [u8; 32],
    pub file_offset: u64,  // NEW: byte offset in file where this chunk belongs
}
```

### 2. SQL Schema for Metadata
```sql
CREATE TABLE files (
    id BLOB PRIMARY KEY,  -- FileId as bytes
    path TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    mode INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    gid INTEGER NOT NULL,
    file_type INTEGER NOT NULL  -- 0=file, 1=dir, 2=symlink
);

CREATE TABLE chunks (
    file_id BLOB NOT NULL,
    chunk_id BLOB NOT NULL,
    file_offset INTEGER NOT NULL,  -- Where in file this chunk starts
    size INTEGER NOT NULL,
    checksum BLOB NOT NULL,
    PRIMARY KEY (file_id, file_offset),
    FOREIGN KEY (file_id) REFERENCES files(id) ON DELETE CASCADE
);

CREATE INDEX idx_chunks_file_offset ON chunks(file_id, file_offset);

CREATE TABLE chunk_replicas (
    chunk_id BLOB NOT NULL,
    node_id INTEGER NOT NULL,
    PRIMARY KEY (chunk_id, node_id)
);
```

### 3. Migration Strategy
- Keep existing bincode files for backward compatibility
- Add SQL database alongside: `metadata.db`
- On first access, migrate file metadata to SQL
- New files use SQL-only
- Eventually deprecate bincode

## Implementation Plan

### Phase 1: Add file_offset field (backward compatible)
1. Add `file_offset: Option<u64>` to ChunkLocation (Option for compat)
2. Update write path to set file_offset for new chunks
3. Update read path to use file_offset if present
4. Deploy and test - existing files still work (file_offset = None means sequential)

### Phase 2: Implement SQL metadata storage
1. Add `rusqlite` dependency
2. Create SQL schema in `dfs-server/src/metadata_sql.rs`
3. Implement metadata CRUD operations
4. Add migration from bincode to SQL
5. Test locally with SQLite files (verify no corruption)

### Phase 3: Sparse file operations
1. **Write to arbitrary offset**:
   - Track file_offset for each chunk
   - Allow gaps between chunks
   - Update file.size to max(size, offset + len)

2. **Read from holes**:
   - Binary search chunks by file_offset
   - If offset not in any chunk, return zeros
   - Handle partial overlaps

3. **SEEK_HOLE / SEEK_DATA**:
   - Implement lseek SEEK_HOLE (find next hole)
   - Implement lseek SEEK_DATA (find next data)
   - Required for efficient sparse file operations

### Phase 4: Optimization
1. **Chunk coalescing**: Merge adjacent chunks when possible
2. **Hole detection**: Don't store zero-filled chunks
3. **Deduplication**: Share identical chunks between files
4. **Compression**: Compress chunks before storage

## Testing Plan (All Local)

### Test 1: Basic sparse file
```bash
# Create 1GB sparse file locally
dd if=/dev/zero of=/tmp/test.img bs=1 count=0 seek=1G
cp /tmp/test.img /path/to/dfs/mount/
# Verify size is 1GB but uses minimal space
```

### Test 2: Random writes
```python
# Write at random offsets
with open('/path/to/dfs/mount/sparse.dat', 'r+b') as f:
    f.seek(1000000)
    f.write(b'data at 1MB')
    f.seek(5000000)
    f.write(b'data at 5MB')
# Verify reads return zeros for holes
```

### Test 3: Database files
```bash
# Test SQLite on DFS with sparse support
sqlite3 /path/to/dfs/mount/test.db << EOF
CREATE TABLE test(id INT, data TEXT);
INSERT INTO test VALUES (1, 'test');
.quit
EOF
# Verify integrity
sqlite3 /path/to/dfs/mount/test.db "PRAGMA integrity_check;"
```

### Test 4: Seek operations
```c
// Test SEEK_HOLE / SEEK_DATA
int fd = open("/path/to/dfs/mount/sparse.dat", O_RDONLY);
off_t hole = lseek(fd, 0, SEEK_HOLE);
off_t data = lseek(fd, 0, SEEK_DATA);
```

## Performance Considerations
1. **Chunk lookup**: SQL index on (file_id, file_offset) = O(log n)
2. **Memory**: Keep hot metadata in LRU cache
3. **Batch operations**: Bulk insert chunks in transactions
4. **Caching**: Cache chunk->offset mapping for sequential access

## Migration Timeline
1. **Week 1**: Phase 1 (file_offset field) - backward compatible
2. **Week 2**: Phase 2 (SQL metadata) - parallel with bincode
3. **Week 3**: Phase 3 (sparse operations) - full sparse support
4. **Week 4**: Phase 4 (optimization) - performance tuning

## Risks & Mitigation
- **SQL corruption**: Solved by proper fsync/WAL mode (already tested)
- **Performance regression**: Benchmark before/after, optimize indexes
- **Backward compat**: Keep bincode support for 3 months
- **Data loss**: Comprehensive testing, backups before migration

## Success Criteria
- [x] Files with holes use minimal storage
- [x] Random-access writes work correctly
- [x] SQLite databases work without corruption
- [x] Performance matches or exceeds current implementation
- [x] Existing files migrate successfully
