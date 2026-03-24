use anyhow::Result;
use dfs_common::{ChunkId, FileMetadata, FileType};
use fuser::{
    FileAttr, FileType as FuseFileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, Request as FuseRequest,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error};

use crate::client::DfsClient;

/// FUSE filesystem implementation for DFS
pub struct DfsFilesystem {
    /// Client for communicating with DFS cluster
    client: Arc<DfsClient>,

    /// Metadata cache: inode -> FileMetadata
    metadata_cache: Arc<RwLock<HashMap<u64, FileMetadata>>>,

    /// Path to inode mapping
    path_to_inode: Arc<RwLock<HashMap<String, u64>>>,

    /// Next available inode number
    next_inode: Arc<RwLock<u64>>,

    /// Root inode is always 1 (FUSE convention)
    root_inode: u64,

    /// Tokio runtime handle for async operations
    runtime: tokio::runtime::Handle,
}

impl DfsFilesystem {
    /// Create a new DFS filesystem
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        let client = Arc::new(DfsClient::new(cluster_nodes)?);

        let metadata_cache = Arc::new(RwLock::new(HashMap::new()));
        let path_to_inode = Arc::new(RwLock::new(HashMap::new()));
        let next_inode = Arc::new(RwLock::new(2)); // Start at 2, root is 1

        // Create root directory metadata
        let root_metadata = FileMetadata {
            id: dfs_common::FileId::new(),
            path: "/".to_string(),
            size: 0,
            chunks: Vec::new(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            modified_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            mode: 0o755,
            uid: 0,
            gid: 0,
            file_type: FileType::Directory,
        };

        metadata_cache.write().unwrap().insert(1, root_metadata);
        path_to_inode.write().unwrap().insert("/".to_string(), 1);

        // Get the current tokio runtime handle
        let runtime = tokio::runtime::Handle::current();

        Ok(Self {
            client,
            metadata_cache,
            path_to_inode,
            next_inode,
            root_inode: 1,
            runtime,
        })
    }

    /// Convert FileMetadata to FUSE FileAttr
    fn metadata_to_attr(&self, ino: u64, metadata: &FileMetadata) -> FileAttr {
        let kind = match metadata.file_type {
            FileType::RegularFile => FuseFileType::RegularFile,
            FileType::Directory => FuseFileType::Directory,
            FileType::Symlink => FuseFileType::Symlink,
        };

        FileAttr {
            ino,
            size: metadata.size,
            blocks: (metadata.size + 511) / 512, // 512-byte blocks
            atime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            mtime: UNIX_EPOCH + Duration::from_secs(metadata.modified_at),
            ctime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            crtime: UNIX_EPOCH + Duration::from_secs(metadata.created_at),
            kind,
            perm: metadata.mode as u16,
            nlink: 1,
            uid: metadata.uid,
            gid: metadata.gid,
            rdev: 0,
            blksize: 4 * 1024 * 1024, // 4MB chunk size
            flags: 0,
        }
    }

    /// Get or allocate inode for a path
    fn get_or_create_inode(&self, path: &str) -> u64 {
        let path_map = self.path_to_inode.read().unwrap();
        if let Some(&ino) = path_map.get(path) {
            return ino;
        }
        drop(path_map);

        // Allocate new inode
        let mut next = self.next_inode.write().unwrap();
        let ino = *next;
        *next += 1;
        drop(next);

        self.path_to_inode
            .write()
            .unwrap()
            .insert(path.to_string(), ino);

        ino
    }

    /// Get path from parent inode and name
    fn get_path_from_parent(&self, parent: u64, name: &OsStr) -> Option<String> {
        let cache = self.metadata_cache.read().unwrap();
        let parent_metadata = cache.get(&parent)?;
        let name_str = name.to_str()?;

        let parent_path = &parent_metadata.path;
        let full_path = if parent_path == "/" {
            format!("/{}", name_str)
        } else {
            format!("{}/{}", parent_path, name_str)
        };

        Some(full_path)
    }
}

