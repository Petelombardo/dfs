use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use dashmap::DashMap;
use dfs_common::{Message, MessageEnvelope, Request, RequestId, Response, ErrorCode, ClusterMessage};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

/// Handler trait for processing messages
pub trait MessageHandler: Send + Sync {
    fn handle_request(
        &self,
        request: Request,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>>;

    fn handle_cluster_message(
        &self,
        message: ClusterMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>>;
}

/// Network server for handling node-to-node communication
/// Optimized for SBC environments (connection reuse, async I/O)
pub struct NetworkServer<H: MessageHandler> {
    /// Address to listen on
    listen_addr: SocketAddr,

    /// Request ID counter
    next_request_id: Arc<AtomicU64>,

    /// Shutdown signal
    shutdown_tx: Option<mpsc::Sender<()>>,

    /// Message handler
    handler: Arc<H>,

}

impl<H: MessageHandler + 'static> NetworkServer<H> {
    /// Create a new network server
    pub fn new(listen_addr: SocketAddr, handler: Arc<H>) -> Self {
        Self {
            listen_addr,
            next_request_id: Arc::new(AtomicU64::new(1)),
            shutdown_tx: None,
            handler,
        }
    }

    /// Start the server (runs until shutdown)
    pub async fn start(&mut self) -> Result<()> {
        let listener = TcpListener::bind(self.listen_addr)
            .await
            .with_context(|| format!("Failed to bind to {}", self.listen_addr))?;

        info!("Network server listening on {}", self.listen_addr);

        // Limit concurrent in-flight connections. Each connection holds a permit for
        // its lifetime, so the runtime never has more than MAX_CONNECTIONS tasks
        // simultaneously blocked in read/write. Without this cap, a burst of connections
        // (e.g. rapid-fire small writes from a SQLite WAL session) can exhaust the tokio
        // thread pool, leaving no threads to service new requests and causing a deadlock.
        const MAX_CONNECTIONS: usize = 512;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, peer_addr)) => {
                            debug!("Accepted connection from {}", peer_addr);
                            let _ = stream.set_nodelay(true);
                            let handler = self.handler.clone();
                            let sem = semaphore.clone();

                            // Try to acquire a permit without blocking the accept loop.
                            // If at capacity, send a busy error and close immediately so
                            // the client gets a fast failure rather than a silent hang.
                            match sem.clone().try_acquire_owned() {
                                Ok(permit) => {
                                    tokio::spawn(async move {
                                        let _permit = permit; // released on drop
                                        if let Err(e) = handle_connection(stream, peer_addr, handler).await {
                                            error!("Connection error from {}: {}", peer_addr, e);
                                        }
                                    });
                                }
                                Err(_) => {
                                    let in_use = MAX_CONNECTIONS - sem.available_permits();
                                    warn!("Connection limit reached ({}/{}) — rejecting {}", in_use, MAX_CONNECTIONS, peer_addr);
                                    tokio::spawn(async move {
                                        let response = MessageEnvelope::new(
                                            RequestId::new(0),
                                            dfs_common::Message::Response(dfs_common::Response::Error {
                                                message: "Server busy — connection limit reached".to_string(),
                                                code: ErrorCode::ServerBusy,
                                            }),
                                        );
                                        if let Ok(encoded) = response.to_bytes() {
                                            let len = (encoded.len() as u32).to_be_bytes();
                                            let _ = stream.write_all(&len).await;
                                            let _ = stream.write_all(&encoded).await;
                                        }
                                    });
                                }
                            }
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
async fn handle_connection<H: MessageHandler>(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    handler: Arc<H>,
) -> Result<()> {
    let mut read_buf = BytesMut::with_capacity(8192); // 8KB buffer (SBC-friendly)

    // Close idle connections after 30s of inactivity so pooled connections from
    // peers don't accumulate indefinitely on the leader (which receives all healing ops).
    const IDLE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

    loop {
        // Read message from stream, with idle timeout
        let read_result = tokio::time::timeout(
            IDLE_TIMEOUT,
            read_message(&mut stream, &mut read_buf),
        ).await;

        match read_result {
            Err(_) => {
                // Idle timeout — close connection to free the fd
                debug!("Connection from {} idle for {}s, closing", peer_addr, IDLE_TIMEOUT.as_secs());
                break;
            }
            Ok(Ok(Some(envelope))) => {
                debug!(
                    "Received message from {}: request_id={}",
                    peer_addr, envelope.request_id.0
                );

                // Process message and send response
                let response = process_message(envelope, handler.clone()).await;

                // Send response
                if let Err(e) = write_message(&mut stream, &response).await {
                    error!("Failed to send response to {}: {}", peer_addr, e);
                    break;
                }
            }
            Ok(Ok(None)) => {
                // Connection closed gracefully
                debug!("Connection closed by {}", peer_addr);
                break;
            }
            Ok(Err(e)) => {
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
    // Per-read-operation timeout. Applied to every read_buf call so a client that
    // dies mid-transfer (e.g. after sending the length prefix but before the payload)
    // doesn't hold the connection open forever and leak the fd / task slot.
    const READ_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);

    loop {
        if buf.len() >= 4 {
            let mut length_bytes = [0u8; 4];
            length_bytes.copy_from_slice(&buf[..4]);
            let length = u32::from_be_bytes(length_bytes) as usize;

            if buf.len() >= 4 + length {
                buf.advance(4);
                let message_bytes = buf.split_to(length);
                let mut envelope = MessageEnvelope::from_bytes(&message_bytes)
                    .context("Failed to deserialize message")?;

                // Split-frame ChunkData: raw payload follows the envelope.
                if let dfs_common::Message::Response(dfs_common::Response::ChunkData { ref mut data, .. }) = envelope.message {
                    if data.is_empty() {
                        // Drain raw payload from buf then stream.
                        while buf.len() < 4 {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading chunk payload length"))?
                                .context("IO error reading chunk payload length")?;
                            if n == 0 { anyhow::bail!("Connection closed reading chunk payload length"); }
                        }
                        let mut plen_bytes = [0u8; 4];
                        plen_bytes.copy_from_slice(&buf[..4]);
                        buf.advance(4);
                        let plen = u32::from_be_bytes(plen_bytes) as usize;

                        while buf.len() < plen {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading chunk payload"))?
                                .context("IO error reading chunk payload")?;
                            if n == 0 { anyhow::bail!("Connection closed reading chunk payload"); }
                        }
                        *data = buf.split_to(plen).to_vec();
                    }
                }

                // Split-frame WriteFileLocalOnly: raw payload follows the envelope.
                if let dfs_common::Message::Request(dfs_common::Request::WriteFileLocalOnly { ref mut data, .. }) = envelope.message {
                    if data.is_empty() {
                        // Drain raw payload from buf then stream.
                        while buf.len() < 4 {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading write payload length"))?
                                .context("IO error reading write payload length")?;
                            if n == 0 { anyhow::bail!("Connection closed reading write payload length"); }
                        }
                        let mut plen_bytes = [0u8; 4];
                        plen_bytes.copy_from_slice(&buf[..4]);
                        buf.advance(4);
                        let plen = u32::from_be_bytes(plen_bytes) as usize;

                        while buf.len() < plen {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading write payload"))?
                                .context("IO error reading write payload")?;
                            if n == 0 { anyhow::bail!("Connection closed reading write payload"); }
                        }
                        *data = buf.split_to(plen).to_vec();
                        debug!("Received split-frame write request: {} bytes", plen);
                    }
                }

                return Ok(Some(envelope));
            }
        }

        let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
            .map_err(|_| anyhow::anyhow!("Timeout reading message frame"))?
            .context("IO error reading message frame")?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            } else {
                anyhow::bail!("Connection closed with incomplete message");
            }
        }
    }
}

/// Write a framed message to the stream.
/// ChunkData responses use split-frame encoding to avoid a bincode copy of the payload:
///   [4B envelope len][bincode envelope (data=empty)][4B raw len][raw bytes]
/// All other messages use standard framing: [4B len][bincode bytes]
async fn write_message(stream: &mut TcpStream, envelope: &MessageEnvelope) -> Result<()> {
    if let dfs_common::Message::Response(dfs_common::Response::ChunkData { ref data, ref chunk_id, ref cache_stats, ref arc_data, ref arc_range }) = envelope.message {
        let stub = MessageEnvelope::new(envelope.request_id, dfs_common::Message::Response(
            dfs_common::Response::ChunkData { chunk_id: *chunk_id, data: vec![], cache_stats: *cache_stats, arc_data: None, arc_range: None }
        ));
        // Use arc_data directly if present (zero extra copy). When arc_range is also
        // set, write only the requested sub-slice — used by striped half-fetches to
        // avoid cloning the slice out of the cached chunk Arc.
        let payload: &[u8] = match (arc_data, arc_range) {
            (Some(arc), Some((start, end))) => &arc[*start..*end],
            (Some(arc), None) => arc.as_slice(),
            (None, _) => data,
        };
        dfs_common::protocol::write_chunk_response(stream, &stub, payload).await?;
        return Ok(());
    }

    let message_bytes = envelope.to_bytes()?;
    let length = message_bytes.len() as u32;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&message_bytes).await?;
    stream.flush().await?;
    Ok(())
}

/// Process a message and return response
async fn process_message<H: MessageHandler>(
    envelope: MessageEnvelope,
    handler: Arc<H>,
) -> MessageEnvelope {
    let response = match envelope.message {
        Message::Request(req) => {
            debug!("Processing request: {:?}", req);
            let response = handler.handle_request(req).await;
            Message::Response(response)
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
            let response = handler.handle_cluster_message(cluster_msg).await;
            Message::Response(response)
        }
    };

    MessageEnvelope::new(envelope.request_id, response)
}

/// Network client for sending requests to other nodes
/// Maintains a per-peer connection pool to avoid opening a new TCP connection
/// for every request (which exhausts file descriptors under heavy healing/replication load).
pub struct NetworkClient {
    /// Request ID counter
    next_request_id: Arc<AtomicU64>,
    /// Idle connection pool: per-peer queue of reusable TcpStreams (cap 4 per peer)
    pool: Arc<DashMap<SocketAddr, Mutex<VecDeque<TcpStream>>>>,
}

impl NetworkClient {
    /// Create a new network client
    pub fn new() -> Self {
        Self {
            next_request_id: Arc::new(AtomicU64::new(1)),
            pool: Arc::new(DashMap::new()),
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

        // Try a pooled connection first; fall back to a fresh one if the pool is empty
        // or the connection has gone stale (detected by write/read failure).
        let stream = if let Some(entry) = self.pool.get(&target) {
            entry.lock().await.pop_front()
        } else {
            None
        };

        let mut stream = match stream {
            Some(s) => {
                // Before reusing, non-blockingly check if the server already closed
                // this connection. try_read is non-blocking: WouldBlock means the
                // socket is open and idle (good to reuse); Ok(0) means the remote sent
                // FIN (CLOSE-WAIT — discard); any other result means discard.
                // DO NOT use ready(READABLE).await here — that blocks until data
                // arrives, turning every idle-connection reuse into a multi-second hang.
                let mut buf = [0u8; 1];
                let peer_closed = match s.try_read(&mut buf) {
                    Ok(0) => true,   // EOF — server closed connection
                    Ok(_) => true,   // Unexpected data in pool — discard
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false, // idle, healthy
                    Err(_) => true,  // Any other error — discard
                };

                if peer_closed {
                    debug!("Pooled connection to {} was closed by peer, opening fresh", target);
                    let mut s = s;
                    let _ = s.shutdown().await;
                    let fresh = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        TcpStream::connect(target),
                    ).await
                        .map_err(|_| anyhow::anyhow!("Connect timeout to {}", target))?
                        .with_context(|| format!("Failed to connect to {}", target))?;
                    let _ = fresh.set_nodelay(true);
                    fresh
                } else {
                    debug!("Reusing pooled connection to {}", target);
                    s
                }
            }
            None => {
                debug!("Connecting to {}", target);
                let fresh = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    TcpStream::connect(target),
                ).await
                    .map_err(|_| anyhow::anyhow!("Connect timeout to {}", target))?
                    .with_context(|| format!("Failed to connect to {}", target))?;
                let _ = fresh.set_nodelay(true);
                fresh
            }
        };

