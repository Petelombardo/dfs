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
use std::os::unix::io::AsRawFd;

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

    /// Called by the network layer as soon as a split-frame MultiPatch envelope
    /// is decoded — before the patch bytes have been read off the wire. Implementations
    /// should kick off the chunk disk read immediately so it overlaps with the remaining
    /// network receive time. Default is a no-op; only the storage server overrides it.
    fn start_prefetch_for_patch(&self, _chunk_id: dfs_common::types::ChunkId) {}
}

// A connection counts against this for its entire open lifetime, including time
// spent idle in a client's reuse pool — not just while actively serving a request.
// 128 was undersized against concurrency caps we've since raised for QD32 tuning
// (PIPELINE_MAX_ITEMS=32 dual-written to 2 replicas, range-fetch up to 6/file/node,
// client POOL_SIZE=20 idle per peer) plus healing fan-out and gossip/heartbeat
// traffic, all drawing from the same budget on every node at once. 384 gives real
// headroom while remaining tiny next to the 65536 NOFILE ulimit the systemd unit
// already grants.
pub const MAX_CONNECTIONS: usize = 384;

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

    /// Shared semaphore — available_permits() reports free slots; cloned to Server for stats.
    pub conn_semaphore: Arc<tokio::sync::Semaphore>,
}

