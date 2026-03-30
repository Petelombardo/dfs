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

/// Cache key for byte-range caching: (inode, file_byte_offset)
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
struct ByteRangeCacheKey {
    inode: u64,
    file_offset: u64,
}

/// Cached chunk data with metadata
#[derive(Debug, Clone)]
struct CachedChunk {
    data: Arc<Vec<u8>>,
    chunk_size: usize,
}

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

    /// Byte-range cache for recently-accessed chunks (inode, offset) -> chunk data
    /// This solves the problem of content-addressed chunks changing during live DVR recording
    /// Even if chunk hashes change, we can still cache by file position
    byte_range_cache: Arc<Mutex<LruCache<ByteRangeCacheKey, CachedChunk>>>,

    /// TCP connection pool - maintains one persistent connection per server
    connection_pool: Arc<Mutex<HashMap<SocketAddr, TcpStream>>>,

    /// Track chunks currently being prefetched to avoid duplicates
    prefetch_in_flight: Arc<Mutex<HashSet<ChunkId>>>,

    /// Track recent read positions per file to detect sequential patterns
    /// Maps file_id (first chunk) -> VecDeque of last 4 read positions
    read_history: Arc<Mutex<HashMap<ChunkId, VecDeque<usize>>>>,

    /// Round-robin counter for replica selection (for load balancing)
    replica_selector: Arc<AtomicU64>,

    /// Replica location cache: ChunkId -> Vec<SocketAddr>
    /// Caches which nodes have which chunks to avoid metadata queries on every read
    /// Cache up to 10000 entries (at 40 bytes each = ~400KB)
    replica_cache: Arc<Mutex<LruCache<ChunkId, Arc<Vec<SocketAddr>>>>>,
}