        // Send message
        if let Err(e) = write_message(&mut stream, &envelope).await {
            // Stale pooled connection — open a fresh one and retry once
            debug!("Pooled connection to {} failed ({}), retrying with new connection", target, e);
            let mut fresh = tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                TcpStream::connect(target),
            ).await
                .map_err(|_| anyhow::anyhow!("Connect timeout to {}", target))?
                .with_context(|| format!("Failed to reconnect to {}", target))?;
            let _ = fresh.set_nodelay(true);
            write_message(&mut fresh, &envelope).await?;
            stream = fresh;
        }

        // Read response — bounded so a hung peer never holds a semaphore permit forever.
        let mut read_buf = BytesMut::with_capacity(8192);
        let response = tokio::time::timeout(
            tokio::time::Duration::from_secs(30),
            read_message(&mut stream, &mut read_buf),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Read timeout from {}", target))?  // Elapsed → Err
        ?                                                                  // Result<Option> → Option
        .context("Connection closed before receiving response")?;          // Option → MessageEnvelope

        debug!("Received response from {}", target);

        // Return connection to pool (cap 4 idle per peer).
        // If the pool is full, set SO_LINGER(0) before dropping so the kernel sends
        // RST instead of going through TIME_WAIT — prevents orphaned socket accumulation
        // under heavy healing load where many short-lived connections are created.
        {
            let entry = self.pool
                .entry(target)
                .or_insert_with(|| Mutex::new(VecDeque::new()));
            let mut queue = entry.lock().await;
            if queue.len() < 4 {
                queue.push_back(stream);
            } else {
                // Pool full — explicitly shut down before dropping so the kernel
                // completes the TCP close handshake promptly rather than leaving
                // the socket in TIME_WAIT as an orphan.
                let _ = stream.shutdown().await;
            }
        }

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

    // Simple test handler
    struct TestHandler;

    impl MessageHandler for TestHandler {
        fn handle_request(
            &self,
            _request: Request,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
            Box::pin(async move {
                Response::Bool { value: false }
            })
        }

        fn handle_cluster_message(
            &self,
            _message: ClusterMessage,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + '_>> {
            Box::pin(async move {
                Response::Ok { data: None }
            })
        }
    }

    #[tokio::test]
    async fn test_client_server() {
        // Start server
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(server_addr).await.unwrap();
        let actual_addr = listener.local_addr().unwrap();

        // Spawn server task
        let handler = Arc::new(TestHandler);
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(stream, peer, handler).await.ok();
        });

        // Give server time to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Create client and send message
        let client = NetworkClient::new();
        let message = Message::Request(Request::HasChunk {
            chunk_id: ChunkId::from_hash([1u8; 32]),
        });

        let response = client.send_message(actual_addr, message).await.unwrap();

        // Should get a Bool response from our test handler
        match response.message {
            Message::Response(Response::Bool { value }) => {
                assert!(!value);
            }
            _ => panic!("Expected Bool response"),
        }
    }
}
