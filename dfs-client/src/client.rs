use anyhow::{Context, Result};
use blake3;
use dfs_common::{ChunkId, FileMetadata, Message, MessageEnvelope, NodeId, Request, RequestId, Response};
use lru::LruCache;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Client for communicating with DFS cluster
#[derive(Clone)]
pub struct DfsClient {
    /// List of cluster nodes
    cluster_nodes: Arc<RwLock<Vec<SocketAddr>>>,

    /// Current node index (for round-robin)
    current_node: Arc<RwLock<usize>>,

    /// LRU cache for chunks (ChunkId -> data)
    /// Cache up to 256 chunks (~1GB at 4MB/chunk)
    chunk_cache: Arc<Mutex<LruCache<ChunkId, Arc<Vec<u8>>>>>,

    /// TCP connection pool - maintains one persistent connection per server
    connection_pool: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,

    /// Track chunks currently being prefetched to avoid duplicates
    prefetch_in_flight: Arc<Mutex<HashSet<ChunkId>>>,

    /// Track recent read positions per file to detect sequential patterns
    /// Maps file_id (first chunk) -> VecDeque of last 4 read positions
    read_history: Arc<Mutex<HashMap<ChunkId, VecDeque<usize>>>>,

    /// Round-robin counter for replica selection (for load balancing)
    replica_selector: Arc<AtomicU64>,
}

impl DfsClient {
    /// Create a new DFS client
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        if cluster_nodes.is_empty() {
            anyhow::bail!("No cluster nodes provided");
        }

        // Create LRU cache for 256 chunks (~1GB at 4MB/chunk)
        let cache = LruCache::new(NonZeroUsize::new(256).unwrap());