impl<H: MessageHandler + 'static> NetworkServer<H> {
    /// Create a new network server
    pub fn new(listen_addr: SocketAddr, handler: Arc<H>) -> Self {
        Self {
            listen_addr,
            next_request_id: Arc::new(AtomicU64::new(1)),
            shutdown_tx: None,
            handler,
            conn_semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
        }
    }

    /// Start the server (runs until shutdown)
    pub async fn start(&mut self) -> Result<()> {
        let listener = TcpListener::bind(self.listen_addr)
            .await
            .with_context(|| format!("Failed to bind to {}", self.listen_addr))?;

        info!("Network server listening on {}", self.listen_addr);

        let semaphore = self.conn_semaphore.clone();

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((mut stream, peer_addr)) => {
                            debug!("Accepted connection from {}", peer_addr);
                            let _ = stream.set_nodelay(true);
                            // Enable TCP keepalive so the kernel detects dead peers
                            // (CLOSE-WAIT, network partition) in ~11s rather than waiting
                            // for the 30s application-level idle timeout. Without this,
                            // a burst of dead connections each hold a semaphore permit
                            // permanently — the idle timeout tasks can't be polled when
                            // the runtime is saturated, causing a permanent hang.
                            // Settings: idle=5s, interval=2s, retries=3 → detect in ~11s.
                            unsafe {
                                let fd = stream.as_raw_fd();
                                let one: libc::c_int = 1;
                                libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE,
                                    &one as *const _ as *const libc::c_void,
                                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                                let secs: libc::c_int = 5;
                                libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE,
                                    &secs as *const _ as *const libc::c_void,
                                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                                let interval: libc::c_int = 2;
                                libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL,
                                    &interval as *const _ as *const libc::c_void,
                                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                                let retries: libc::c_int = 3;
                                libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPCNT,
                                    &retries as *const _ as *const libc::c_void,
                                    std::mem::size_of::<libc::c_int>() as libc::socklen_t);
                            }
                            let handler = self.handler.clone();
                            let sem = semaphore.clone();

                            // Try to acquire a permit without blocking the accept loop.
                            // If at capacity, send a busy error and close immediately so
                            // the client gets a fast failure rather than a silent hang.
                            match sem.clone().try_acquire_owned() {
                                Ok(permit) => {
                                    let in_use = MAX_CONNECTIONS - sem.available_permits();
                                    if in_use > MAX_CONNECTIONS * 3 / 4 {
                                        warn!("Connection pressure: {}/{} slots in use", in_use, MAX_CONNECTIONS);
                                    }
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

    // Close idle connections after 5 minutes of inactivity. Must be longer than
    // READ_TIMEOUT (30s) so the outer idle wrapper always fires first (producing a
    // clean DEBUG log) rather than the inner read timeout producing a spurious WARN.
    // 30s was too short: during quiet I/O periods (VM paused, no disk activity)
    // all inter-node connections would tear down and rebuild every 30s, causing the
    // client to briefly see the leader as unreachable on every cluster health refresh.
    const IDLE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(300);

    loop {
        // Read message from stream, with idle timeout.
        // Pass a prefetch callback so the network layer can kick off the chunk
        // disk read as soon as a split-frame MultiPatch envelope is decoded —
        // before the patch bytes have arrived — overlapping disk I/O with network receive.
        let read_start = std::time::Instant::now();
        let read_result = tokio::time::timeout(
            IDLE_TIMEOUT,
            read_message(&mut stream, &mut read_buf, |cid| handler.start_prefetch_for_patch(cid)),
        ).await;
        // NETTIMING read_wait: dominated by idle time since the last request/response
        // on this persistent connection (client-side think-time between reuses), NOT
        // purely this request's own transmission time — a large value here on its own
        // doesn't mean this request was slow, only logged for completeness per "track
        // gaps between ops too" — cross-reference against the client's own rpc timing
        // for the same logical operation to interpret it.
        let read_elapsed = read_start.elapsed();

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

                // Process message and send response — bounded so a hung handler
                // (e.g. a blocking disk read that was never moved to spawn_blocking)
                // can't hold a semaphore permit indefinitely.
                const HANDLER_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(120);
                // NETTIMING: added 2026-07-12 alongside SPTIMING (in server.rs) to
                // isolate whether a slow MultiPatch round trip (measured client-side,
                // 233ms-1.5s+ under kdiskmark-style load, while handle_multi_patch's own
                // SPTIMING showed 5-25ms) is spent inside process_message/handle_request
                // (would show up here) or purely in network/connection-layer transit
                // (this would be fast here despite the client seeing it slow).
                let dispatch_start = std::time::Instant::now();
                let response = match tokio::time::timeout(
                    HANDLER_TIMEOUT,
                    process_message(envelope, handler.clone()),
                ).await {
                    Ok(r) => r,
                    Err(_) => {
                        warn!("Handler timed out (>120s) for request from {}, closing connection", peer_addr);
                        break;
                    }
                };
                let dispatch_elapsed = dispatch_start.elapsed();

                // Send response
                let write_start = std::time::Instant::now();
                if let Err(e) = write_message(&mut stream, &response).await {
                    error!("Failed to send response to {}: {}", peer_addr, e);
                    break;
                }
                let write_elapsed = write_start.elapsed();
                // Volume control (2026-07-22): the >=5ms gate below turned out to be the
                // COMMON case, not the exceptional one — one staging node accumulated 3.49M
                // NETTIMING lines in a 4.8GB dfs-server.log. That log lives on the SAME
                // filesystem as the chunk data, and durability is a per-device syncfs
                // (DurabilityCoalescer), so every client-facing patch barrier had to flush
                // those dirty log pages too — verbose logging was feeding directly back into
                // write latency. Routine samples now go to debug!; only genuinely
                // pathological dispatches stay at info! so a real stall is still visible in
                // production without re-enabling debug (which on a busy node is itself a
                // load event).
                const NETTIMING_INFO_MS: u128 = 500;
                let slow_ms = dispatch_elapsed.as_millis().max(write_elapsed.as_millis());
                if slow_ms >= NETTIMING_INFO_MS {
                    info!("NETTIMING peer={} read_wait={:?} dispatch={:?} write_response={:?}",
                        peer_addr, read_elapsed, dispatch_elapsed, write_elapsed);
                } else if slow_ms >= 5 {
                    debug!("NETTIMING peer={} read_wait={:?} dispatch={:?} write_response={:?}",
                        peer_addr, read_elapsed, dispatch_elapsed, write_elapsed);
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
async fn read_message<F>(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    on_multi_patch_envelope: F,
) -> Result<Option<MessageEnvelope>>
where
    F: Fn(dfs_common::types::ChunkId) + Send,
{
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

                // Split-frame MultiPatch: all patch Vec<u8> are empty as a signal.
                // Raw payload: [4B len0][data0][4B len1][data1]... for each patch.
                if let dfs_common::Message::Request(dfs_common::Request::MultiPatch { chunk_id, ref mut patches, .. }) = envelope.message {
                    if !patches.is_empty() && patches.iter().all(|(_, d)| d.is_empty()) {
                        // We have chunk_id before the patch bytes arrive — kick off the
                        // disk read immediately so it overlaps with the remaining network
                        // receive. handle_multi_patch() will await the result instead of
                        // starting a fresh disk read.
                        on_multi_patch_envelope(chunk_id);

                        while buf.len() < 4 {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading MultiPatch payload length"))?
                                .context("IO error reading MultiPatch payload length")?;
                            if n == 0 { anyhow::bail!("Connection closed reading MultiPatch payload length"); }
                        }
                        let mut plen_bytes = [0u8; 4];
                        plen_bytes.copy_from_slice(&buf[..4]);
                        buf.advance(4);
                        let plen = u32::from_be_bytes(plen_bytes) as usize;

                        while buf.len() < plen {
                            let n = tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                                .map_err(|_| anyhow::anyhow!("Timeout reading MultiPatch payload"))?
                                .context("IO error reading MultiPatch payload")?;
                            if n == 0 { anyhow::bail!("Connection closed reading MultiPatch payload"); }
                        }
                        let raw = buf.split_to(plen);
                        // Parse [4B len_i][data_i] for each patch in order.
                        let mut pos = 0usize;
                        for (_, data) in patches.iter_mut() {
                            if pos + 4 > raw.len() {
                                anyhow::bail!("MultiPatch split-frame: truncated length header");
                            }
                            let dlen = u32::from_be_bytes(raw[pos..pos+4].try_into().unwrap()) as usize;
                            pos += 4;
                            if pos + dlen > raw.len() {
                                anyhow::bail!("MultiPatch split-frame: truncated patch data");
                            }
                            *data = raw[pos..pos+dlen].to_vec();
                            pos += dlen;
                        }
                        debug!("Received split-frame MultiPatch: {} patches, {} bytes total", patches.len(), plen);
                    }
                }

                return Ok(Some(envelope));
            }
        }

        // If the buffer is empty we're waiting for the START of the next message.
        // Don't apply READ_TIMEOUT here — an idle connection will wait here for a
        // long time between requests (normal) and the outer IDLE_TIMEOUT wrapper
        // will close it cleanly after 300s without producing a WARN.
        // Only apply READ_TIMEOUT when buf already has partial frame bytes (mid-frame
        // stall from a client that sent some bytes then hung).
        let n = if buf.is_empty() {
            stream.read_buf(buf).await
                .context("IO error reading message frame")?
        } else {
            tokio::time::timeout(READ_TIMEOUT, stream.read_buf(buf)).await
                .map_err(|_| anyhow::anyhow!("Timeout reading message frame"))?
                .context("IO error reading message frame")?
        };
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
    // Coalesce the length prefix and body into one buffer/write — this is the generic
    // response path for every write/patch/metadata/heartbeat ack, so the extra packet
    // from a separate write_all directly adds RTT to those latency-bound RPCs.
    let mut framed = Vec::with_capacity(4 + message_bytes.len());
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&message_bytes);
    stream.write_all(&framed).await?;
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
    /// Idle connection pool: per-peer queue of reusable TcpStreams (cap 8 per peer).
    ///
    /// The per-peer `Mutex<VecDeque>` is `Arc`-wrapped so callers can clone the Arc
    /// out and DROP the DashMap shard guard (`get`/`entry` Ref) *before* awaiting
    /// `.lock()`. Holding a DashMap guard across an `.await` is what wedged
    /// gluster3 on 2026-07-19: a shard guard held across `entry.lock().await` while
    /// a peer was a black hole (TCP up, never replies) parked the holder, every
    /// other `send_message_inner` piled onto that shard's `lock_exclusive_slow`,
    /// and the whole tokio worker pool drained (0% CPU, all threads futex_wait) —
    /// which then froze both FUSE clients. DashMap shards are shared across many
    /// peer addresses, so one stuck peer jams every peer hashing to its shard.
    /// (68c3bee moved the slow `shutdown().await` out of the guard but left the
    /// `entry.lock().await` under it; Arc-ing the inner Mutex closes both paths.)
    pool: Arc<DashMap<SocketAddr, Arc<Mutex<VecDeque<TcpStream>>>>>,
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
    /// Like `send_message` but with a caller-specified response read timeout.
    /// Use for operations where the remote handler itself does slow work (e.g.
    /// PushChunkTo, which must read a chunk from disk and forward it before replying).
    pub async fn send_message_timeout(
        &self,
        target: SocketAddr,
        message: Message,
        response_timeout: std::time::Duration,
    ) -> Result<MessageEnvelope> {
        self.send_message_inner(target, message, response_timeout).await
    }

    pub async fn send_message(
        &self,
        target: SocketAddr,
        message: Message,
    ) -> Result<MessageEnvelope> {
        self.send_message_inner(target, message, std::time::Duration::from_secs(30)).await
    }

    async fn send_message_inner(
        &self,
        target: SocketAddr,
        message: Message,
        response_timeout: std::time::Duration,
    ) -> Result<MessageEnvelope> {
        let request_id = self.next_request_id();
        let envelope = MessageEnvelope::new(request_id, message);

        // Try a pooled connection first; fall back to a fresh one if the pool is empty
        // or the connection has gone stale (detected by write/read failure).
        // Clone the Arc<Mutex> out and drop the DashMap shard guard BEFORE awaiting
        // `.lock()` — never hold a DashMap guard across an `.await` (see the `pool`
        // field's doc comment for the black-hole wedge this prevents).
        let pool_slot = self.pool.get(&target).map(|entry| entry.value().clone());
        let stream = match pool_slot {
            Some(slot) => slot.lock().await.pop_front(),
            None => None,
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

        // Send message — bounded so a backed-up peer (TCP window closed) can't hold
        // a handler indefinitely. Mirrors the 30s read timeout below.
        const WRITE_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(30);
        let write_result = tokio::time::timeout(WRITE_TIMEOUT, write_message(&mut stream, &envelope)).await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Connection error (e.g. stale pooled connection) — retry with fresh one.
                debug!("Pooled connection to {} failed ({}), retrying with new connection", target, e);
                let mut fresh = tokio::time::timeout(
                    tokio::time::Duration::from_secs(5),
                    TcpStream::connect(target),
                ).await
                    .map_err(|_| anyhow::anyhow!("Connect timeout to {}", target))?
                    .with_context(|| format!("Failed to reconnect to {}", target))?;
                let _ = fresh.set_nodelay(true);
                tokio::time::timeout(WRITE_TIMEOUT, write_message(&mut fresh, &envelope))
                    .await
                    .map_err(|_| anyhow::anyhow!("Write timeout to {}", target))??;
                stream = fresh;
            }
            Err(_) => {
                // Write timed out — target's TCP window is closed/backed up.
                // Do NOT retry: a fresh connection won't help if the target is slow to
                // receive, and retrying doubles the worst-case latency from 30s to 95s,
                // pushing it past the 90s PushChunkTo leader timeout.
                return Err(anyhow::anyhow!("Write timeout to {}", target));
            }
        }

        // Read response — bounded so a hung peer never holds a semaphore permit forever.
        let mut read_buf = BytesMut::with_capacity(8192);
        let response = tokio::time::timeout(
            response_timeout,
            read_message(&mut stream, &mut read_buf, |_| {}),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Read timeout from {}", target))?  // Elapsed → Err
        ?                                                                  // Result<Option> → Option
        .context("Connection closed before receiving response")?;          // Option → MessageEnvelope

        debug!("Received response from {}", target);

        // Return connection to pool (cap 8 idle per peer).
        // If the pool is full, set SO_LINGER(0) before dropping so the kernel sends
        // RST instead of going through TIME_WAIT — prevents orphaned socket accumulation
        // under heavy healing load where many short-lived connections are created.
        //
        // IMPORTANT: the DashMap `entry` guard holds that shard's exclusive lock, and
        // DashMap shards are shared across multiple target addresses. `shutdown().await`
        // can block for a while against a slow/dead peer, so it must run only after the
        // guard is dropped — otherwise one stuck shutdown call jams `.entry()` for every
        // other target hashing to the same shard, which starves the whole node's tokio
        // worker pool once enough concurrent callers pile up on it (root cause of the
        // 2026-07-18 gluster2/gluster3 wedge during post-power-failure mass healing).
        // Clone the Arc<Mutex> out and drop the DashMap shard guard BEFORE awaiting
        // `.lock()` — same rule as the acquisition path above (see `pool` doc).
        let slot = self.pool
            .entry(target)
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
            .value()
            .clone();
        let stream_to_close = {
            let mut queue = slot.lock().await;
            if queue.len() < 8 {
                queue.push_back(stream);
                None
            } else {
                Some(stream)
            }
        };
        if let Some(mut stream) = stream_to_close {
            let _ = tokio::time::timeout(tokio::time::Duration::from_secs(5), stream.shutdown()).await;
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
