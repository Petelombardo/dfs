use anyhow::{Context, Result};
use blake3;
use dfs_common::{ChunkId, FileMetadata, Message, MessageEnvelope, Request, RequestId, Response};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Client for communicating with DFS cluster
pub struct DfsClient {
    /// List of cluster nodes
    cluster_nodes: Arc<RwLock<Vec<SocketAddr>>>,

    /// Current node index (for round-robin)
    current_node: Arc<RwLock<usize>>,
}

impl DfsClient {
    /// Create a new DFS client
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        if cluster_nodes.is_empty() {
            anyhow::bail!("No cluster nodes provided");
        }

        Ok(Self {
            cluster_nodes: Arc::new(RwLock::new(cluster_nodes)),
            current_node: Arc::new(RwLock::new(0)),
        })
    }

    /// Get next node address (round-robin)
    async fn get_next_node(&self) -> SocketAddr {
        let nodes = self.cluster_nodes.read().await;
        let mut current = self.current_node.write().await;

        let addr = nodes[*current];
        *current = (*current + 1) % nodes.len();

        addr
    }

    /// Send a request to a cluster node with retry
    async fn send_request_with_retry(&self, request: Request) -> Result<Response> {
        let nodes = self.cluster_nodes.read().await.clone();
        let mut last_error = None;

        // Try all nodes
        for node_addr in &nodes {
            match self.send_request(*node_addr, request.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    warn!("Failed to send request to {}: {}", node_addr, e);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))
    }

    /// Send a request to a specific node
    async fn send_request(&self, addr: SocketAddr, request: Request) -> Result<Response> {
        debug!("Sending request to {}: {:?}", addr, request);

        // Connect to node
        let mut stream = TcpStream::connect(addr)
            .await
            .context("Failed to connect to node")?;

        // Create envelope with request ID
        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;

        // Send message with length prefix
        let len = encoded.len() as u32;
        let send_result = async {
            stream.write_all(&len.to_be_bytes()).await?;
            stream.write_all(&encoded).await?;
            stream.flush().await?;
            Ok::<(), std::io::Error>(())
        }.await;

        if let Err(e) = send_result {
            // Connection failed, don't return to pool
            return Err(e).context("Failed to send request");
        }

        // Read response
        let mut len_buf = [0u8; 4];
        let read_result = async {
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;

            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        }.await;

        match read_result {
            Ok(buf) => {
                // Deserialize response envelope
                let response_envelope = MessageEnvelope::from_bytes(&buf)
                    .context("Failed to deserialize response")?;

                match response_envelope.message {
                    Message::Response(response) => Ok(response),
                    _ => anyhow::bail!("Expected Response message"),
                }
            }
            Err(e) => {
                // Connection failed, don't return to pool
                Err(e).context("Failed to read response")
            }
        }
    }

    /// Get file metadata from cluster
    pub async fn get_file_metadata(&self, path: &str) -> Result<Option<FileMetadata>> {
        let request = Request::GetFileMetadataByPath {
            path: path.to_string(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::FileMetadata { metadata } => Ok(Some(metadata)),
            Response::Error { code, .. } if code == dfs_common::ErrorCode::NotFound => Ok(None),
            Response::Error { message, .. } => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// List directory contents
    pub async fn list_directory(&self, path: &str) -> Result<Vec<FileMetadata>> {
        let request = Request::ListDirectory {
            path: path.to_string(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::DirectoryListing { entries } => Ok(entries),
            Response::Error { message, .. } => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Read data from cluster by chunk IDs - parallelized for performance
    pub async fn read_data(&self, chunk_ids: &[ChunkId]) -> Result<Vec<u8>> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let start = std::time::Instant::now();
        info!("Reading {} chunks in parallel", chunk_ids.len());

        // Read all chunks in parallel
        let mut tasks = Vec::new();

        for (idx, chunk_id) in chunk_ids.iter().enumerate() {
            let chunk_id = *chunk_id;
            let request = Request::ReadChunk { chunk_id };

            // Clone self for the async task
            let nodes = self.cluster_nodes.read().await.clone();

            let task = tokio::spawn(async move {
                // Try all nodes for this chunk
                let mut last_error = None;

                for node_addr in &nodes {
                    match Self::read_chunk_from_server(*node_addr, chunk_id).await {
                        Ok(data) => return Ok::<(usize, Vec<u8>), anyhow::Error>((idx, data)),
                        Err(e) => {
                            last_error = Some(e);
                            continue;
                        }
                    }
                }

                Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed")))
            });

            tasks.push(task);
        }

        // Wait for all chunks and reassemble in order
        let mut chunks_with_index = Vec::new();
        for task in tasks {
            let (idx, data) = task.await??;
            chunks_with_index.push((idx, data));
        }

        // Sort by index to maintain chunk order
        chunks_with_index.sort_by_key(|(idx, _)| *idx);

        // Concatenate all chunks
        let mut all_data = Vec::new();
        for (_, data) in chunks_with_index {
            all_data.extend_from_slice(&data);
        }

        let elapsed = start.elapsed();
        let throughput = (all_data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
        info!("Read complete: {} bytes from {} chunks in {:?} ({:.2} MB/s)",
              all_data.len(), chunk_ids.len(), elapsed, throughput);

        Ok(all_data)
    }

    /// Read a single chunk from a specific server
    async fn read_chunk_from_server(server_addr: SocketAddr, chunk_id: ChunkId) -> Result<Vec<u8>> {
        let request = Request::ReadChunk { chunk_id };

        let mut stream = TcpStream::connect(server_addr)
            .await
            .context("Failed to connect to server")?;

        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;

        // Send request
        let len = encoded.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        // Read response
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;

        match response_envelope.message {
            Message::Response(Response::ChunkData { data, .. }) => Ok(data),
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Write data to cluster with dual-stream parallelization
    /// For RF=3: Split data in half, send to 2 servers simultaneously
    /// Each server replicates to 2 nodes, 3rd replica created in background
    pub async fn write_data(&self, data: &[u8]) -> Result<Vec<ChunkId>> {
        const MIN_PARALLEL_SIZE: usize = 512 * 1024; // 512KB minimum

        // For small writes, use single server (less overhead)
        if data.len() < MIN_PARALLEL_SIZE {
            return self.write_data_single_chunk(data).await;
        }

        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            return self.write_data_single_chunk(data).await;
        }

        info!("Writing {} bytes using dual-stream parallelization", data.len());

        // Split data in half for 2 parallel streams
        let mid = data.len() / 2;
        let (chunk1, chunk2) = data.split_at(mid);

        // Send to 2 different servers in parallel
        let server1 = nodes[0];
        let server2 = nodes[1 % nodes.len()];

        let chunk1 = chunk1.to_vec();
        let chunk2 = chunk2.to_vec();

        // Spawn both writes in parallel
        let task1 = Self::write_chunk_to_server(server1, chunk1);
        let task2 = Self::write_chunk_to_server(server2, chunk2);

        let (result1, result2) = tokio::join!(task1, task2);

        let mut all_chunk_ids = Vec::new();
        all_chunk_ids.extend(result1?);
        all_chunk_ids.extend(result2?);

        info!("Completed dual-stream write: {} total chunks", all_chunk_ids.len());

        Ok(all_chunk_ids)
    }

    /// Write a chunk to a specific server
    async fn write_chunk_to_server(server_addr: SocketAddr, data: Vec<u8>) -> Result<Vec<ChunkId>> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFile { data };

        // Create connection
        let connect_start = std::time::Instant::now();
        let mut stream = TcpStream::connect(server_addr)
            .await
            .context("Failed to connect to server")?;
        let connect_time = connect_start.elapsed();

        let serialize_start = std::time::Instant::now();
        let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
        let envelope = MessageEnvelope::new(request_id, Message::Request(request));
        let encoded = envelope.to_bytes().context("Failed to serialize message")?;
        let serialize_time = serialize_start.elapsed();

        // Send request
        let send_start = std::time::Instant::now();
        let len = encoded.len() as u32;
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;
        let send_time = send_start.elapsed();

        // Read response
        let recv_start = std::time::Instant::now();
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        let recv_time = recv_start.elapsed();

        let deserialize_start = std::time::Instant::now();
        let response_envelope = MessageEnvelope::from_bytes(&buf)
            .context("Failed to deserialize response")?;
        let deserialize_time = deserialize_start.elapsed();

        let total_time = total_start.elapsed();
        let throughput = (data_len as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Client write to {}: {} bytes in {:?} ({:.2} MB/s) - connect: {:?}, serialize: {:?}, send: {:?}, recv: {:?}, deserialize: {:?}",
              server_addr, data_len, total_time, throughput, connect_time, serialize_time, send_time, recv_time, deserialize_time);

        match response_envelope.message {
            Message::Response(Response::ChunkIds { chunk_ids }) => Ok(chunk_ids),
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Write small data via single server (old path)
    async fn write_data_single_chunk(&self, data: &[u8]) -> Result<Vec<ChunkId>> {
        let request = Request::WriteFile {
            data: data.to_vec(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::ChunkIds { chunk_ids } => Ok(chunk_ids),
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to write data: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Create or update file metadata
    pub async fn put_file_metadata(&self, metadata: &FileMetadata) -> Result<()> {
        let request = Request::PutFileMetadata {
            metadata: metadata.clone(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to put metadata: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Delete file
    pub async fn delete_file(&self, path: &str) -> Result<()> {
        let request = Request::DeleteFile {
            path: path.to_string(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::Ok { .. } => Ok(()),
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to delete file: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Get storage statistics from all nodes and aggregate them
    /// Returns (total_space, free_space, available_space, replication_factor)
    pub async fn get_storage_stats(&self) -> Result<(u64, u64, u64, usize)> {
        let nodes = self.cluster_nodes.read().await.clone();
        let request = Request::GetStorageStats;

        // Query all nodes
        let mut min_total = u64::MAX;
        let mut min_free = u64::MAX;
        let mut min_available = u64::MAX;
        let mut replication_factor = 3; // default

        for node_addr in &nodes {
            match self.send_request(*node_addr, request.clone()).await {
                Ok(Response::StorageStats {
                    total_space,
                    free_space,
                    available_space,
                    replication_factor: rf,
                    ..
                }) => {
                    // Track minimum values (bottleneck node determines capacity)
                    if total_space > 0 && total_space < min_total {
                        min_total = total_space;
                    }
                    if free_space > 0 && free_space < min_free {
                        min_free = free_space;
                    }
                    if available_space > 0 && available_space < min_available {
                        min_available = available_space;
                    }
                    replication_factor = rf;
                }
                Ok(Response::Error { message, .. }) => {
                    warn!("Failed to get stats from {}: {}", node_addr, message);
                }
                Err(e) => {
                    warn!("Failed to query {}: {}", node_addr, e);
                }
                _ => {}
            }
        }

        // If we didn't get any valid stats, return error
        if min_total == u64::MAX {
            anyhow::bail!("Failed to get storage stats from any node");
        }

        // For replica 3 setup: the usable space is the smallest node's capacity
        // because all 3 copies must fit, and we're limited by the smallest node
        // The actual capacity calculation:
        // - With N nodes and replication factor R, we have N/R sets of replicas
        // - Each set can store (smallest_node_size) worth of unique data
        // - But since all our nodes are the same, we just report the smallest node's space
        //
        // For your setup: 3 nodes with replica 3 means 1 set of replicas
        // So usable space = smallest node space (not divided by RF)

        Ok((min_total, min_free, min_available, replication_factor))
    }
}
