use anyhow::{Context, Result};
use dfs_common::{ChunkId, FileMetadata, Message, Request, Response};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tracing::{debug, warn};

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

        // Serialize message
        let message = Message::Request(request);
        let encoded = bincode::serialize(&message).context("Failed to serialize message")?;

        // Send message with length prefix
        let len = encoded.len() as u32;
        stream
            .write_all(&len.to_be_bytes())
            .await
            .context("Failed to write message length")?;
        stream
            .write_all(&encoded)
            .await
            .context("Failed to write message")?;
        stream.flush().await.context("Failed to flush stream")?;

        // Read response
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .context("Failed to read response length")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        let mut buf = vec![0u8; len];
        stream
            .read_exact(&mut buf)
            .await
            .context("Failed to read response")?;

        // Deserialize response
        let response_message: Message =
            bincode::deserialize(&buf).context("Failed to deserialize response")?;

        match response_message {
            Message::Response(response) => Ok(response),
            _ => anyhow::bail!("Expected Response message"),
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

    /// Read data from cluster by chunk IDs
    pub async fn read_data(&self, chunk_ids: &[ChunkId]) -> Result<Vec<u8>> {
        let mut all_data = Vec::new();

        for chunk_id in chunk_ids {
            let request = Request::ReadChunk {
                chunk_id: *chunk_id,
            };

            let response = self.send_request_with_retry(request).await?;

            match response {
                Response::ChunkData { data, .. } => {
                    all_data.extend_from_slice(&data);
                }
                Response::Error { message, .. } => {
                    anyhow::bail!("Failed to read chunk: {}", message);
                }
                _ => anyhow::bail!("Unexpected response type"),
            }
        }

        Ok(all_data)
    }

    /// Write data to cluster
    pub async fn write_data(&self, data: &[u8]) -> Result<Vec<ChunkId>> {
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
}
