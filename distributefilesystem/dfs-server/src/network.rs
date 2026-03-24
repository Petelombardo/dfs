use anyhow::{Context, Result};
use bytes::{Buf, BufMut, BytesMut};
use dfs_common::{Message, MessageEnvelope, RequestId};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Network server for handling node-to-node communication
/// Optimized for SBC environments (connection reuse, async I/O)
pub struct NetworkServer {
    /// Address to listen on
    listen_addr: SocketAddr,

    /// Request ID counter
    next_request_id: Arc<AtomicU64>,

    /// Shutdown signal
    shutdown_tx: Option<mpsc::Sender<()>>,
}

impl NetworkServer {
    /// Create a new network server
    pub fn new(listen_addr: SocketAddr) -> Self {
        Self {
            listen_addr,
            next_request_id: Arc::new(AtomicU64::new(1)),
            shutdown_tx: None,
        }
    }

    /// Start the server (runs until shutdown)
    pub async fn start(&mut self) -> Result<()> {
        let listener = TcpListener::bind(self.listen_addr)
            .await
            .with_context(|| format!("Failed to bind to {}", self.listen_addr))?;

        info!("Network server listening on {}", self.listen_addr);

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            debug!("Accepted connection from {}", peer_addr);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, peer_addr).await {
                                    error!("Connection error from {}: {}", peer_addr, e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("Shutting down network server");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Shutdown the server
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(()).await;
        }
        Ok(())
    }

    /// Get next request ID
    pub fn next_request_id(&self) -> RequestId {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        RequestId::new(id)
    }
}

/// Handle a single TCP connection
async fn handle_connection(mut stream: TcpStream, peer_addr: SocketAddr) -> Result<()> {
    let mut read_buf = BytesMut::with_capacity(8192); // 8KB buffer (SBC-friendly)

    loop {
        // Read message from stream
        match read_message(&mut stream, &mut read_buf).await {
            Ok(Some(envelope)) => {
                debug!(
                    "Received message from {}: request_id={}",
                    peer_addr, envelope.request_id.0
                );

                // Process message and send response
                let response = process_message(envelope).await;

                // Send response
                if let Err(e) = write_message(&mut stream, &response).await {
                    error!("Failed to send response to {}: {}", peer_addr, e);
                    break;
                }
            }
            Ok(None) => {
                // Connection closed gracefully
                debug!("Connection closed by {}", peer_addr);
                break;
            }
            Err(e) => {
                warn!("Error reading from {}: {}", peer_addr, e);
                break;
            }
        }
    }

    Ok(())
}

/// Read a framed message from the stream
/// Format: [4 bytes length][message bytes]
async fn read_message(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
) -> Result<Option<MessageEnvelope>> {
    // Read length prefix (4 bytes)
    loop {
        if buf.len() >= 4 {
            let mut length_bytes = [0u8; 4];
            length_bytes.copy_from_slice(&buf[..4]);
            let length = u32::from_be_bytes(length_bytes) as usize;

            // Check if we have the full message
            if buf.len() >= 4 + length {
                buf.advance(4); // Skip length prefix

                // Deserialize message
                let message_bytes = buf.split_to(length);
                let envelope = MessageEnvelope::from_bytes(&message_bytes)
                    .context("Failed to deserialize message")?;

                return Ok(Some(envelope));
            }
        }

        // Read more data
        if stream.read_buf(buf).await? == 0 {
            if buf.is_empty() {
                return Ok(None);
            } else {
                anyhow::bail!("Connection closed with incomplete message");
            }
        }
    }
}

/// Write a framed message to the stream
/// Format: [4 bytes length][message bytes]
async fn write_message(stream: &mut TcpStream, envelope: &MessageEnvelope) -> Result<()> {
    let message_bytes = envelope.to_bytes()?;
    let length = message_bytes.len() as u32;

    // Write length prefix
    stream.write_all(&length.to_be_bytes()).await?;

    // Write message
    stream.write_all(&message_bytes).await?;

    stream.flush().await?;

    Ok(())
}

