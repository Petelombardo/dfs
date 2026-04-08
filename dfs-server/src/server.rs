use crate::chunker::Chunker;
use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::{MessageHandler, NetworkClient};
use crate::storage::ChunkStorage;
use anyhow::{Context, Result};
use dfs_common::{
    compute_chunk_hash, ChunkId, ChunkLocation, ClusterMessage, ErrorCode, FileId, FileMetadata,
    Message, NodeId, NodeInfo, Request, Response,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Cached storage statistics to avoid expensive stat calls
#[derive(Clone)]
struct StorageStatsCache {
    total_chunks: usize,
    total_space: u64,
    free_space: u64,
    available_space: u64,
    timestamp: std::time::Instant,
}

/// Main server context holding all components
/// This is the core of the DFS node
pub struct Server {
    /// Local chunk storage
    storage: Arc<ChunkStorage>,

    /// Metadata store
    metadata: Arc<MetadataStore>,

    /// File chunker
    chunker: Arc<Chunker>,

    /// Cluster manager
    cluster: Arc<ClusterManager>,

    /// Network client for talking to other nodes
    client: Arc<NetworkClient>,

    /// Replication factor
    replication_factor: usize,

    /// Metadata directory path for persisting peer list
    metadata_dir: PathBuf,

    /// Storage stats cache with 10-second TTL
    storage_stats_cache: Arc<RwLock<Option<StorageStatsCache>>>,

    /// Prefetch concurrency limiter - prevents prefetch from overwhelming disk I/O
    /// Limits low-priority prefetch operations while allowing unlimited high-priority reads
    prefetch_semaphore: Arc<tokio::sync::Semaphore>,
}

impl Server {
    /// Create a new server instance
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        chunk_size: usize,
        cluster: Arc<ClusterManager>,
        replication_factor: usize,
        metadata_dir: PathBuf,
    ) -> Self {
        Self {
            storage,
            metadata,
            chunker: Arc::new(Chunker::new(chunk_size)),
            cluster,
            client: Arc::new(NetworkClient::new()),
            replication_factor,
            metadata_dir,
            storage_stats_cache: Arc::new(RwLock::new(None)),
            // Allow 8 concurrent prefetch operations for faster cache warming
            // With modern HDDs and read-ahead, parallel reads are efficient
            // Real client reads bypass this limit (high priority)
            prefetch_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
        }
    }

    /// Get reference to cluster manager
    pub fn cluster(&self) -> Arc<ClusterManager> {
        self.cluster.clone()
    }

    /// Get reference to network client
    pub fn network_client(&self) -> Arc<NetworkClient> {
        self.client.clone()
    }

    /// Handle an incoming request message
    pub async fn handle_request(&self, request: Request) -> Response {
        match request {
            Request::ReadChunk { chunk_id, sequential_hint } => {
                if let Some((idx, total)) = sequential_hint {
                    debug!("ReadChunk {} with sequential hint: {}/{} chunks", chunk_id, idx, total);
                    // TODO: Use hint for server-side prefetching
                }
                self.handle_read_chunk(chunk_id).await
            },
            Request::ReadChunkRange { chunk_id, offset, length } => {
                self.handle_read_chunk_range(chunk_id, offset, length).await
            }
            Request::WriteChunk {
                chunk_id,
                data,
                checksum,
            } => self.handle_write_chunk(chunk_id, data, checksum).await,
            Request::DeleteChunk { chunk_id } => self.handle_delete_chunk(chunk_id).await,
            Request::HasChunk { chunk_id } => self.handle_has_chunk(chunk_id).await,
            Request::ReplicateChunk {
                chunk_id,
                data,
                checksum,
            } => self.handle_replicate_chunk(chunk_id, data, checksum).await,
            Request::ReplicateMetadata { metadata } => {
                self.handle_replicate_metadata(metadata).await
            }
            Request::DeleteMetadata { file_id, path } => {
                self.handle_delete_metadata(file_id, path).await
            }
            Request::ReplicateChunkLocation { location } => {
                self.handle_replicate_chunk_location(location).await
            }
            Request::PrefetchHint { chunk_ids } => {
                self.handle_prefetch_hint(chunk_ids).await
            }
            Request::GetFileMetadataByPath { path, if_modified_since } => {
                self.handle_get_file_metadata_by_path(path, if_modified_since).await
            }
            Request::PutFileMetadata { metadata } => {
                self.handle_put_file_metadata(metadata).await
            }
            Request::ListDirectory { path } => self.handle_list_directory(path).await,
            Request::WriteFile { data } => self.handle_write_file(data).await,
            Request::WriteFileLocalOnly { data } => self.handle_write_file_local_only(data).await,
            Request::DeleteFile { path } => self.handle_delete_file(path).await,
            Request::RenameFile { old_path, new_path } => {
                self.handle_rename_file(old_path, new_path).await
            }

            // Admin requests
            Request::GetClusterStatus => self.handle_get_cluster_status().await,
            Request::GetStorageStats => self.handle_get_storage_stats().await,
            Request::GetHealingStatus => self.handle_get_healing_status().await,
            Request::TriggerScrub => self.handle_trigger_scrub().await,
            Request::EnableHealing => self.handle_enable_healing().await,
            Request::DisableHealing => self.handle_disable_healing().await,
            Request::TriggerHealing => self.handle_trigger_healing().await,
            Request::GetFileInfo { path } => self.handle_get_file_info(path).await,
            Request::GetChunkReplicas { chunk_id } => {
                self.handle_get_chunk_replicas(chunk_id).await
            }
            Request::RemoveNode { node_id } => self.handle_remove_node(node_id).await,
            Request::ListAllFiles => self.handle_list_all_files().await,
            Request::PurgeFileMetadata { path } => self.handle_purge_file_metadata(path).await,
            Request::PurgeFileMetadataById { file_id } => {
                self.handle_purge_file_metadata_by_id(file_id).await
            }

            _ => Response::Error {
                message: "Request type not yet implemented".to_string(),
                code: ErrorCode::InternalError,
            },
        }
    }

    /// Handle read chunk request (try local first, then forward to other nodes)
    async fn handle_read_chunk(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling read chunk: {}", chunk_id);

        // Use the internal read_chunk method which tries local first,
        // then forwards to other nodes if needed
        match self.read_chunk(&chunk_id).await {
            Ok(data) => {
                // Get cache stats for flow control
                let (capacity, size) = self.storage.get_cache_stats();
                let cache_stats = Some((0, capacity, size)); // hits=0 for now, can track later
                Response::ChunkData { chunk_id, data, cache_stats }
            },
            Err(e) => {
                warn!("Failed to read chunk {}: {}", chunk_id, e);
                Response::Error {
                    message: format!("Failed to read chunk: {}", e),
                    code: ErrorCode::NotFound,
                }
            }
        }
    }

    /// Handle read chunk range request (for striped multi-replica reads)
    async fn handle_read_chunk_range(&self, chunk_id: ChunkId, offset: u64, length: u64) -> Response {
        debug!("Handling read chunk range: {} offset={} length={}", chunk_id, offset, length);

        match self.read_chunk(&chunk_id).await {
            Ok(data) => {
                let start = offset as usize;
                let end = std::cmp::min(start + length as usize, data.len());

                if start >= data.len() {
                    return Response::Error {
                        message: format!("Offset {} beyond chunk size {}", offset, data.len()),
                        code: ErrorCode::InvalidRequest,
                    };
                }

                let range_data = data[start..end].to_vec();
                debug!("Returning {} bytes from chunk {} (requested {}, offset {})",
                       range_data.len(), chunk_id, length, offset);

                // Get cache stats for flow control
                let (capacity, size) = self.storage.get_cache_stats();
                let cache_stats = Some((0, capacity, size));

                Response::ChunkData {
                    chunk_id,
                    data: range_data,
                    cache_stats,
                }
            }
            Err(e) => {
                warn!("Failed to read chunk range {} offset={} length={}: {}",
                      chunk_id, offset, length, e);
                Response::Error {
                    message: format!("Failed to read chunk range: {}", e),
                    code: ErrorCode::NotFound,
                }
            }
        }
    }

    /// Handle write chunk request (local write + replication)
    async fn handle_write_chunk(
        &self,
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    ) -> Response {
        debug!("Handling write chunk: {} ({} bytes)", chunk_id, data.len());

        // Verify checksum matches chunk_id
        if checksum != chunk_id.hash {
            return Response::Error {
                message: "Checksum mismatch".to_string(),
                code: ErrorCode::ChecksumMismatch,
            };
        }

        // Write locally
        if let Err(e) = self.storage.write_chunk(&chunk_id, &data) {
            warn!("Failed to write chunk {}: {}", chunk_id, e);
            return Response::Error {
                message: format!("Failed to write chunk: {}", e),
                code: ErrorCode::IOError,
            };
        }

        // Update chunk location metadata
        let local_node_id = self.cluster.local_node_id();
        if let Ok(mut location) = self.get_or_create_chunk_location(&chunk_id, data.len()).await {
            if !location.nodes.contains(&local_node_id) {
                location.nodes.push(local_node_id);
                let _ = self.metadata.put_chunk_location(&location);
            }
        }

        Response::Ok { data: None }
    }

    /// Handle replicate chunk request (replication from another node)
    async fn handle_replicate_chunk(
        &self,
        chunk_id: ChunkId,
        data: Vec<u8>,
        checksum: [u8; 32],
    ) -> Response {
        debug!(
            "Handling replicate chunk: {} ({} bytes)",
            chunk_id,
            data.len()
        );

        // Same as write, but this is a replication request
        self.handle_write_chunk(chunk_id, data, checksum).await
    }

    /// Handle replicate metadata request (metadata replication from another node)
    async fn handle_replicate_metadata(&self, metadata: FileMetadata) -> Response {
        debug!("Handling replicate metadata: {}", metadata.path);

        // Store metadata locally without re-replicating (to avoid loops)
        match self.metadata.put_file(&metadata) {
            Ok(_) => {
                debug!("Successfully replicated metadata for {}", metadata.path);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to replicate metadata: {}", e);
                Response::Error {
                    message: format!("Failed to replicate metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle delete metadata replication (internal cluster operation)
    async fn handle_delete_metadata(&self, file_id: FileId, path: String) -> Response {
        debug!("Handling delete metadata: {} (file_id: {})", path, file_id);

        // CRITICAL: Only delete the path index, not the file metadata!
        // This is used during rename to clean up the old path on replicas
        // If we use delete_file(&file_id), we'll delete the NEW metadata that was just replicated!
        // delete_file() would:
        //   1. Look up metadata by file_id (finds the NEW path in metadata)
        //   2. Delete path index for the CURRENT path in metadata (NEW path)
        //   3. Delete the file_id entry (deletes ALL metadata)
        // Instead, we just delete the specific old path index
        match self.metadata.delete_path_index(&path) {
            Ok(_) => {
                debug!("Successfully deleted path index for {}", path);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to delete path index: {}", e);
                Response::Error {
                    message: format!("Failed to delete path index: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle replicate chunk location (internal cluster operation)
    async fn handle_replicate_chunk_location(&self, location: ChunkLocation) -> Response {
        info!("Handling replicate chunk location: {} (nodes: {:?})", location.chunk_id, location.nodes);

        // MERGE chunk location with existing metadata instead of replacing
        // This ensures all servers know about all replicas
        let merged_location = match self.metadata.get_chunk_location(&location.chunk_id) {
            Ok(Some(existing)) => {
                // Merge node lists - combine and deduplicate
                let mut merged_nodes = existing.nodes.clone();
                for node in &location.nodes {
                    if !merged_nodes.contains(node) {
                        merged_nodes.push(*node);
                    }
                }
                info!("Merging chunk location: {} existing nodes + {} new nodes = {} total",
                      existing.nodes.len(), location.nodes.len(), merged_nodes.len());

                ChunkLocation {
                    chunk_id: location.chunk_id,
                    nodes: merged_nodes,
                    size: location.size,
                    checksum: location.checksum,
                    file_offset: location.file_offset,  // Preserve existing offset
                }
            }
            Ok(None) => {
                info!("Creating new chunk location for {}", location.chunk_id);
                location.clone()
            }
            Err(e) => {
                warn!("Failed to get existing chunk location: {}, using new location", e);
                location.clone()
            }
        };

        // Store merged location
        match self.metadata.put_chunk_location(&merged_location) {
            Ok(_) => {
                info!("Successfully replicated chunk location for {} (total nodes: {})",
                      merged_location.chunk_id, merged_location.nodes.len());
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to replicate chunk location: {}", e);
                Response::Error {
                    message: format!("Failed to replicate chunk location: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle prefetch hint - warm cache with requested chunks (best-effort, low priority)
    /// Client sends this when it detects sequential reads to minimize future latency
    ///
    /// This runs in background with:
    /// - Concurrency limiting (max 2 concurrent prefetches via semaphore)
    /// - Throttling (50ms delay between chunks to spread I/O load)
    /// - Real client reads bypass the semaphore (they are high priority)
    async fn handle_prefetch_hint(&self, chunk_ids: Vec<ChunkId>) -> Response {
        info!("Received prefetch hint for {} chunks", chunk_ids.len());

        let storage = self.storage.clone();
        let semaphore = self.prefetch_semaphore.clone();
        let chunk_ids_clone = chunk_ids.clone();

        // Spawn background task to warm cache (non-blocking, best-effort, low priority)
        tokio::spawn(async move {
            let mut warmed = 0;
            let mut failed = 0;
            let mut skipped = 0;

            for chunk_id in chunk_ids_clone {
                // Acquire semaphore permit to limit concurrent prefetch operations
                // This prevents prefetch from overwhelming disk I/O
                let permit = match semaphore.try_acquire() {
                    Ok(p) => p,
                    Err(_) => {
                        // Too many prefetches in flight, skip this chunk
                        skipped += 1;
                        debug!("Skipping prefetch for chunk {} (too many in flight)", chunk_id);
                        continue;
                    }
                };

                match storage.warm_cache(&chunk_id) {
                    Ok(true) => {
                        warmed += 1;
                        debug!("Warmed cache for chunk {}", chunk_id);
                    }
                    Ok(false) => {
                        debug!("Chunk {} already in cache", chunk_id);
                    }
                    Err(e) => {
                        failed += 1;
                        debug!("Failed to warm cache for chunk {}: {}", chunk_id, e);
                    }
                }

                drop(permit); // Release semaphore

                // Minimal throttle to prevent CPU spinning, but allow aggressive prefetch
                // HDD read-ahead and OS page cache make sequential reads efficient
                tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            }

            if warmed > 0 || failed > 0 || skipped > 0 {
                info!("Prefetch completed: {} warmed, {} failed, {} skipped", warmed, failed, skipped);
            }
        });

        // Return immediately - prefetch happens in background
        Response::PrefetchAccepted {
            accepted: chunk_ids.len(),
        }
    }

    /// Handle delete chunk request
    async fn handle_delete_chunk(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling delete chunk: {}", chunk_id);

        match self.storage.delete_chunk(&chunk_id) {
            Ok(_) => Response::Ok { data: None },
            Err(e) => {
                warn!("Failed to delete chunk {}: {}", chunk_id, e);
                Response::Error {
                    message: format!("Failed to delete chunk: {}", e),
                    code: ErrorCode::IOError,
                }
            }
        }
    }

    /// Handle has chunk request
    async fn handle_has_chunk(&self, chunk_id: ChunkId) -> Response {
        let exists = self.storage.has_chunk(&chunk_id);
        Response::Bool { value: exists }
    }

    /// Write data to the cluster with replication
    pub async fn write_data(&self, data: &[u8]) -> Result<Vec<(ChunkId, u64)>> {
        let total_start = std::time::Instant::now();
        info!("Writing {} bytes to cluster", data.len());

        // Chunk the data
        let chunk_start = std::time::Instant::now();
        let chunks = self.chunker.chunk_data(data);
        let chunk_time = chunk_start.elapsed();
        info!("Chunking took {:?} for {} chunks", chunk_time, chunks.len());

        // Process ALL chunks in parallel for maximum throughput
        let mut chunk_tasks = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            let cluster = self.cluster.clone();
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let client = self.client.clone();
            let replication_factor = self.replication_factor;

            // Spawn a task for each chunk
            let task = tokio::spawn(async move {
                let chunk_total_start = std::time::Instant::now();

                // Determine target nodes using capacity-aware placement
                // This prefers nodes with more available space
                let target_nodes = cluster
                    .get_nodes_with_capacity_awareness(&chunk_id, replication_factor)
                    .await;

                if target_nodes.is_empty() {
                    anyhow::bail!("No nodes available for chunk {}", chunk_id);
                }

                debug!(
                    "Replicating chunk {} to {} nodes",
                    chunk_id,
                    target_nodes.len()
                );

                // Optimized replication strategy:
                // - RF=2: Write to 2 nodes synchronously (quorum=2)
                // - RF=3: Write to 2 nodes synchronously, 3rd replica happens in background
                // This reduces client bandwidth and network hops for RF=3
                let immediate_replicas = if replication_factor >= 3 {
                    2  // For RF=3+, only write 2 copies immediately
                } else {
                    replication_factor  // For RF=1 or RF=2, write all immediately
                };

                let quorum = immediate_replicas;

                // Spawn parallel replication tasks
                let mut quorum_tasks = Vec::new();
                let mut async_tasks = Vec::new();

                for (idx, node_id) in target_nodes.iter().enumerate() {
                    let node_id = *node_id;
                    let chunk_id = chunk_id;
                    let chunk_data = chunk_data.clone();

                    // First 'quorum' nodes: wait for these
                    // Remaining nodes: fire-and-forget (async replication)
                    let is_quorum_node = idx < quorum;

                    if node_id == cluster.local_node_id() {
                        // Local write
                        let storage = storage.clone();
                        let task = tokio::spawn(async move {
                            storage.write_chunk(&chunk_id, &chunk_data).is_ok()
                        });

                        if is_quorum_node {
                            quorum_tasks.push(task);
                        } else {
                            async_tasks.push(task);
                        }
                    } else {
                        // Remote write
                        let cluster = cluster.clone();
                        let client = client.clone();

                        let task = tokio::spawn(async move {
                            if let Some(node_info) = cluster.get_node(&node_id).await {
                                let request = Request::ReplicateChunk {
                                    chunk_id,
                                    data: chunk_data,
                                    checksum: chunk_id.hash,
                                };

                                match client
                                    .send_message(node_info.addr, Message::Request(request))
                                    .await
                                {
                                    Ok(response) => matches!(
                                        response.message,
                                        Message::Response(Response::Ok { .. })
                                    ),
                                    Err(_) => false,
                                }
                            } else {
                                false
                            }
                        });

                        if is_quorum_node {
                            quorum_tasks.push(task);
                        } else {
                            async_tasks.push(task);
                        }
                    }
                }

                // Wait ONLY for quorum tasks (fast path)
                let quorum_start = std::time::Instant::now();
                let mut success_count = 0;
                for task in quorum_tasks {
                    if let Ok(true) = task.await {
                        success_count += 1;
                    }
                }
                let quorum_time = quorum_start.elapsed();

                if success_count < quorum {
                    anyhow::bail!(
                        "Failed to achieve quorum for chunk {} ({}/{})",
                        chunk_id,
                        success_count,
                        quorum
                    );
                }

                info!("Chunk {} quorum write took {:?} ({} nodes)", chunk_id, quorum_time, success_count);

                // Async tasks continue in background - no waiting!
                // Auto-healing will catch any failures later
                debug!(
                    "Chunk {} written to quorum ({} nodes), {} additional replicas in progress",
                    chunk_id,
                    success_count,
                    async_tasks.len()
                );

                // Store chunk location metadata
                let location = ChunkLocation {
                    chunk_id,
                    nodes: target_nodes.clone(),
                    size: chunk_data.len(),
                    checksum: chunk_id.hash,
                    file_offset: None,  // Server-side replication doesn't track file offsets
                };

                let metadata_start = std::time::Instant::now();
                metadata
                    .put_chunk_location(&location)
                    .context("Failed to store chunk location")?;
                let metadata_time = metadata_start.elapsed();

                // Replicate chunk location metadata to all other nodes asynchronously
                // This ensures all servers know about chunk locations for consistency
                let nodes = cluster.get_all_nodes().await;
                let local_id = cluster.local_node_id();

                info!("Replicating chunk location for {} to {} nodes", chunk_id, nodes.len() - 1);

                for node in nodes {
                    // Skip self and offline nodes
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }

                    let client_clone = client.clone();
                    let location_clone = location.clone();
                    let node_addr = node.addr;
                    let node_id = node.id;
                    let chunk_id_clone = chunk_id;

                    // Fire-and-forget: spawn individual replication tasks
                    tokio::spawn(async move {
                        info!("Sending chunk location {} to node {}", chunk_id_clone, node_id);
                        let request = Request::ReplicateChunkLocation {
                            location: location_clone,
                        };

                        if let Err(e) = client_clone.send_message(node_addr, Message::Request(request)).await {
                            warn!("Failed to replicate chunk location {} to node {}: {}", chunk_id_clone, node_id, e);
                        } else {
                            info!("Successfully sent chunk location {} to node {}", chunk_id_clone, node_id);
                        }
                    });
                }

                let chunk_total_time = chunk_total_start.elapsed();
                info!("Chunk {} complete in {:?} (metadata: {:?})", chunk_id, chunk_total_time, metadata_time);

                Ok::<(ChunkId, u64), anyhow::Error>((chunk_id, chunk_data.len() as u64))
            });

            chunk_tasks.push(task);
        }

        // Wait for all chunk tasks to complete in parallel
        let gather_start = std::time::Instant::now();
        let mut chunk_ids_with_sizes = Vec::new();
        for task in chunk_tasks {
            match task.await {
                Ok(Ok(chunk_id_with_size)) => chunk_ids_with_sizes.push(chunk_id_with_size),
                Ok(Err(e)) => return Err(e),
                Err(e) => anyhow::bail!("Chunk task panicked: {}", e),
            }
        }
        let gather_time = gather_start.elapsed();

        let total_time = total_start.elapsed();
        let throughput = (data.len() as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Write complete: {} bytes in {:?} ({:.2} MB/s) - gather: {:?}",
              data.len(), total_time, throughput, gather_time);

        info!("Successfully wrote {} chunks", chunk_ids_with_sizes.len());
        Ok(chunk_ids_with_sizes)
    }

    /// Write data locally only (no replication)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    /// Healing creates the 3rd replica in background
    pub async fn write_data_local_only(&self, data: &[u8]) -> Result<Vec<(ChunkId, u64)>> {
        let total_start = std::time::Instant::now();
        info!("Writing {} bytes locally (no replication)", data.len());

        // Chunk the data
        let chunks = self.chunker.chunk_data(data);
        info!("Chunked into {} chunks (local write only)", chunks.len());

        // Write all chunks locally in parallel
        let mut chunk_tasks = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            let storage = self.storage.clone();
            let metadata = self.metadata.clone();
            let cluster = self.cluster.clone();
            let client = self.client.clone();
            let local_node_id = self.cluster.local_node_id();

            let task = tokio::spawn(async move {
                // Write chunk locally
                storage.write_chunk(&chunk_id, &chunk_data)
                    .context(format!("Failed to write chunk {} locally", chunk_id))?;

                // Store chunk location metadata (with only local node)
                let location = ChunkLocation {
                    chunk_id,
                    nodes: vec![local_node_id],  // Only local node
                    size: chunk_data.len(),
                    checksum: chunk_id.hash,
                    file_offset: None,  // Server-side local-only writes don't track file offsets
                };

                metadata.put_chunk_location(&location)
                    .context("Failed to store chunk location")?;

                // Replicate chunk location metadata to all other nodes asynchronously
                let nodes = cluster.get_all_nodes().await;
                let local_id = cluster.local_node_id();

                info!("Replicating chunk location for {} to {} nodes", chunk_id, nodes.len() - 1);

                for node in nodes {
                    // Skip self and offline nodes
                    if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                        continue;
                    }

                    let client_clone = client.clone();
                    let location_clone = location.clone();
                    let node_addr = node.addr;
                    let node_id = node.id;
                    let chunk_id_clone = chunk_id;

                    // Fire-and-forget: spawn individual replication tasks
                    tokio::spawn(async move {
                        info!("Sending chunk location {} to node {}", chunk_id_clone, node_id);
                        let request = Request::ReplicateChunkLocation {
                            location: location_clone,
                        };

                        if let Err(e) = client_clone.send_message(node_addr, Message::Request(request)).await {
                            warn!("Failed to replicate chunk location {} to node {}: {}", chunk_id_clone, node_id, e);
                        } else {
                            info!("Successfully sent chunk location {} to node {}", chunk_id_clone, node_id);
                        }
                    });
                }

                Ok::<(ChunkId, u64), anyhow::Error>((chunk_id, chunk_data.len() as u64))
            });

            chunk_tasks.push(task);
        }

        // Wait for all chunks to complete
        let mut chunk_ids_with_sizes = Vec::new();
        for task in chunk_tasks {
            match task.await {
                Ok(Ok(chunk_id_with_size)) => chunk_ids_with_sizes.push(chunk_id_with_size),
                Ok(Err(e)) => return Err(e),
                Err(e) => anyhow::bail!("Chunk task panicked: {}", e),
            }
        }

        let total_time = total_start.elapsed();
        let throughput = (data.len() as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Local write complete: {} bytes in {:?} ({:.2} MB/s) - {} chunks",
              data.len(), total_time, throughput, chunk_ids_with_sizes.len());

        Ok(chunk_ids_with_sizes)
    }

    /// Read data from the cluster by chunk IDs
    pub async fn read_data(&self, chunk_ids: &[ChunkId]) -> Result<Vec<u8>> {
        info!("Reading {} chunks from cluster", chunk_ids.len());

        let mut all_chunks = Vec::new();

        for chunk_id in chunk_ids {
            let chunk_data = self.read_chunk(chunk_id).await?;
            all_chunks.push(chunk_data);
        }

        // Reassemble chunks
        let data = self.chunker.reassemble_chunks(all_chunks);

        info!("Successfully read {} bytes", data.len());
        Ok(data)
    }

    /// Read a single chunk from the cluster
    async fn read_chunk(&self, chunk_id: &ChunkId) -> Result<Vec<u8>> {
        // Try reading from local storage (OS page cache handles caching automatically)
        if let Ok(data) = self.storage.read_chunk(chunk_id) {
            debug!("Read chunk {} from local storage", chunk_id);
            return Ok(data);
        }

        // Get chunk location from metadata
        let location = self
            .metadata
            .get_chunk_location(chunk_id)
            .context("Failed to get chunk location")?
            .ok_or_else(|| anyhow::anyhow!("Chunk location not found"))?;

        // Try reading from remote nodes
        for node_id in &location.nodes {
            if node_id == &self.cluster.local_node_id() {
                continue; // Already tried local
            }

            if let Some(node_info) = self.cluster.get_node(node_id).await {
                let request = Request::ReadChunk {
                    chunk_id: *chunk_id,
                    sequential_hint: None,
                };

                match self
                    .client
                    .send_message(node_info.addr, Message::Request(request))
                    .await
                {
                    Ok(response) => match response.message {
                        Message::Response(Response::ChunkData { data, .. }) => {
                            debug!("Read chunk {} from remote node {}", chunk_id, node_id);
                            return Ok(data);
                        }
                        _ => continue,
                    },
                    Err(e) => {
                        warn!("Failed to read from node {}: {}", node_id, e);
                        continue;
                    }
                }
            }
        }

        anyhow::bail!("Failed to read chunk {} from any node", chunk_id)
    }

    /// Get or create chunk location metadata
    async fn get_or_create_chunk_location(
        &self,
        chunk_id: &ChunkId,
        size: usize,
    ) -> Result<ChunkLocation> {
        if let Ok(Some(location)) = self.metadata.get_chunk_location(chunk_id) {
            Ok(location)
        } else {
            Ok(ChunkLocation {
                chunk_id: *chunk_id,
                nodes: Vec::new(),
                size,
                checksum: chunk_id.hash,
                file_offset: None,  // Legacy fallback when metadata not found
            })
        }
    }

    /// Handle get file metadata by path request
    async fn handle_get_file_metadata_by_path(&self, path: String, if_modified_since: Option<u64>) -> Response {
        debug!("Handling get file metadata by path: {} (if_modified_since: {:?})", path, if_modified_since);

        // Try local first
        match self.metadata.get_file_by_path(&path) {
            Ok(Some(mut metadata)) => {
                // Check if client has provided if_modified_since timestamp
                if let Some(cached_timestamp) = if_modified_since {
                    // Return NotModified if metadata hasn't changed
                    if metadata.modified_at <= cached_timestamp {
                        debug!("Metadata not modified for {}: {} <= {}", path, metadata.modified_at, cached_timestamp);
                        return Response::NotModified;
                    }
                }

                // DO NOT backfill chunk_locations here - it causes massive metadata bloat
                // Instead, let client query chunk locations on-demand or use replica cache
                // The "No chunk_locations" warnings are expected for legacy files and handled gracefully

                Response::FileMetadata { metadata }
            }
            Ok(None) => {
                // Not found locally - with metadata replication, this means file doesn't exist
                // Don't query other nodes for performance (replication ensures consistency)
                Response::Error {
                    message: "File not found".to_string(),
                    code: ErrorCode::NotFound,
                }
            }
            Err(e) => {
                warn!("Failed to get file metadata: {}", e);
                Response::Error {
                    message: format!("Failed to get file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle put file metadata request
    async fn handle_put_file_metadata(&self, metadata: FileMetadata) -> Response {
        debug!("Handling put file metadata: {}", metadata.path);

        // Store metadata locally first
        match self.metadata.put_file(&metadata) {
            Ok(_) => {
                // Replicate metadata to all other nodes asynchronously with timeout
                let cluster = self.cluster.clone();
                let client = self.client.clone();
                let metadata_clone = metadata.clone();

                tokio::spawn(async move {
                    let nodes = cluster.get_all_nodes().await;
                    let local_id = cluster.local_node_id();

                    // Limit to max 10 concurrent replications to prevent storms
                    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(10));
                    let mut tasks = Vec::new();

                    for node in nodes {
                        // Skip self and offline nodes
                        if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                            continue;
                        }

                        let request = Request::ReplicateMetadata {
                            metadata: metadata_clone.clone(),
                        };

                        let client_clone = client.clone();
                        let node_addr = node.addr;
                        let semaphore_clone = semaphore.clone();

                        let task = tokio::spawn(async move {
                            // Acquire semaphore permit
                            let _permit = semaphore_clone.acquire().await.ok()?;

                            // Replicate with 5 second timeout
                            let result = tokio::time::timeout(
                                tokio::time::Duration::from_secs(5),
                                client_clone.send_message(node_addr, Message::Request(request))
                            ).await;

                            match result {
                                Ok(Ok(_)) => {
                                    debug!("Replicated metadata to {}", node_addr);
                                    Some(())
                                }
                                Ok(Err(e)) => {
                                    debug!("Failed to replicate metadata to {}: {}", node_addr, e);
                                    None
                                }
                                Err(_) => {
                                    debug!("Timeout replicating metadata to {}", node_addr);
                                    None
                                }
                            }
                        });

                        tasks.push(task);
                    }

                    // Don't wait for all tasks, just spawn them
                    drop(tasks);
                });

                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to put file metadata: {}", e);
                Response::Error {
                    message: format!("Failed to put file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle list directory request
    async fn handle_list_directory(&self, path: String) -> Response {
        debug!("Handling list directory: {}", path);

        // ALWAYS return local only for performance
        // Metadata replication ensures all nodes have the same data
        match self.metadata.list_directory(&path) {
            Ok(entries) => Response::DirectoryListing { entries },
            Err(e) => {
                warn!("Failed to list directory: {}", e);
                Response::Error {
                    message: format!("Failed to list directory: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle write file request (client writes entire file)
    async fn handle_write_file(&self, data: Vec<u8>) -> Response {
        debug!("Handling write file: {} bytes", data.len());

        match self.write_data(&data).await {
            Ok(chunk_ids_with_sizes) => {
                // Separate chunk IDs from sizes
                let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes.iter().map(|(id, _)| *id).collect();
                let chunk_sizes: Vec<u64> = chunk_ids_with_sizes.iter().map(|(_, size)| *size).collect();
                Response::ChunkIds { chunk_ids, chunk_sizes }
            }
            Err(e) => {
                warn!("Failed to write file: {}", e);
                Response::Error {
                    message: format!("Failed to write file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle write file request (local only, no replication)
    /// Used for optimized RF=3+ writes where client sends to 2 servers in parallel
    async fn handle_write_file_local_only(&self, data: Vec<u8>) -> Response {
        debug!("Handling write file local only: {} bytes", data.len());

        match self.write_data_local_only(&data).await {
            Ok(chunk_ids_with_sizes) => {
                let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes.iter().map(|(id, _)| *id).collect();
                let chunk_sizes: Vec<u64> = chunk_ids_with_sizes.iter().map(|(_, size)| *size).collect();
                info!("Wrote {} chunks locally (no replication)", chunk_ids.len());
                Response::ChunkIds { chunk_ids, chunk_sizes }
            }
            Err(e) => {
                warn!("Failed to write file locally: {}", e);
                Response::Error {
                    message: format!("Failed to write file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle delete file request
    async fn handle_delete_file(&self, path: String) -> Response {
        debug!("Handling delete file: {}", path);

        // Get file metadata first to find chunks
        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                // Get chunk IDs before deleting metadata
                let chunk_ids = metadata.chunks.clone();

                // Delete the file metadata
                match self.metadata.delete_file(&metadata.id) {
                    Ok(_) => {
                        // Replicate metadata deletion to all other nodes asynchronously
                        let cluster = self.cluster.clone();
                        let client = self.client.clone();
                        let storage = self.storage.clone();
                        let metadata_store = self.metadata.clone();
                        let file_id = metadata.id;
                        let path_clone = path.clone();

                        tokio::spawn(async move {
                            // First, replicate the metadata deletion to all nodes
                            let nodes = cluster.get_all_nodes().await;
                            let local_id = cluster.local_node_id();

                            info!("Replicating metadata deletion for file: {}", path_clone);

                            for node in &nodes {
                                // Skip self and offline nodes
                                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                    continue;
                                }

                                let request = Request::DeleteMetadata {
                                    file_id,
                                    path: path_clone.clone(),
                                };

                                if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                                    warn!("Failed to replicate metadata deletion to node {}: {}", node.id, e);
                                }
                            }

                            // Then delete chunks from all nodes asynchronously
                            info!("Deleting {} chunks for file: {}", chunk_ids.len(), path);

                            for chunk_id in &chunk_ids {
                                // Get chunk location
                                let location = match metadata_store.get_chunk_location(chunk_id) {
                                    Ok(Some(loc)) => loc,
                                    _ => continue,
                                };

                                // Delete from all nodes that have it
                                for node_id in &location.nodes {
                                    if *node_id == cluster.local_node_id() {
                                        // Delete locally
                                        if let Err(e) = storage.delete_chunk(chunk_id) {
                                            warn!("Failed to delete local chunk {}: {}", chunk_id, e);
                                        }
                                    } else {
                                        // Delete from remote node
                                        if let Some(node_info) = cluster.get_node(node_id).await {
                                            let request = Request::DeleteChunk {
                                                chunk_id: *chunk_id,
                                            };

                                            if let Err(e) = client
                                                .send_message(node_info.addr, Message::Request(request))
                                                .await
                                            {
                                                warn!(
                                                    "Failed to delete chunk {} from node {}: {}",
                                                    chunk_id, node_id, e
                                                );
                                            }
                                        }
                                    }
                                }
                            }

                            info!("Chunk deletion complete for file: {}", path);
                        });

                        Response::Ok { data: None }
                    }
                    Err(e) => {
                        warn!("Failed to delete file metadata: {}", e);
                        Response::Error {
                            message: format!("Failed to delete file: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
            }
            Ok(None) => Response::Error {
                message: "File not found".to_string(),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to find file: {}", e);
                Response::Error {
                    message: format!("Failed to delete file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle get cluster status request
    async fn handle_get_cluster_status(&self) -> Response {
        debug!("Handling get cluster status");

        let nodes = self.cluster.get_all_nodes().await;
        let healthy_nodes = nodes
            .iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .count();
        let total_nodes = nodes.len();
        let chunk_size_mb = self.chunker.chunk_size() / (1024 * 1024);

        Response::ClusterStatus {
            nodes,
            total_nodes,
            healthy_nodes,
            chunk_size_mb,
        }
    }

    /// Handle get storage stats request
    async fn handle_get_storage_stats(&self) -> Response {
        debug!("Handling get storage stats");

        const CACHE_TTL_SECS: u64 = 10;

        // Check cache first
        {
            let cache_read = self.storage_stats_cache.read().await;
            if let Some(cached) = cache_read.as_ref() {
                if cached.timestamp.elapsed().as_secs() < CACHE_TTL_SECS {
                    debug!("Storage stats cache HIT (age: {}s)", cached.timestamp.elapsed().as_secs());
                    let nodes_count = self.cluster.get_all_nodes().await.len();
                    let total_size = cached.total_space.saturating_sub(cached.available_space);

                    return Response::StorageStats {
                        total_chunks: cached.total_chunks,
                        total_size,
                        replication_factor: self.replication_factor,
                        nodes_count,
                        total_space: cached.total_space,
                        free_space: cached.free_space,
                        available_space: cached.available_space,
                    };
                }
            }
        }

        // Cache miss - calculate stats
        debug!("Storage stats cache MISS");

        let nodes_count = self.cluster.get_all_nodes().await.len();

        // Get filesystem statistics (fast - just statvfs syscall)
        let (total_space, free_space, available_space) = match self.storage.get_filesystem_stats() {
            Ok(stats) => stats,
            Err(e) => {
                warn!("Failed to get storage stats: {}", e);
                return Response::Error {
                    message: format!("Failed to get storage stats: {}", e),
                    code: ErrorCode::InternalError,
                };
            }
        };

        // Calculate total_size as used space on filesystem
        let total_size = total_space.saturating_sub(available_space);

        // Update local node's capacity for placement decisions
        self.cluster.update_node_capacity(
            self.cluster.local_node_id(),
            available_space,
            total_space
        ).await;

        // Estimate chunk count from used space (4MB chunks)
        // This avoids expensive list_chunks() call for statfs queries
        const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
        let total_chunks = (total_size / CHUNK_SIZE) as usize;

        // Update cache
        {
            let mut cache_write = self.storage_stats_cache.write().await;
            *cache_write = Some(StorageStatsCache {
                total_chunks,
                total_space,
                free_space,
                available_space,
                timestamp: std::time::Instant::now(),
            });
        }

        Response::StorageStats {
            total_chunks,
            total_size,
            replication_factor: self.replication_factor,
            nodes_count,
            total_space,
            free_space,
            available_space,
        }
    }

    /// Handle get healing status request
    async fn handle_get_healing_status(&self) -> Response {
        debug!("Handling get healing status");

        // TODO: This requires access to HealingManager
        // For now, return basic response
        Response::HealingStatus {
            enabled: true,
            pending_count: 0,
            last_check: 0,
        }
    }

    /// Handle trigger scrub request
    async fn handle_trigger_scrub(&self) -> Response {
        debug!("Handling trigger scrub");

        // TODO: Implement scrub trigger
        Response::Ok { data: None }
    }

    /// Handle enable healing request
    async fn handle_enable_healing(&self) -> Response {
        debug!("Handling enable healing");

        // TODO: Implement healing enable
        Response::Ok { data: None }
    }

    /// Handle disable healing request
    async fn handle_disable_healing(&self) -> Response {
        debug!("Handling disable healing");

        // TODO: Implement healing disable
        Response::Ok { data: None }
    }

    /// Handle trigger healing request
    async fn handle_trigger_healing(&self) -> Response {
        debug!("Handling trigger healing");

        // TODO: Implement healing trigger
        Response::Ok { data: None }
    }

    /// Handle get file info request
    async fn handle_get_file_info(&self, path: String) -> Response {
        debug!("Handling get file info: {}", path);

        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                // Get chunk locations
                let mut chunk_locations = Vec::new();
                for chunk_id in &metadata.chunks {
                    if let Ok(Some(location)) = self.metadata.get_chunk_location(chunk_id) {
                        chunk_locations.push(location);
                    }
                }

                Response::FileInfo {
                    metadata,
                    chunk_locations,
                }
            }
            Ok(None) => Response::Error {
                message: "File not found".to_string(),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get file info: {}", e);
                Response::Error {
                    message: format!("Failed to get file info: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle get chunk replicas request
    async fn handle_get_chunk_replicas(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling get chunk replicas: {}", chunk_id);

        match self.metadata.get_chunk_location(&chunk_id) {
            Ok(Some(location)) => Response::ChunkReplicas {
                chunk_id,
                nodes: location.nodes,
            },
            Ok(None) => Response::Error {
                message: "Chunk not found".to_string(),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get chunk replicas: {}", e);
                Response::Error {
                    message: format!("Failed to get chunk replicas: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle list all files request
    async fn handle_list_all_files(&self) -> Response {
        debug!("Handling list all files");

        match self.metadata.list_files() {
            Ok(files) => {
                let total_count = files.len();
                Response::FileList { files, total_count }
            }
            Err(e) => {
                warn!("Failed to list files: {}", e);
                Response::Error {
                    message: format!("Failed to list files: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle purge file metadata request
    async fn handle_purge_file_metadata(&self, path: String) -> Response {
        info!("Handling purge file metadata: {}", path);

        // Get metadata to find file ID
        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                let file_id = metadata.id;

                // Delete from local metadata store only (not chunks)
                match self.metadata.delete_file(&file_id) {
                    Ok(_) => {
                        info!("Purged local metadata for file: {}", path);

                        // CRITICAL: Replicate metadata deletion to all other nodes
                        // This ensures rename operations don't leave stale metadata on other servers
                        let cluster = self.cluster.clone();
                        let client = self.client.clone();
                        let path_clone = path.clone();

                        tokio::spawn(async move {
                            let nodes = cluster.get_all_nodes().await;
                            let local_id = cluster.local_node_id();

                            info!("Replicating metadata purge for file: {}", path_clone);

                            for node in &nodes {
                                // Skip self and offline nodes
                                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                    continue;
                                }

                                let request = Request::DeleteMetadata {
                                    file_id,
                                    path: path_clone.clone(),
                                };

                                if let Err(e) = client.send_message(node.addr, Message::Request(request)).await {
                                    warn!("Failed to replicate metadata purge to node {}: {}", node.id, e);
                                }
                            }
                        });

                        Response::Ok { data: None }
                    }
                    Err(e) => {
                        warn!("Failed to purge file metadata: {}", e);
                        Response::Error {
                            message: format!("Failed to purge file metadata: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
            }
            Ok(None) => Response::Error {
                message: format!("File not found: {}", path),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to get file metadata: {}", e);
                Response::Error {
                    message: format!("Failed to get file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_purge_file_metadata_by_id(&self, file_id: FileId) -> Response {
        info!("Handling purge file metadata by ID: {}", file_id);

        // Delete from local node first
        match self.metadata.delete_file(&file_id) {
            Ok(_) => {
                info!("Purged metadata for file ID {} from local node", file_id);

                // Broadcast deletion to all other nodes in the cluster
                let nodes = self.cluster.get_all_nodes().await;
                let local_id = self.cluster.local_node_id();
                let client = self.client.clone();

                let mut success_count = 1; // Local deletion succeeded
                let total_nodes = nodes.len();

                for node in nodes {
                    if node.id == local_id {
                        continue; // Skip local node
                    }

                    // Send purge request to remote node
                    match client.send_message(
                        node.addr,
                        Message::Request(Request::PurgeFileMetadataById { file_id: file_id.clone() })
                    ).await {
                        Ok(envelope) => {
                            match envelope.message {
                                Message::Response(Response::Ok { .. }) => {
                                    info!("Purged metadata for file ID {} from node {}", file_id, node.id);
                                    success_count += 1;
                                }
                                Message::Response(Response::Error { message, .. }) => {
                                    warn!("Failed to purge from node {}: {}", node.id, message);
                                }
                                _ => {
                                    warn!("Unexpected response from node {} during purge", node.id);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Error contacting node {} for purge: {}", node.id, e);
                        }
                    }
                }

                info!("Purged metadata from {}/{} nodes", success_count, total_nodes);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to purge file metadata by ID: {}", e);
                Response::Error {
                    message: format!("Failed to purge file metadata: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    /// Handle atomic rename file request
    /// This is critical - must update metadata path AND delete old path atomically
    /// to prevent file from disappearing during rename
    async fn handle_rename_file(&self, old_path: String, new_path: String) -> Response {
        info!("Handling atomic rename: {} -> {}", old_path, new_path);

        // Get existing metadata
        match self.metadata.get_file_by_path(&old_path) {
            Ok(Some(mut metadata)) => {
                let file_id = metadata.id;

                // Update path and timestamp
                metadata.path = new_path.clone();
                metadata.modified_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // Store new metadata locally first
                match self.metadata.put_file(&metadata) {
                    Ok(_) => {
                        // Now replicate to all servers BEFORE deleting old path
                        // This ensures the new metadata exists everywhere before we delete the old
                        let nodes = self.cluster.get_all_nodes().await;
                        let local_id = self.cluster.local_node_id();
                        let client = self.client.clone();
                        let metadata_clone = metadata.clone();
                        let old_path_clone = old_path.clone();

                        // Replicate new metadata synchronously
                        let mut put_success = true;
                        for node in &nodes {
                            if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                continue;
                            }

                            let put_request = Request::ReplicateMetadata {
                                metadata: metadata_clone.clone(),
                            };

                            if let Err(e) = client.send_message(node.addr, Message::Request(put_request)).await {
                                warn!("Failed to replicate new metadata to {}: {}", node.addr, e);
                                put_success = false;
                            }
                        }

                        if !put_success {
                            warn!("Some replications failed for rename {} -> {}", old_path, new_path);
                        }

                        // Now delete the OLD path index entry locally
                        // We use delete_path_index() instead of delete_file() because:
                        // - put_file() already updated the file_id → metadata entry
                        // - put_file() already created the new_path → file_id entry
                        // - We just need to remove the old_path → file_id entry
                        // - delete_file() would delete EVERYTHING including the new metadata!
                        if let Err(e) = self.metadata.delete_path_index(&old_path) {
                            warn!("Failed to delete old path index during rename: {}", e);
                        }

                        // Replicate deletion of old path to all servers
                        tokio::spawn(async move {
                            for node in &nodes {
                                if node.id == local_id || node.status != dfs_common::NodeStatus::Online {
                                    continue;
                                }

                                let delete_request = Request::DeleteMetadata {
                                    file_id,
                                    path: old_path_clone.clone(),
                                };

                                if let Err(e) = client.send_message(node.addr, Message::Request(delete_request)).await {
                                    warn!("Failed to replicate old metadata deletion to {}: {}", node.addr, e);
                                }
                            }
                        });

                        info!("Renamed {} -> {} (file_id: {})", old_path, new_path, file_id);
                        Response::Ok { data: None }
                    }
                    Err(e) => {
                        warn!("Failed to store new metadata during rename: {}", e);
                        Response::Error {
                            message: format!("Failed to rename file: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
            }
            Ok(None) => Response::Error {
                message: format!("File not found: {}", old_path),
                code: ErrorCode::NotFound,
            },
            Err(e) => {
                warn!("Failed to find file for rename: {}", e);
                Response::Error {
                    message: format!("Failed to rename file: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }

    async fn handle_remove_node(&self, node_id: NodeId) -> Response {
        info!("Handling remove node request: {}", node_id);

        // Check if node exists
        if self.cluster.get_node(&node_id).await.is_none() {
            return Response::Error {
                message: format!("Node {} not found in cluster", node_id),
                code: ErrorCode::NotFound,
            };
        }

        // Remove from cluster
        match self.cluster.remove_node(&node_id).await {
            Ok(_) => {
                info!("Successfully removed node {} from cluster", node_id);
                Response::Ok { data: None }
            }
            Err(e) => {
                warn!("Failed to remove node {}: {}", node_id, e);
                Response::Error {
                    message: format!("Failed to remove node: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_server_write_read_local() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        // Write data
        let data = b"Hello, distributed filesystem!";
        let chunk_ids_with_sizes = server.write_data(data).await.unwrap();

        assert!(!chunk_ids_with_sizes.is_empty());

        // Read data back
        let chunk_ids: Vec<ChunkId> = chunk_ids_with_sizes.iter().map(|(id, _)| *id).collect();
        let read_data = server.read_data(&chunk_ids).await.unwrap();
        assert_eq!(data.as_slice(), read_data.as_slice());
    }

    #[tokio::test]
    async fn test_handle_write_read_chunk() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        // Test write
        let data = b"Test chunk data";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        let response = server
            .handle_write_chunk(chunk_id, data.to_vec(), hash)
            .await;

        match response {
            Response::Ok { .. } => {}
            _ => panic!("Expected Ok response"),
        }

        // Test read
        let response = server.handle_read_chunk(chunk_id).await;

        match response {
            Response::ChunkData { data: read_data, .. } => {
                assert_eq!(data.as_slice(), read_data.as_slice());
            }
            _ => panic!("Expected ChunkData response"),
        }
    }

    #[tokio::test]
    async fn test_handle_has_chunk() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();
        let temp_metadata_dir = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3, temp_metadata_dir.path().to_path_buf());

        let data = b"Test";
        let hash = compute_chunk_hash(data);
        let chunk_id = ChunkId::from_hash(hash);

        // Should not exist yet
        let response = server.handle_has_chunk(chunk_id).await;
        match response {
            Response::Bool { value } => assert!(!value),
            _ => panic!("Expected Bool response"),
        }

        // Write chunk
        server
            .handle_write_chunk(chunk_id, data.to_vec(), hash)
            .await;

        // Should exist now
        let response = server.handle_has_chunk(chunk_id).await;
        match response {
            Response::Bool { value } => assert!(value),
            _ => panic!("Expected Bool response"),
        }
    }
}

/// Implement MessageHandler trait for Server
impl MessageHandler for Server {
    fn handle_request(
        &self,
        request: Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
        Box::pin(async move { self.handle_request(request).await })
    }

    fn handle_cluster_message(
        &self,
        message: ClusterMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
        Box::pin(async move {
            // Handle cluster messages (heartbeat, join, leave, etc.)
            match message {
                ClusterMessage::Heartbeat { node_info, cluster_view } => {
                    debug!("Received heartbeat from {} with {} gossip entries",
                           node_info.id, cluster_view.len());

                    // Update sender's heartbeat
                    if let Err(e) = self.cluster.update_heartbeat(&node_info.id).await {
                        warn!("Failed to update heartbeat: {}", e);
                    }

                    // Merge cluster view gossip if present
                    if !cluster_view.is_empty() {
                        if let Err(e) = self.cluster.merge_cluster_gossip(cluster_view).await {
                            warn!("Failed to merge cluster gossip: {}", e);
                        }
                    }

                    Response::Ok { data: None }
                }
                ClusterMessage::Join { node_info } => {
                    info!("Node {} joining cluster", node_info.id);
                    if let Err(e) = self.cluster.add_node(node_info).await {
                        warn!("Failed to add node: {}", e);
                        Response::Error {
                            message: format!("Failed to add node: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    } else {
                        Response::Ok { data: None }
                    }
                }
                ClusterMessage::Leave { node_id } => {
                    info!("Node {} leaving cluster", node_id);
                    if let Err(e) = self.cluster.remove_node(&node_id).await {
                        warn!("Failed to remove node: {}", e);
                    }
                    Response::Ok { data: None }
                }
                ClusterMessage::JoinRequest { node_info } => {
                    info!("Received join request from node {}", node_info.id);

                    // Add node to cluster
                    if let Err(e) = self.cluster.add_node(node_info.clone()).await {
                        warn!("Failed to add node: {}", e);
                        let response = ClusterMessage::JoinResponse {
                            accepted: false,
                            cluster_nodes: vec![],
                        };
                        return Response::Ok {
                            data: Some(bincode::serialize(&response).unwrap()),
                        };
                    }

                    // Get all cluster nodes
                    let cluster_nodes = self.cluster.get_all_nodes().await;

                    info!(
                        "Node {} joined cluster, now {} nodes total",
                        node_info.id,
                        cluster_nodes.len()
                    );

                    // Return success with cluster state
                    let response = ClusterMessage::JoinResponse {
                        accepted: true,
                        cluster_nodes,
                    };

                    Response::Ok {
                        data: Some(bincode::serialize(&response).unwrap()),
                    }
                }
                ClusterMessage::NodeJoined { node_info } => {
                    debug!("Node {} joined the cluster (broadcast)", node_info.id);

                    // Only add if not already known (prevents re-processing)
                    let already_known = self.cluster.get_node(&node_info.id).await.is_some();

                    if !already_known {
                        info!("New node {} joined the cluster", node_info.id);
                        if let Err(e) = self.cluster.add_node(node_info).await {
                            warn!("Failed to add node from broadcast: {}", e);
                        }

                        // Save updated peer list to disk
                        let peer_addrs = self.cluster.get_all_peer_addrs().await;
                        if let Err(e) = ClusterManager::save_persisted_peers(&peer_addrs, &self.metadata_dir).await {
                            warn!("Failed to save persisted peers after NodeJoined: {}", e);
                        }
                    } else {
                        debug!("Node {} already known, ignoring duplicate join", node_info.id);
                    }

                    // NO reciprocal announcements - prevents infinite loops
                    Response::Ok { data: None }
                }
                _ => Response::Error {
                    message: "Cluster message not implemented".to_string(),
                    code: ErrorCode::InternalError,
                },
            }
        })
    }
}