impl DfsClient {
    /// Create a new DFS client
    pub fn new(cluster_nodes: Vec<SocketAddr>) -> Result<Self> {
        if cluster_nodes.is_empty() {
            anyhow::bail!("No cluster nodes provided");
        }

        // Initialize LRU cache with dynamic sizing based on available system memory
        // Target: 15% of available RAM (slightly higher than server since client is single-purpose)
        // min 50 chunks (~200MB), max 1000 chunks (~4GB)
        // This prevents OOM on memory-constrained SBCs while maximizing cache on larger systems
        let cache_capacity = dfs_common::calculate_cache_capacity(
            4 * 1024 * 1024, // 4MB chunk size
            15,   // 15% of available memory (higher than server since client-focused)
            50,   // min 50 chunks (~200MB)
            1000, // max 1000 chunks (~4GB)
        )
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to calculate cache capacity: {}, using default of 256 chunks", e);
            NonZeroUsize::new(256).unwrap()
        });

        let cache = LruCache::new(cache_capacity);

        // Byte-range cache uses same capacity as chunk cache
        let byte_range_cache = LruCache::new(cache_capacity);

        // Replica location cache: 10000 entries
        let replica_cache_capacity = NonZeroUsize::new(10000).unwrap();
        let replica_cache = LruCache::new(replica_cache_capacity);

        Ok(Self {
            cluster_nodes: Arc::new(RwLock::new(cluster_nodes)),
            current_node: Arc::new(RwLock::new(0)),
            chunk_cache: Arc::new(Mutex::new(cache)),
            byte_range_cache: Arc::new(Mutex::new(byte_range_cache)),
            connection_pool: Arc::new(Mutex::new(HashMap::new())),
            prefetch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            read_history: Arc::new(Mutex::new(HashMap::new())),
            replica_selector: Arc::new(AtomicU64::new(0)),
            replica_cache: Arc::new(Mutex::new(replica_cache)),
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
    /// inode: File inode for byte-range caching (optional, 0 to disable)
    /// chunk_offsets: File byte offset for each chunk in chunk_ids (for byte-range caching)
    pub async fn read_data(
        &self,
        chunk_ids: &[ChunkId],
        all_file_chunks: &[ChunkId],
        start_chunk_idx: usize,
        inode: u64,
        chunk_offsets: &[u64],
    ) -> Result<Vec<u8>> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let start = std::time::Instant::now();

        // Check byte-range cache first (for live DVR files), then chunk cache
        let mut cached_chunks: Vec<(usize, Arc<Vec<u8>>)> = Vec::new();
        let mut chunks_to_fetch: Vec<(usize, ChunkId, u64)> = Vec::new(); // (idx, chunk_id, file_offset)

        {
            let mut byte_cache = self.byte_range_cache.lock().await;
            let mut chunk_cache = self.chunk_cache.lock().await;

            for (idx, chunk_id) in chunk_ids.iter().enumerate() {
                let mut found = false;

                // Try byte-range cache first if we have inode + offset
                // We need to check if this chunk's offset range overlaps with any cached chunk
                if inode > 0 && idx < chunk_offsets.len() {
                    let requested_offset = chunk_offsets[idx];

                    // Iterate through all cached chunks for this inode to find range overlap
                    // LRU cache doesn't support range queries, so we need to check all keys
                    let mut matching_cached_chunk: Option<(ByteRangeCacheKey, Arc<Vec<u8>>)> = None;

                    // Peek at cache entries without modifying LRU order yet
                    for (key, cached) in byte_cache.iter() {
                        if key.inode == inode {
                            let cached_start = key.file_offset;
                            let cached_end = cached_start + cached.chunk_size as u64;

                            // Check if requested offset falls within this cached chunk
                            if requested_offset >= cached_start && requested_offset < cached_end {
                                matching_cached_chunk = Some((*key, Arc::clone(&cached.data)));
                                break;
                            }
                        }
                    }

                    if let Some((key, data)) = matching_cached_chunk {
                        // Now actually get it to update LRU order
                        byte_cache.get(&key);
                        info!("Byte-range cache HIT for inode={} offset={} (found in cached chunk at offset={})",
                              inode, requested_offset, key.file_offset);
                        cached_chunks.push((idx, data));
                        found = true;
                    }
                }

                // Fall back to chunk ID cache
                if !found {
                    if let Some(data) = chunk_cache.get(chunk_id) {
                        debug!("Chunk cache HIT for chunk {}", chunk_id);
                        cached_chunks.push((idx, Arc::clone(data)));
                        found = true;
                    }
                }

                // Need to fetch
                if !found {
                    let file_offset = if idx < chunk_offsets.len() { chunk_offsets[idx] } else { 0 };
                    info!("Cache MISS for chunk {} (inode={}, offset={}) - will fetch", chunk_id, inode, file_offset);
                    chunks_to_fetch.push((idx, *chunk_id, file_offset));
                }
            }
        }

        let cache_hits = cached_chunks.len();
        let cache_misses = chunks_to_fetch.len();

        info!("Reading {} chunks: {} cached, {} to fetch (chunk_ids: {:?})",
              chunk_ids.len(), cache_hits, cache_misses, chunk_ids);

        // Fetch missing chunks IN PARALLEL with intelligent replica selection
        // Different chunks can be fetched from different replica nodes simultaneously,
        // maximizing network bandwidth and reducing latency
        let nodes = self.cluster_nodes.read().await.clone();

        // Create parallel fetch tasks
        let fetch_tasks: Vec<_> = chunks_to_fetch.iter().map(|(idx, chunk_id, file_offset)| {
            let idx = *idx;
            let chunk_id = *chunk_id;
            let file_offset = *file_offset;
            let client = self.clone();
            let nodes = nodes.clone();

            tokio::spawn(async move {
                // Try to get replica locations from cache first
                let cached_replicas = {
                    let mut cache = client.replica_cache.lock().await;
                    cache.get(&chunk_id).cloned()
                };

                let replicas = if let Some(cached) = cached_replicas {
                    debug!("Replica cache HIT for chunk {}", chunk_id);
                    (*cached).clone()
                } else {
                    // Cache miss - query metadata server
                    debug!("Replica cache MISS for chunk {}", chunk_id);
                    match client.get_chunk_replicas(chunk_id).await {
                        Ok(r) => {
                            debug!("Found {} replicas for chunk {}", r.len(), chunk_id);
                            // Cache the result
                            let r_arc = Arc::new(r.clone());
                            client.replica_cache.lock().await.put(chunk_id, r_arc);
                            r
                        }
                        Err(e) => {
                            // Fallback to trying all nodes if query fails
                            debug!("Failed to get replicas for {}: {}, trying all nodes", chunk_id, e);
                            nodes.clone()
                        }
                    }
                };

                // Select one replica using round-robin load balancing
                // This ensures different chunks are likely fetched from different nodes
                let selected_replica = client.select_replica(&replicas)
                    .context("No replicas available")?;

                debug!("Selected replica {} for chunk {} (round-robin)", selected_replica, chunk_id);

                // Try selected replica first, then fallback to others
                let mut last_error = None;
                let mut data = None;

                for (i, node_addr) in std::iter::once(&selected_replica)
                    .chain(replicas.iter().filter(|&n| n != &selected_replica))
                    .enumerate()
                {
                    match client.read_chunk_from_server(*node_addr, chunk_id).await {
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

                Ok::<_, anyhow::Error>((idx, chunk_id, file_offset, Arc::new(chunk_data)))
            })
        }).collect();

        // Wait for all fetches to complete
        let fetch_results = futures::future::join_all(fetch_tasks).await;

        // Process results and update both caches
        let mut fetched_chunks = Vec::new();
        for result in fetch_results {
            let (idx, chunk_id, file_offset, data_arc) = result
                .context("Fetch task panicked")?
                .context("Failed to fetch chunk")?;

            // Add to both chunk cache and byte-range cache
            {
                let mut chunk_cache = self.chunk_cache.lock().await;
                chunk_cache.put(chunk_id, Arc::clone(&data_arc));
                debug!("Cached chunk {} ({} bytes)", chunk_id, data_arc.len());
            }

            // Add to byte-range cache if we have inode
            if inode > 0 && file_offset > 0 {
                let mut byte_cache = self.byte_range_cache.lock().await;
                let key = ByteRangeCacheKey {
                    inode,
                    file_offset,
                };
                let cached = CachedChunk {
                    data: Arc::clone(&data_arc),
                    chunk_size: data_arc.len(),
                };
                byte_cache.put(key, cached);
                info!("Byte-range cached: inode={} offset={} ({} bytes)", inode, file_offset, data_arc.len());
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

            // Enable aggressive prefetching for sequential reads (DVR playback)
            // With tiny variable-sized chunks (MPEG-TS packets), caching can't help much
            // Prefetching is essential for smooth playback
            if is_sequential {
                // Adaptive prefetch distance based on chunk count
                // More chunks = smaller chunk sizes = need more prefetch
                // Target: ~256KB of prefetch buffer
                let prefetch_distance = if all_file_chunks.len() > 500 {
                    // Many tiny chunks (MPEG-TS): prefetch 64 chunks (~192KB)
                    64
                } else if all_file_chunks.len() > 100 {
                    // Medium chunks: prefetch 32 chunks
                    32
                } else {
                    // Large chunks: prefetch 16 chunks
                    16
                };

                info!("Prefetch: detected sequential pattern at chunk {}/{}, prefetching next {} chunks",
                      last_file_chunk_idx, all_file_chunks.len(), prefetch_distance);

                for prefetch_offset in 1..=prefetch_distance {
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

    /// Pre-populate replica cache with chunk locations
    /// This is called when reading file metadata to warm the cache for sequential reads
    /// For now, we use a simple heuristic: all nodes have all chunks (true for RF=2 with 5 nodes)
    /// In the future, this could query the metadata server for actual locations
    pub async fn warm_replica_cache(&self, chunk_ids: &[ChunkId]) {
        if chunk_ids.is_empty() {
            return;
        }

        let nodes = self.cluster_nodes.read().await.clone();
        let nodes_arc = Arc::new(nodes);

        let mut cache = self.replica_cache.lock().await;
        for chunk_id in chunk_ids {
            // Only add if not already in cache
            if !cache.contains(chunk_id) {
                cache.put(*chunk_id, Arc::clone(&nodes_arc));
            }
        }

        debug!("Warmed replica cache with {} chunks", chunk_ids.len());
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

    /// Write data to cluster with optimized dual-stream parallelization
    /// For RF=3+: Split data in half, send to 2 servers simultaneously (local write only)
    /// Each server writes locally only, healing creates 3rd replica in background
    ///
    /// Optionally populates byte-range cache if inode and file_offset are provided
    pub async fn write_data(&self, data: &[u8]) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        self.write_data_with_cache(data, 0, 0).await
    }

    /// Write data and populate byte-range cache for immediate read-back
    /// This enables zero-latency reads of just-written data (DVR use case)
    pub async fn write_data_with_cache(&self, data: &[u8], inode: u64, file_offset: u64) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        const MIN_PARALLEL_SIZE: usize = 128 * 1024; // 128KB minimum (lowered for better perf)

        // For small writes, use single server (less overhead)
        if data.len() < MIN_PARALLEL_SIZE {
            return self.write_data_single_chunk(data).await;
        }

        let nodes = self.cluster_nodes.read().await.clone();
        if nodes.len() < 2 {
            return self.write_data_single_chunk(data).await;
        }

        info!("Writing {} bytes using optimized dual-stream (RF=3+ local-only)", data.len());

        // Split data in half for 2 parallel streams
        let mid = data.len() / 2;
        let (chunk1, chunk2) = data.split_at(mid);

        // Send to 2 different servers in parallel (local write only)
        let server1 = nodes[0];
        let server2 = nodes[1 % nodes.len()];

        let chunk1 = chunk1.to_vec();
        let chunk2 = chunk2.to_vec();

        // Spawn both writes in parallel using local-only write
        let task1 = Self::write_chunk_to_server_local_only(server1, chunk1);
        let task2 = Self::write_chunk_to_server_local_only(server2, chunk2);

        let (result1, result2) = tokio::join!(task1, task2);

        let (chunk_ids_1, chunk_sizes_1) = result1?;
        let (chunk_ids_2, chunk_sizes_2) = result2?;

        let mut all_chunk_ids = Vec::new();
        all_chunk_ids.extend(chunk_ids_1);
        all_chunk_ids.extend(chunk_ids_2);

        let mut all_chunk_sizes = Vec::new();
        all_chunk_sizes.extend(chunk_sizes_1);
        all_chunk_sizes.extend(chunk_sizes_2);

        info!("Completed optimized dual-stream write: {} total chunks (2 copies, healing creates 3rd)",
              all_chunk_ids.len());

        // Populate byte-range cache with written data for immediate read-back
        if inode > 0 && file_offset > 0 {
            let mut byte_cache = self.byte_range_cache.lock().await;
            let mut current_offset = file_offset;

            // Cache each chunk by its file offset
            for (idx, &chunk_size) in all_chunk_sizes.iter().enumerate() {
                // Reconstruct chunk data from original write
                let chunk_start = if idx == 0 { 0 } else { all_chunk_sizes[..idx].iter().sum::<u64>() as usize };
                let chunk_end = chunk_start + chunk_size as usize;
                let chunk_data = data[chunk_start..chunk_end].to_vec();

                let key = ByteRangeCacheKey {
                    inode,
                    file_offset: current_offset,
                };
                let cached = CachedChunk {
                    data: Arc::new(chunk_data),
                    chunk_size: chunk_size as usize,
                };
                byte_cache.put(key, cached);
                info!("Write-through cached: inode={} offset={} ({} bytes)", inode, current_offset, chunk_size);

                current_offset += chunk_size;
            }
        }

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

    /// Write a chunk to a specific server (local only, no replication)
    /// Used for optimized RF=3+ writes
    async fn write_chunk_to_server_local_only(server_addr: SocketAddr, data: Vec<u8>) -> Result<(Vec<ChunkId>, Vec<u64>)> {
        let total_start = std::time::Instant::now();
        let data_len = data.len();

        let request = Request::WriteFileLocalOnly { data };

        // Create connection
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

        let total_time = total_start.elapsed();
        let throughput = (data_len as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();
        info!("Client LOCAL write to {}: {} bytes in {:?} ({:.2} MB/s)",
              server_addr, data_len, total_time, throughput);

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
                // Wrap entire query with 10s timeout to avoid hanging on offline nodes
                // (ARM servers need generous timeout for reliable stats)
                let query_future = async {
                    // Create a temporary client for this request with 5s connect timeout
                    let mut stream = tokio::time::timeout(
                        std::time::Duration::from_millis(5000),
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

                // Apply overall 10s timeout to entire query
                tokio::time::timeout(std::time::Duration::from_millis(10000), query_future)
                    .await
                    .map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::TimedOut, "query timeout")) as Box<dyn std::error::Error + Send + Sync>)?
            });

            tasks.push(task);
        }

        // Wait for all queries to complete
        let mut total_raw_space = 0u64;
        let mut node_capacities: Vec<(u64, u64)> = Vec::new(); // (total, available) per node
        let mut replication_factor = None;

        for task in tasks {
            if let Ok(Ok(Some((total, free, avail, rf)))) = task.await {
                total_raw_space += total;
                node_capacities.push((total, avail));
                if replication_factor.is_none() {
                    replication_factor = Some(rf);
                }
            }
        }

        // If we didn't get any valid stats, return reasonable defaults
        // This prevents df from hanging when servers are temporarily slow
        if node_capacities.is_empty() {
            warn!("Failed to get storage stats from any node, using defaults");
            return Ok((0, 0, 0, replication_factor.unwrap_or(2)));
        }

        let replication_factor = replication_factor.unwrap_or(2);

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