impl Filesystem for DfsFilesystem {
    fn lookup(&mut self, _req: &FuseRequest, parent: u64, name: &OsStr, reply: ReplyEntry) {
        debug!("lookup: parent={}, name={:?}", parent, name);

        let path = match self.get_path_from_parent(parent, name) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check cache first
        {
            let path_map = self.path_to_inode.read().unwrap();
            if let Some(&ino) = path_map.get(&path) {
                let cache = self.metadata_cache.read().unwrap();
                if let Some(metadata) = cache.get(&ino) {
                    let attr = self.metadata_to_attr(ino, metadata);
                    reply.entry(&Duration::from_secs(1), &attr, 0);
                    return;
                }
            }
        }

        // Fetch from cluster
        let client = self.client.clone();
        let result = self.runtime.block_on(async {
            client.get_file_metadata(&path).await
        });

        match result {
            Ok(Some(metadata)) => {
                // Allocate inode
                let ino = self.get_or_create_inode(&path);

                // Cache metadata
                self.metadata_cache.write().unwrap().insert(ino, metadata.clone());

                // Convert to FUSE attr
                let attr = self.metadata_to_attr(ino, &metadata);
                reply.entry(&Duration::from_secs(1), &attr, 0);
            }
            Ok(None) => {
                reply.error(libc::ENOENT);
            }
            Err(e) => {
                error!("Failed to lookup {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &FuseRequest, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        debug!("getattr: ino={}", ino);

        let cache = self.metadata_cache.read().unwrap();
        if let Some(metadata) = cache.get(&ino) {
            let attr = self.metadata_to_attr(ino, metadata);
            reply.attr(&Duration::from_secs(1), &attr);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        debug!("read: ino={}, offset={}, size={}", ino, offset, size);

        let metadata = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(m) => m.clone(),
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        if metadata.file_type != FileType::RegularFile {
            reply.error(libc::EISDIR);
            return;
        }

        let client = self.client.clone();
        let chunk_ids = metadata.chunks.clone();

        let result = self.runtime.block_on(async {
            client.read_data(&chunk_ids).await
        });

        match result {
            Ok(data) => {
                let offset = offset as usize;
                let size = size as usize;

                if offset >= data.len() {
                    reply.data(&[]);
                } else {
                    let end = std::cmp::min(offset + size, data.len());
                    reply.data(&data[offset..end]);
                }
            }
            Err(e) => {
                error!("Failed to read data: {}", e);
                reply.error(libc::EIO);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &FuseRequest,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!("readdir: ino={}, offset={}", ino, offset);

        let path = {
            let cache = self.metadata_cache.read().unwrap();
            match cache.get(&ino) {
                Some(metadata) => {
                    if metadata.file_type != FileType::Directory {
                        reply.error(libc::ENOTDIR);
                        return;
                    }
                    metadata.path.clone()
                }
                None => {
                    reply.error(libc::ENOENT);
                    return;
                }
            }
        };

        let client = self.client.clone();
        let result = self.runtime.block_on(async {
            client.list_directory(&path).await
        });

        match result {
            Ok(entries) => {
                let mut entry_offset = 0i64;

                // Add . and ..
                if offset == 0 {
                    if reply.add(ino, entry_offset + 1, FuseFileType::Directory, ".") {
                        reply.ok();
                        return;
                    }
                    entry_offset += 1;
                }
                if offset <= 1 {
                    if reply.add(ino, entry_offset + 1, FuseFileType::Directory, "..") {
                        reply.ok();
                        return;
                    }
                    entry_offset += 1;
                }

                // Add actual entries
                let skip_count = if offset > 2 { (offset - 2) as usize } else { 0 };
                for (i, entry) in entries.iter().enumerate().skip(skip_count) {
                    let file_name = entry.path.rsplit('/').next().unwrap_or("");
                    let kind = match entry.file_type {
                        FileType::RegularFile => FuseFileType::RegularFile,
                        FileType::Directory => FuseFileType::Directory,
                        FileType::Symlink => FuseFileType::Symlink,
                    };

                    // Get or allocate inode
                    let entry_ino = self.get_or_create_inode(&entry.path);

                    // Cache metadata
                    self.metadata_cache
                        .write()
                        .unwrap()
                        .insert(entry_ino, entry.clone());

                    if reply.add(entry_ino, offset + 2 + i as i64 + 1, kind, file_name) {
                        break; // Buffer full
                    }
                }

                reply.ok();
            }
            Err(e) => {
                error!("Failed to read directory {}: {}", path, e);
                reply.error(libc::EIO);
            }
        }
    }
}
