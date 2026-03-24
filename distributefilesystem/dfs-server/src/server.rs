use crate::chunker::Chunker;
use crate::cluster::ClusterManager;
use crate::metadata::MetadataStore;
use crate::network::{MessageHandler, NetworkClient};
use crate::storage::ChunkStorage;
use anyhow::{Context, Result};
use dfs_common::{
    compute_chunk_hash, ChunkId, ChunkLocation, ClusterMessage, ErrorCode, FileMetadata, Message,
    NodeId, Request, Response,
};
use std::net::SocketAddr;
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
}

impl Server {
    /// Create a new server instance
    pub fn new(
        storage: Arc<ChunkStorage>,
        metadata: Arc<MetadataStore>,
        chunk_size: usize,
        cluster: Arc<ClusterManager>,
        replication_factor: usize,
    ) -> Self {
        Self {
            storage,
            metadata,
            chunker: Arc::new(Chunker::new(chunk_size)),
            cluster,
            client: Arc::new(NetworkClient::new()),
            replication_factor,
        }
    }

    /// Get reference to cluster manager
    pub fn cluster(&self) -> Arc<ClusterManager> {
        self.cluster.clone()
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
            Request::GetFileMetadataByPath { path } => {
                self.handle_get_file_metadata_by_path(path).await
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

            _ => Response::Error {
                message: "Request type not yet implemented".to_string(),
                code: ErrorCode::InternalError,
            },
        }
    }

    /// Handle read chunk request (local read only)
    async fn handle_read_chunk(&self, chunk_id: ChunkId) -> Response {
        debug!("Handling read chunk: {}", chunk_id);

        match self.storage.read_chunk(&chunk_id) {
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
    pub async fn write_data(&self, data: &[u8]) -> Result<Vec<ChunkId>> {
        info!("Writing {} bytes to cluster", data.len());

        // Chunk the data
        let chunks = self.chunker.chunk_data(data);
        let mut chunk_ids = Vec::new();

        for (chunk_id, chunk_data) in chunks {
            // Determine target nodes using consistent hashing
            let target_nodes = self
                .cluster
                .get_nodes_for_chunk(&chunk_id, self.replication_factor)
                .await;

            if target_nodes.is_empty() {
                anyhow::bail!("No nodes available for chunk {}", chunk_id);
            }

            debug!(
                "Replicating chunk {} to {} nodes",
                chunk_id,
                target_nodes.len()
            );

            // Write to nodes (quorum approach - wait for majority)
            let quorum = (target_nodes.len() / 2) + 1;
            let mut success_count = 0;

            for node_id in &target_nodes {
                // If it's the local node, write locally
                if node_id == &self.cluster.local_node_id() {
                    if self
                        .storage
                        .write_chunk(&chunk_id, &chunk_data)
                        .is_ok()
                    {
                        success_count += 1;
                        debug!("Wrote chunk {} locally", chunk_id);
                    }
                } else {
                    // Send to remote node
                    if let Some(node_info) = self.cluster.get_node(node_id).await {
                        let request = Request::ReplicateChunk {
                            chunk_id,
                            data: chunk_data.clone(),
                            checksum: chunk_id.hash,
                        };

                        match self
                            .client
                            .send_message(node_info.addr, Message::Request(request))
                            .await
                        {
                            Ok(response) => match response.message {
                                Message::Response(Response::Ok { .. }) => {
                                    success_count += 1;
                                    debug!("Replicated chunk {} to {}", chunk_id, node_id);
                                }
                                _ => {
                                    warn!("Failed to replicate chunk {} to {}", chunk_id, node_id);
                                }
                            },
                            Err(e) => {
                                warn!("Network error replicating to {}: {}", node_id, e);
                            }
                        }
                    }
                }

                // Check if we've reached quorum
                if success_count >= quorum {
                    break;
                }
            }

            if success_count < quorum {
                anyhow::bail!(
                    "Failed to achieve quorum for chunk {} ({}/{})",
                    chunk_id,
                    success_count,
                    quorum
                );
            }

            // Store chunk location metadata
            let location = ChunkLocation {
                chunk_id,
                nodes: target_nodes,
                size: chunk_data.len(),
                checksum: chunk_id.hash,
            };

            self.metadata
                .put_chunk_location(&location)
                .context("Failed to store chunk location")?;

            chunk_ids.push(chunk_id);
        }

        info!("Successfully wrote {} chunks", chunk_ids.len());
        Ok(chunk_ids)
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
        // Try local first
        if let Ok(data) = self.storage.read_chunk(chunk_id) {
            debug!("Read chunk {} locally", chunk_id);
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
    async fn handle_get_file_metadata_by_path(&self, path: String) -> Response {
        debug!("Handling get file metadata by path: {}", path);

        match self.metadata.get_file_by_path(&path) {
            Ok(Some(metadata)) => Response::FileMetadata { metadata },
            Ok(None) => Response::Error {
                message: "File not found".to_string(),
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

    /// Handle put file metadata request
    async fn handle_put_file_metadata(&self, metadata: FileMetadata) -> Response {
        debug!("Handling put file metadata: {}", metadata.path);

        match self.metadata.put_file(&metadata) {
            Ok(_) => Response::Ok { data: None },
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
            Ok(chunk_ids) => Response::ChunkIds { chunk_ids },
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
                // Delete the file metadata
                match self.metadata.delete_file(&metadata.id) {
                    Ok(_) => Response::Ok { data: None },
                    Err(e) => {
                        warn!("Failed to delete file metadata: {}", e);
                        Response::Error {
                            message: format!("Failed to delete file: {}", e),
                            code: ErrorCode::InternalError,
                        }
                    }
                }
                // Note: Chunk cleanup will be handled by garbage collection later
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
            .filter(|n| self.cluster.is_node_healthy(&n.id))
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
                let total_size: u64 = chunks
                    .iter()
                    .map(|chunk_id| {
                        self.storage
                            .read_chunk(chunk_id)
                            .map(|data| data.len() as u64)
                            .unwrap_or(0)
                    })
                    .sum();

                let nodes_count = self.cluster.get_all_nodes().await.len();

                Response::StorageStats {
                    total_chunks: chunks.len(),
                    total_size,
                    replication_factor: self.replication_factor,
                    nodes_count,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_server_write_read_local() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3);

        // Write data
        let data = b"Hello, distributed filesystem!";
        let chunk_ids = server.write_data(data).await.unwrap();

        assert!(!chunk_ids.is_empty());

        // Read data back
        let read_data = server.read_data(&chunk_ids).await.unwrap();
        assert_eq!(data.as_slice(), read_data.as_slice());
    }

    #[tokio::test]
    async fn test_handle_write_read_chunk() {
        let temp_storage = TempDir::new().unwrap();
        let temp_metadata = TempDir::new().unwrap();

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3);

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

        let storage = Arc::new(ChunkStorage::new(temp_storage.path().to_path_buf()).unwrap());
        let metadata = Arc::new(MetadataStore::new(temp_metadata.path().to_path_buf()).unwrap());

        let node_id = NodeId::new();
        let addr: SocketAddr = "127.0.0.1:8900".parse().unwrap();
        let cluster = Arc::new(ClusterManager::new(node_id, addr, 10, 30));

        let server = Server::new(storage, metadata, 4 * 1024 * 1024, cluster, 3);

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
                _ => Response::Error {
                    message: "Cluster message not implemented".to_string(),
                    code: ErrorCode::InternalError,
                },
            }
        })
    }
}