        Ok(Self {
            cluster_nodes: Arc::new(RwLock::new(cluster_nodes)),
            current_node: Arc::new(RwLock::new(0)),
            chunk_cache: Arc::new(Mutex::new(cache)),
            connection_pool: Arc::new(Mutex::new(HashMap::new())),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            read_history: Arc::new(Mutex::new(HashMap::new())),
            replica_selector: Arc::new(AtomicU64::new(0)),
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

    /// Get file metadata from cluster with optional conditional fetch
    /// Returns Ok(Some(metadata)) if found and modified, Ok(None) if not found, Err if error
    /// If if_modified_since is provided and metadata hasn't changed, returns Ok(None) with NotModified indicator
    pub async fn get_file_metadata_conditional(&self, path: &str, if_modified_since: Option<u64>) -> Result<Option<FileMetadata>> {
        let request = Request::GetFileMetadataByPath {
            path: path.to_string(),
            if_modified_since,
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::FileMetadata { metadata } => Ok(Some(metadata)),
            Response::NotModified => {
                // Metadata hasn't changed, return None to signal cache is valid
                debug!("Metadata not modified for {}", path);
                Ok(None)
            }
            Response::Error { code, .. } if code == dfs_common::ErrorCode::NotFound => Ok(None),
            Response::Error { message, .. } => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Get file metadata from cluster (unconditional)
    pub async fn get_file_metadata(&self, path: &str) -> Result<Option<FileMetadata>> {
        self.get_file_metadata_conditional(path, None).await
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

    /// Read data from cluster by chunk IDs - parallelized with caching
    /// all_file_chunks: Complete list of chunk IDs for the file (for prefetch - can be same as chunk_ids)
    /// start_chunk_idx: Index in all_file_chunks where chunk_ids[0] is located
    pub async fn read_data(&self, chunk_ids: &[ChunkId], all_file_chunks: &[ChunkId], start_chunk_idx: usize) -> Result<Vec<u8>> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let start = std::time::Instant::now();

        // Check cache first and identify missing chunks
        let mut cached_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        let mut chunks_to_fetch: Vec<(usize, ChunkId)> = Vec::new();

        {
            let mut cache = self.chunk_cache.lock().await;
            for (idx, chunk_id) in chunk_ids.iter().enumerate() {
                if let Some(data) = cache.get(chunk_id) {
                    cached_chunks.push((idx, Arc::clone(data)));
                } else {
                    chunks_to_fetch.push((idx, *chunk_id));
                }
            }
        }

        let cache_hits = cached_chunks.len();
        let cache_misses = chunks_to_fetch.len();

        info!("Reading {} chunks: {} cached, {} to fetch",
              chunk_ids.len(), cache_hits, cache_misses);

        // Fetch missing chunks sequentially with connection pooling
        // FUSE typically requests 128KB at a time, which usually maps to 1 chunk,
        // so parallelism within read_data() doesn't help much
        let mut fetched_chunks = Vec::new();
        let nodes = self.cluster_nodes.read().await.clone();

        for (idx, chunk_id) in chunks_to_fetch.iter() {
            let idx = *idx;
            let chunk_id = *chunk_id;

            // Get replica locations for this chunk (with fallback to all nodes)
            let replicas = match self.get_chunk_replicas(chunk_id).await {
                Ok(r) => {
                    debug!("Found {} replicas for chunk {}", r.len(), chunk_id);
                    r
                }
                Err(e) => {
                    // Fallback to trying all nodes if query fails
                    debug!("Failed to get replicas for {}: {}, trying all nodes", chunk_id, e);
                    nodes.clone()
                }
            };

            // Select one replica using round-robin load balancing
            let selected_replica = self.select_replica(&replicas)
                .context("No replicas available")?;

            debug!("Selected replica {} for chunk {} (round-robin)", selected_replica, chunk_id);

            // Try selected replica first, then fallback to others
            let mut last_error = None;
            let mut data = None;

            for (i, node_addr) in std::iter::once(&selected_replica)
                .chain(replicas.iter().filter(|&n| n != &selected_replica))
                .enumerate()
            {
                match self.read_chunk_from_server(*node_addr, chunk_id).await {
                    Ok(chunk_data) => {
                        if i > 0 {
                            debug!("Fetched chunk {} from fallback replica {}", chunk_id, node_addr);
                        }
                        data = Some(chunk_data);
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                        continue;
                    }
                }
            }

            let chunk_data = data.ok_or_else(|| {
                last_error.unwrap_or_else(|| anyhow::anyhow!("All nodes failed for chunk"))
            })?;

            let data_arc = Arc::new(chunk_data);

            // Add to cache
            {
                let mut cache = self.chunk_cache.lock().await;
                cache.put(chunk_id, Arc::clone(&data_arc));
            }

            fetched_chunks.push((idx, data_arc));
        }

        // Combine cached and fetched chunks
        let mut all_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        all_chunks.extend(cached_chunks);
        all_chunks.extend(fetched_chunks);

        // Sort by index to maintain chunk order
        all_chunks.sort_by_key(|(idx, _)| *idx);

        // Find the highest index we accessed (within the local array)
        let last_local_idx = all_chunks.iter().map(|(idx, _)| *idx).max().unwrap_or(0);

        // Concatenate all chunks
        let mut all_data = Vec::new();
        for (_, data) in all_chunks {
            all_data.extend_from_slice(&data);
        }

        let elapsed = start.elapsed();
        let throughput = (all_data.len() as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64();
        info!("Read complete: {} bytes from {} chunks in {:?} ({:.2} MB/s) - cache: {}/{} hits",
              all_data.len(), chunk_ids.len(), elapsed, throughput, cache_hits, chunk_ids.len());

        // Detect sequential access patterns and prefetch aggressively only when sequential
        if !chunk_ids.is_empty() && !all_file_chunks.is_empty() && cache_misses > 0 {
            let last_file_chunk_idx = start_chunk_idx + last_local_idx;
            let file_id = all_file_chunks[0]; // Use first chunk as file identifier

            // Track read history and detect sequential patterns
            let mut history = self.read_history.lock().await;
            let read_positions = history.entry(file_id).or_insert_with(|| VecDeque::with_capacity(4));

            // Add current read position
            read_positions.push_back(last_file_chunk_idx);
            if read_positions.len() > 4 {
                read_positions.pop_front();
            }

            // Detect if we have sequential momentum (2+ consecutive sequential reads)
            let is_sequential = if read_positions.len() >= 2 {
                let mut sequential_count = 0;
                for i in 1..read_positions.len() {
                    let prev = read_positions[i - 1];
                    let curr = read_positions[i];
                    // Consider sequential if within 2 chunks forward
                    if curr > prev && curr <= prev + 2 {
                        sequential_count += 1;
                    }
                }
                sequential_count >= 1 // Need at least 1 sequential step
            } else {
                false // Not enough history yet
            };

            drop(history); // Release lock before spawning tasks

            if is_sequential {
                info!("Prefetch: detected sequential pattern at chunk {}, prefetching next 8 chunks",
                      last_file_chunk_idx);

                // Prefetch next 8 chunks for aggressive sequential read-ahead
                for prefetch_offset in 1..=8 {
                    let prefetch_file_idx = last_file_chunk_idx + prefetch_offset;

                    // Check if this chunk exists in the file
                    if prefetch_file_idx >= all_file_chunks.len() {
                        break; // Beyond end of file
                    }

                    let prefetch_chunk_id = all_file_chunks[prefetch_file_idx];

                // Check if already cached or being prefetched
                let should_prefetch = {
                    let cache = self.chunk_cache.lock().await;
                    let mut in_flight = self.prefetch_in_flight.lock().await;

                    if cache.peek(&prefetch_chunk_id).is_some() || in_flight.contains(&prefetch_chunk_id) {
                        false // Already have it or fetching it
                    } else {
                        in_flight.insert(prefetch_chunk_id);
                        true
                    }
                };

                if should_prefetch {
                    // Spawn background prefetch task
                    let client = self.clone();
                    let nodes = nodes.clone();

                    tokio::spawn(async move {
                        info!("Prefetching chunk {} (read-ahead)", prefetch_chunk_id);

                        // Get replica locations for load balancing
                        let replicas = match client.get_chunk_replicas(prefetch_chunk_id).await {
                            Ok(r) => r,
                            Err(_) => nodes.clone(), // Fallback to all nodes
                        };

                        // Select replica using round-robin
                        let selected_replica = client.select_replica(&replicas);

                        // Try selected replica first, then fallback to others
                        let try_nodes: Vec<SocketAddr> = if let Some(selected) = selected_replica {
                            std::iter::once(selected)
                                .chain(replicas.iter().filter(|&n| *n != selected).copied())
                                .collect()
                        } else {
                            replicas
                        };

                        for node_addr in &try_nodes {
                            match client.read_chunk_from_server(*node_addr, prefetch_chunk_id).await {
                                Ok(data) => {
                                    // Add to cache
                                    let data_arc = Arc::new(data);
                                    {
                                        let mut cache = client.chunk_cache.lock().await;
                                        cache.put(prefetch_chunk_id, data_arc);
                                    }
                                    info!("Prefetch complete: {}", prefetch_chunk_id);
                                    break;
                                }
                                Err(e) => {
                                    debug!("Prefetch failed from {}: {}", node_addr, e);
                                    continue;
                                }
                            }
                        }

                        // Remove from in-flight tracker
                        {
                            let mut in_flight = client.prefetch_in_flight.lock().await;
                            in_flight.remove(&prefetch_chunk_id);
                        }
                    });
                }
            }
            } else {
                info!("Skipping prefetch: random/non-sequential access detected at chunk {}",
                       last_file_chunk_idx);
            }
        }

        Ok(all_data)
    }

    /// Query cluster for chunk replica locations (returns node addresses that have this chunk)
    async fn get_chunk_replicas(&self, chunk_id: ChunkId) -> Result<Vec<SocketAddr>> {
        let request = Request::GetChunkReplicas { chunk_id };
        let nodes = self.cluster_nodes.read().await;

        // Query any node for replica locations
        let query_node = nodes.first().context("No cluster nodes available")?;

        let response = self.send_request(*query_node, request).await?;

        match response {
            Response::ChunkReplicas { nodes: replica_node_ids, .. } => {
                // Convert NodeIds to SocketAddrs using cluster node list
                // For now, use all nodes as potential replicas if we can't map NodeId
                // In production, you'd maintain a NodeId->SocketAddr mapping
                if !replica_node_ids.is_empty() {
                    Ok(nodes.clone())
                } else {
                    anyhow::bail!("No replicas found for chunk")
                }
            }
            Response::Error { message, .. } => {
                anyhow::bail!("Failed to get chunk replicas: {}", message)
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Select one replica from a list using round-robin for load balancing
    fn select_replica(&self, replicas: &[SocketAddr]) -> Option<SocketAddr> {
        if replicas.is_empty() {
            return None;
        }

        let idx = self.replica_selector.fetch_add(1, Ordering::Relaxed) as usize % replicas.len();
        Some(replicas[idx])
    }

    /// Read a single chunk from a specific server using connection pooling
    async fn read_chunk_from_server(&self, server_addr: SocketAddr, chunk_id: ChunkId) -> Result<Vec<u8>> {
        let request = Request::ReadChunk { chunk_id };

        // Try using pooled connection first, with fallback to new connection
        let mut attempt = 0;
        loop {
            attempt += 1;

            // Get or create connection
            let stream = {
                let mut pool = self.connection_pool.lock().await;
                pool.remove(&server_addr)
            };

            let mut stream = match stream {
                Some(s) => {
                    debug!("Reusing pooled connection to {}", server_addr);
                    s
                }
                None => {
                    debug!("Creating new connection to {}", server_addr);
                    TcpStream::connect(server_addr)
                        .await
                        .context("Failed to connect to server")?
                }
            };

            let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
            let envelope = MessageEnvelope::new(request_id, Message::Request(request.clone()));
            let encoded = envelope.to_bytes().context("Failed to serialize message")?;

            // Send request and read response
            let result = async {
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

                Ok::<(TcpStream, Vec<u8>), std::io::Error>((stream, buf))
            }.await;

            match result {
                Ok((stream, buf)) => {
                    // Return connection to pool
                    {
                        let mut pool = self.connection_pool.lock().await;
                        pool.insert(server_addr, stream);
                    }

                    // Deserialize response
                    let response_envelope = MessageEnvelope::from_bytes(&buf)
                        .context("Failed to deserialize response")?;

                    match response_envelope.message {
                        Message::Response(Response::ChunkData { data, .. }) => return Ok(data),
                        Message::Response(Response::Error { message, .. }) => {
                            anyhow::bail!("Server error: {}", message);
                        }
                        _ => anyhow::bail!("Unexpected response type"),
                    }
                }
                Err(e) => {
                    // Connection failed - don't return to pool
                    warn!("Connection to {} failed (attempt {}): {}", server_addr, attempt, e);

                    // Retry once with new connection if this was a pooled connection
                    if attempt == 1 {
                        debug!("Retrying with new connection to {}", server_addr);
                        continue;
                    } else {
                        return Err(e).context("Failed to read chunk after retry");
                    }
                }
            }
        }
    }

    /// Write data to cluster with dual-stream parallelization
    /// For RF=3: Split data in half, send to 2 servers simultaneously
    /// Each server replicates to 2 nodes, 3rd replica created in background
    pub async fn write_data(&self, data: &[u8]) -> Result<(Vec<ChunkId>, Vec<u64>)> {
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

        let (chunk_ids_1, chunk_sizes_1) = result1?;
        let (chunk_ids_2, chunk_sizes_2) = result2?;

        let mut all_chunk_ids = Vec::new();
        all_chunk_ids.extend(chunk_ids_1);
        all_chunk_ids.extend(chunk_ids_2);

        let mut all_chunk_sizes = Vec::new();
        all_chunk_sizes.extend(chunk_sizes_1);
        all_chunk_sizes.extend(chunk_sizes_2);

        info!("Completed dual-stream write: {} total chunks", all_chunk_ids.len());

        Ok((all_chunk_ids, all_chunk_sizes))
    }

    /// Write a chunk to a specific server
    async fn write_chunk_to_server(server_addr: SocketAddr, data: Vec<u8>) -> Result<(Vec<ChunkId>, Vec<u64>)> {
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
            Message::Response(Response::ChunkIds { chunk_ids, chunk_sizes }) => Ok((chunk_ids, chunk_sizes)),
            Message::Response(Response::Error { message, .. }) => {
                anyhow::bail!("Server error: {}", message);
            }
            _ => anyhow::bail!("Unexpected response type"),
        }
    }

    /// Write small data via single server (old path)
    pub async fn write_data_single_chunk(&self, data: &[u8]) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let request = Request::WriteFile {
            data: data.to_vec(),
        };

        let response = self.send_request_with_retry(request).await?;

        match response {
            Response::ChunkIds { chunk_ids, chunk_sizes } => Ok((chunk_ids, chunk_sizes)),
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

    /// Refresh cluster node list by querying GetClusterStatus
    pub async fn refresh_cluster_nodes(&self) -> Result<()> {
        let nodes = self.cluster_nodes.read().await.clone();

        // Try to get cluster status from any node
        for node_addr in &nodes {
            let request = Request::GetClusterStatus;

            match self.send_request(*node_addr, request).await {
                Ok(Response::ClusterStatus { nodes: cluster_nodes, .. }) => {
                    // Extract online node addresses
                    let new_addrs: Vec<SocketAddr> = cluster_nodes
                        .iter()
                        .filter(|n| n.status == dfs_common::NodeStatus::Online)
                        .map(|n| n.addr)
                        .collect();

                    if !new_addrs.is_empty() {
                        let mut cluster_nodes = self.cluster_nodes.write().await;
                        *cluster_nodes = new_addrs;
                        info!("Refreshed cluster nodes: {} nodes", cluster_nodes.len());
                        return Ok(());
                    }
                }
                _ => continue,
            }
        }

        Err(anyhow::anyhow!("Failed to refresh cluster nodes from any server"))
    }

    /// Get storage statistics from all nodes and aggregate them
    /// Returns (total_space, free_space, available_space, replication_factor)
    pub async fn get_storage_stats(&self) -> Result<(u64, u64, u64, usize)> {
        let nodes = self.cluster_nodes.read().await.clone();
        let request = Request::GetStorageStats;

        // Query all nodes IN PARALLEL for speed
        let mut tasks: Vec<tokio::task::JoinHandle<Result<Option<(u64, u64, u64, usize)>, Box<dyn std::error::Error + Send + Sync>>>> = Vec::new();

        for node_addr in nodes {
            let request = request.clone();
            let task = tokio::spawn(async move {
                // Wrap entire query with 2s timeout to avoid hanging on offline nodes
                // (ARM servers are slower, need more generous timeout)
                let query_future = async {
                    // Create a temporary client for this request with 1s connect timeout
                    let mut stream = tokio::time::timeout(
                        std::time::Duration::from_millis(1000),
                        tokio::net::TcpStream::connect(node_addr)
                    ).await
                        .map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout")) as Box<dyn std::error::Error + Send + Sync>)?
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                let request_id = dfs_common::RequestId::new(
                    std::sync::atomic::AtomicU64::new(1).fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                );
                let envelope = dfs_common::MessageEnvelope::new(
                    request_id,
                    dfs_common::Message::Request(request)
                );
                let encoded = envelope.to_bytes().map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                use tokio::io::{AsyncWriteExt, AsyncReadExt};
                stream.write_u32(encoded.len() as u32).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                stream.write_all(&encoded).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                stream.flush().await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                let mut len_buf = [0u8; 4];
                stream.read_exact(&mut len_buf).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
                let len = u32::from_be_bytes(len_buf) as usize;

                let mut buf = vec![0u8; len];
                stream.read_exact(&mut buf).await.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                    let response_envelope = dfs_common::MessageEnvelope::from_bytes(&buf).map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

                    match response_envelope.message {
                        dfs_common::Message::Response(dfs_common::Response::StorageStats {
                            total_space,
                            free_space,
                            available_space,
                            replication_factor,
                            ..
                        }) => Ok(Some((total_space, free_space, available_space, replication_factor))),
                        _ => Ok(None),
                    }
                };

                // Apply overall 2s timeout to entire query
                tokio::time::timeout(std::time::Duration::from_millis(2000), query_future)
                    .await
                    .map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "query timeout")) as Box<dyn std::error::Error + Send + Sync>)?
            });

            tasks.push(task);
        }

        // Wait for all queries to complete
        let mut total_raw_space = 0u64;
        let mut node_capacities: Vec<(u64, u64)> = Vec::new(); // (total, available) per node
        let mut replication_factor = 3; // default

        for task in tasks {
            if let Ok(Ok(Some((total, free, avail, rf)))) = task.await {
                total_raw_space += total;
                node_capacities.push((total, avail));
                replication_factor = rf;
            }
        }

        // If we didn't get any valid stats, return error
        if node_capacities.is_empty() {
            anyhow::bail!("Failed to get storage stats from any node");
        }

        // Calculate usable capacity using greedy algorithm:
        // Iteratively select the best RF nodes and add their bottleneck to total capacity
        //
        // This correctly handles heterogeneous clusters where smart replica set selection
        // can dramatically increase usable capacity.
        //
        // Example: RF=3, nodes (100G, 100G, 100G, 10G)
        //   Iteration 1: Pick top 3 (100,100,100), min=100G, total=100G
        //   Iteration 2: Pick top 3 (0,0,0), min=0G, done
        //   → Total = 100G (NOT 13G from naive formula!)
        let usable_total = total_raw_space / replication_factor as u64;
        let usable_available = calculate_usable_capacity(
            &node_capacities.iter().map(|(_, avail)| *avail).collect::<Vec<_>>(),
            replication_factor
        );

        info!("Storage stats: {} nodes, usable_total={}, usable_avail={} (RF={})",
              node_capacities.len(), usable_total, usable_available, replication_factor);

        // Calculate usable_free as the complement of used space
        // (usable_total - usable_available gives used space on a per-replica basis)
        let usable_free = usable_available;

        Ok((usable_total, usable_free, usable_available, replication_factor))
    }
}

/// Calculate usable capacity using greedy algorithm for smart replica set selection
///
/// This algorithm correctly handles heterogeneous clusters by iteratively selecting
/// the best replica sets (top RF nodes by capacity) and accounting for their bottleneck.
///
/// Example: RF=3, nodes (100G, 100G, 100G, 10G)
///   - Iteration 1: Pick top 3 (100,100,100), min=100G, add 100G to total
///   - Iteration 2: Pick top 3 (0,0,0), min=0G, done
///   - Result: 100G (NOT 13G from naive min×nodes/RF formula)
///
/// This matches the bash algorithm provided by the user and works for any RF value.
fn calculate_usable_capacity(node_capacities: &[u64], replication_factor: usize) -> u64 {
    if node_capacities.is_empty() || replication_factor == 0 {
        return 0;
    }

    let mut capacities = node_capacities.to_vec();
    let mut total = 0u64;

    loop {
        // Filter out zeros and sort descending
        let mut non_zero: Vec<u64> = capacities.iter()
            .copied()
            .filter(|&c| c > 0)
            .collect();

        // Check if we have at least RF nodes with capacity > 0
        if non_zero.len() < replication_factor {
            break;
        }

        // Sort descending
        non_zero.sort_by(|a, b| b.cmp(a));

        // The decrement is the minimum of the top RF nodes (the RF-th largest value)
        let decrement = non_zero[replication_factor - 1];
        total += decrement;

        // Subtract decrement ONLY from the top RF nodes
        let mut decremented_count = 0;
        for val in &non_zero[0..replication_factor] {
            // Find this value in the original capacities array and decrement it
            for capacity in &mut capacities {
                if *capacity == *val && decremented_count < replication_factor {
                    *capacity = capacity.saturating_sub(decrement);
                    decremented_count += 1;
                    break; // Move to next value in the top RF
                }
            }
        }
    }

    total
}