/// Process a message and return response
/// TODO: Wire up to actual storage/metadata in Phase 4
async fn process_message(envelope: MessageEnvelope) -> MessageEnvelope {
    use dfs_common::{ErrorCode, Response};

    let response = match envelope.message {
        Message::Request(req) => {
            debug!("Processing request: {:?}", req);
            // TODO: Handle actual requests in Phase 4
            Message::Response(Response::Error {
                message: "Not yet implemented".to_string(),
                code: ErrorCode::InternalError,
            })
        }
        Message::Response(_) => {
            warn!("Received response message on server - ignoring");
            Message::Response(Response::Error {
                message: "Server does not accept response messages".to_string(),
                code: ErrorCode::InvalidRequest,
            })
        }
        Message::Cluster(cluster_msg) => {
            debug!("Processing cluster message: {:?}", cluster_msg);
            // TODO: Handle cluster messages in Phase 4
            Message::Response(Response::Ok { data: None })
        }
    };

    MessageEnvelope::new(envelope.request_id, response)
}

/// Network client for sending requests to other nodes
/// Maintains connection pool for efficiency
pub struct NetworkClient {
    /// Request ID counter
    next_request_id: Arc<AtomicU64>,
}

impl NetworkClient {
    /// Create a new network client
    pub fn new() -> Self {
        Self {
            next_request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Send a message to a remote node and wait for response
    pub async fn send_message(
        &self,
        target: SocketAddr,
        message: Message,
    ) -> Result<MessageEnvelope> {
        let request_id = self.next_request_id();
        let envelope = MessageEnvelope::new(request_id, message);

        // Connect to target
        let mut stream = TcpStream::connect(target)
            .await
            .with_context(|| format!("Failed to connect to {}", target))?;

        debug!("Connected to {}, sending message", target);

        // Send message
        write_message(&mut stream, &envelope).await?;

        // Read response
        let mut read_buf = BytesMut::with_capacity(8192);
        let response = read_message(&mut stream, &mut read_buf)
            .await?
            .context("Connection closed before receiving response")?;

        debug!("Received response from {}", target);

        Ok(response)
    }

    /// Get next request ID
    fn next_request_id(&self) -> RequestId {
        let id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        RequestId::new(id)
    }
}

impl Default for NetworkClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dfs_common::{Request, Response, ChunkId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_message_framing() {
        let (mut server, mut client) = tokio::io::duplex(1024);

        let original = MessageEnvelope::new(
            RequestId::new(42),
            Message::Request(Request::HasChunk {
                chunk_id: ChunkId::from_hash([0u8; 32]),
            }),
        );

        // Spawn writer
        let write_msg = original.clone();
        let writer = tokio::spawn(async move {
            let message_bytes = write_msg.to_bytes().unwrap();
            let length = message_bytes.len() as u32;
            server.write_all(&length.to_be_bytes()).await.unwrap();
            server.write_all(&message_bytes).await.unwrap();
            server.flush().await.unwrap();
        });

        // Read message
        let mut length_bytes = [0u8; 4];
        client.read_exact(&mut length_bytes).await.unwrap();
        let length = u32::from_be_bytes(length_bytes) as usize;

        let mut message_bytes = vec![0u8; length];
        client.read_exact(&mut message_bytes).await.unwrap();
        let received = MessageEnvelope::from_bytes(&message_bytes).unwrap();

        writer.await.unwrap();

        assert_eq!(original.request_id, received.request_id);
    }

    #[tokio::test]
    async fn test_client_server() {
        // Start server
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(server_addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Spawn server task
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(stream, peer).await.ok();
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create client and send message
        let client = NetworkClient::new();
        let message = Message::Request(Request::HasChunk {
            chunk_id: ChunkId::from_hash([1u8; 32]),
        });

        let response = client.send_message(actual_addr, message).await.unwrap();

        // Should get an error response (not implemented yet)
        match response.message {
            Message::Response(Response::Error { .. }) => {
                // Expected
            }
            _ => panic!("Expected error response"),
        }
    }
}
