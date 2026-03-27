use crate::chunker::Chunker;
use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::{MessageHandler, NetworkClient};
use crate::storage::ChunkStorage;
use anyhow::{Context, Result};
use dfs_common::{
    compute_chunk_hash, ChunkId, ChunkLocation, ClusterMessage, ErrorCode, FileMetadata, Message,
    NodeId, NodeInfo, Request, Response,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn};

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
            Request::ReadChunk { chunk_id } => self.handle_read_chunk(chunk_id).await,
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
            Request::GetFileMetadataByPath { path, if_modified_since } => {
                self.handle_get_file_metadata_by_path(path, if_modified_since).await
            }
            Request::PutFileMetadata { metadata } => {
                self.handle_put_file_metadata(metadata).await
            }
            Request::ListDirectory { path } => self.handle_list_directory(path).await,
            Request::WriteFile { data } => self.handle_write_file(data).await,
            Request::DeleteFile { path } => self.handle_delete_file(path).await,

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
            Ok(data) => Response::ChunkData { chunk_id, data },
            Err(e) => {
                warn!("Failed to read chunk {}: {}", chunk_id, e);
                Response::Error {
                    message: format!("Failed to read chunk: {}", e),
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
                };

                let metadata_start = std::time::Instant::now();
                metadata
                    .put_chunk_location(&location)
                    .context("Failed to store chunk location")?;
                let metadata_time = metadata_start.elapsed();

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
            })
        }
    }

    /// Handle get file metadata by path request
    async fn handle_get_file_metadata_by_path(&self, path: String, if_modified_since: Option<u64>) -> Response {
        debug!("Handling get file metadata by path: {} (if_modified_since: {:?})", path, if_modified_since);

        // Try local first
        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => {
                // Check if client has provided if_modified_since timestamp
                if let Some(cached_timestamp) = if_modified_since {
                    // Return NotModified if metadata hasn't changed
                    if metadata.modified_at <= cached_timestamp {
                        debug!("Metadata not modified for {}: {} <= {}", path, metadata.modified_at, cached_timestamp);
                        return Response::NotModified;
                    }
                }
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
                        // Delete chunks from all nodes asynchronously
                        let cluster = self.cluster.clone();
                        let client = self.client.clone();
                        let storage = self.storage.clone();
                        let metadata_store = self.metadata.clone();

                        tokio::spawn(async move {
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

        Response::ClusterStatus {
            nodes,
            total_nodes,
            healthy_nodes,
        }
    }

    /// Handle get storage stats request
    async fn handle_get_storage_stats(&self) -> Response {
        debug!("Handling get storage stats");

        match self.storage.list_chunks() {
            Ok(chunks) => {
                let nodes_count = self.cluster.get_all_nodes().await.len();

                // Get filesystem statistics (fast - just statvfs syscall)
                let (total_space, free_space, available_space) = self.storage.get_filesystem_stats()
                    .unwrap_or((0, 0, 0));

                // Calculate total_size as used space on filesystem
                // This is much faster than reading every chunk!
                let total_size = total_space.saturating_sub(available_space);

                // Update local node's capacity for placement decisions
                self.cluster.update_node_capacity(
                    self.cluster.local_node_id(),
                    available_space,
                    total_space
                ).await;

                Response::StorageStats {
                    total_chunks: chunks.len(),
                    total_size,
                    replication_factor: self.replication_factor,
                    nodes_count,
                    total_space,
                    free_space,
                    available_space,
                }
            }
            Err(e) => {
                warn!("Failed to get storage stats: {}", e);
                Response::Error {
                    message: format!("Failed to get storage stats: {}", e),
                    code: ErrorCode::InternalError,
                }
            }
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
                ClusterMessage::Heartbeat { node_info } => {
                    debug!("Received heartbeat from {}", node_info.id);
                    if let Err(e) = self.cluster.update_heartbeat(&node_info.id).await {
                        warn!("Failed to update heartbeat: {}", e);
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
