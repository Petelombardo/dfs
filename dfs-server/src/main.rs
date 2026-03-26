mod chunker;
mod cluster;
mod healing;
mod metadata;
mod network;
mod server;
mod storage;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dfs_common::Config;
use std::path::PathBuf;
use tracing::{debug, info, warn};
use tracing_subscriber;

#[derive(Parser)]
#[command(name = "dfs-server")]
#[command(about = "DFS Storage Node Server", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new DFS node
    Init {
        /// Data directory path
        #[arg(long, default_value = "/var/lib/dfs/data")]
        data_dir: PathBuf,

        /// Metadata directory path
        #[arg(long, default_value = "/var/lib/dfs/metadata")]
        meta_dir: PathBuf,

        /// Configuration file output path
        #[arg(long, default_value = "/etc/dfs/config.toml")]
        config: PathBuf,
    },

    /// Start the DFS server
    Start {
        /// Configuration file path
        #[arg(long, default_value = "/etc/dfs/config.toml")]
        config: PathBuf,
    },

    /// Show server status and statistics
    Status {
        /// Configuration file path
        #[arg(long, default_value = "/etc/dfs/config.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            data_dir,
            meta_dir,
            config,
        } => {
            init_node(data_dir, meta_dir, config)?;
        }
        Commands::Start { config } => {
            start_server(config).await?;
        }
        Commands::Status { config } => {
            show_status(config)?;
        }
    }

    Ok(())
}

/// Initialize a new DFS node
fn init_node(data_dir: PathBuf, meta_dir: PathBuf, config_path: PathBuf) -> Result<()> {
    info!("Initializing DFS node...");

    // Create directories
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(&meta_dir)?;

    // Create default configuration
    let mut config = Config::default();
    config.storage.data_dir = data_dir.clone();
    config.storage.metadata_dir = meta_dir.clone();

    // Create config directory if needed
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Save configuration
    config.to_file(&config_path)?;

    info!("✓ Created data directory: {:?}", data_dir);
    info!("✓ Created metadata directory: {:?}", meta_dir);
    info!("✓ Saved configuration to: {:?}", config_path);
    info!("");
    info!("Node initialized successfully!");
    info!("Start the server with: dfs-server start");

    Ok(())
}

/// Start the DFS server
async fn start_server(config_path: PathBuf) -> Result<()> {
    info!("Starting DFS server...");

    // Load configuration
    let config = Config::from_file(&config_path)?;

    info!("Configuration loaded from: {:?}", config_path);
    info!("  Data directory: {:?}", config.storage.data_dir);
    info!("  Metadata directory: {:?}", config.storage.metadata_dir);
    info!("  Chunk size: {} MB", config.storage.chunk_size_mb);
    info!("  Replication factor: {}", config.replication.replication_factor);
    info!("  Listen address: {}", config.node.listen_addr);

    // Initialize storage
    let storage = std::sync::Arc::new(storage::ChunkStorage::new(config.storage.data_dir.clone())?);
    info!("✓ Chunk storage initialized");

    // Initialize metadata store
    let metadata = std::sync::Arc::new(metadata::MetadataStore::new(config.storage.metadata_dir.clone())?);
    info!("✓ Metadata store initialized");

    // Load or create persistent node ID
    let node_id = config.load_or_create_node_id()?;
    info!("Node ID: {}", node_id);

    // Initialize cluster manager
    let cluster = std::sync::Arc::new(cluster::ClusterManager::new(
        node_id,
        config.node.listen_addr,
        config.cluster.heartbeat_interval_secs,
        config.cluster.failure_timeout_secs,
    ));
    info!("✓ Cluster manager initialized");

    // Create server instance
    let server = std::sync::Arc::new(server::Server::new(
        storage.clone(),
        metadata.clone(),
        config.chunk_size_bytes(),
        cluster.clone(),
        config.replication.replication_factor,
        config.storage.metadata_dir.clone(),
    ));
    info!("✓ Server instance created");

    // Start failure detector
    server.cluster().start_failure_detector().await;
    info!("✓ Failure detector started");

    // Start heartbeat sender
    server.cluster().start_heartbeat_sender().await;
    info!("✓ Heartbeat sender started");

    // Start healing manager
    let healing = std::sync::Arc::new(healing::HealingManager::new(
        storage,
        metadata,
        cluster,
        server.network_client(),
        config.replication.replication_factor,
        config.replication.healing_delay_secs,
        config.replication.scrub_interval_hours,
        config.replication.auto_heal,
    ));
    healing.clone().start().await;
    info!("✓ Healing manager started");

    // Start network server
    let mut net_server = network::NetworkServer::new(config.node.listen_addr, server.clone());
    let server_handle = tokio::spawn(async move {
        if let Err(e) = net_server.start().await {
            tracing::error!("Network server error: {}", e);
        }
    });

    info!("");
    info!("DFS server is ready!");
    info!("Listening on: {}", config.node.listen_addr);

    // Write address to port-specific file for dfs-admin auto-discovery
    let addr_file = format!("/tmp/dfs-server-{}.addr", config.node.listen_addr.port());
    if let Err(e) = std::fs::write(&addr_file, config.node.listen_addr.to_string()) {
        warn!("Failed to write address file {}: {}", addr_file, e);
    } else {
        debug!("Wrote server address to {}", addr_file);
    }

    // Try to join cluster using both seed nodes AND persisted peers
    // This ensures any node can rejoin even if the seed node is down
    let metadata_dir = std::path::PathBuf::from(&config.storage.metadata_dir);
    let local_addr = config.node.listen_addr;

    // Start with configured seed nodes
    let mut all_join_targets = config.cluster.seed_nodes.clone();

    // Load persisted peers (excluding our own address)
    match cluster::ClusterManager::load_persisted_peers(&metadata_dir).await {
        Ok(persisted_peers) => {
            // Filter out our own address before adding to join targets
            let filtered_peers: Vec<_> = persisted_peers
                .into_iter()
                .filter(|addr| *addr != local_addr)
                .collect();

            if !filtered_peers.is_empty() {
                info!("✓ Loaded {} persisted peers (excluding self)", filtered_peers.len());
                all_join_targets.extend(filtered_peers);
            }
        }
        Err(e) => debug!("Failed to load persisted peers: {}", e),
    }

    // Join cluster if we have any targets (seeds or peers)
    if !all_join_targets.is_empty() {
        info!("Attempting to join cluster via {} total nodes (seeds + peers)...", all_join_targets.len());
        tokio::time::sleep(std::time::Duration::from_millis(500)).await; // Let server start

        match join_cluster(server.clone(), &all_join_targets, &metadata_dir, local_addr).await {
            Ok(_) => info!("✓ Successfully joined cluster"),
            Err(e) => warn!("Failed to join cluster: {}", e),
        }

        // Start periodic rejoin attempts in background
        start_periodic_rejoin(server.clone(), all_join_targets.clone(), metadata_dir.clone(), local_addr).await;
    } else {
        info!("No seed nodes or peers configured - running as standalone node");
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    server_handle.abort();

    Ok(())
}

/// Attempt to join cluster via seed nodes
async fn join_cluster(
    server: std::sync::Arc<server::Server>,
    seed_nodes: &[std::net::SocketAddr],
    metadata_dir: &std::path::Path,
    local_addr: std::net::SocketAddr,
) -> Result<()> {
    use dfs_common::protocol::{ClusterMessage, Message, MessageEnvelope, RequestId};
    use tracing::warn;

    info!("Attempting to join cluster via {} seed/peer nodes", seed_nodes.len());

    // Deduplicate seed nodes and filter out our own address
    let unique_seeds: std::collections::HashSet<_> = seed_nodes
        .iter()
        .filter(|addr| **addr != local_addr)
        .cloned()
        .collect();

    if unique_seeds.is_empty() {
        anyhow::bail!("No valid join targets after filtering self");
    }

    for seed_addr in &unique_seeds {
        match send_join_request(*seed_addr, &server).await {
            Ok(cluster_nodes) => {
                info!("✓ Successfully joined cluster via {} - learned about {} total nodes",
                    seed_addr, cluster_nodes.len());

                // Save discovered peers to disk (excluding self) for future recovery
                let peer_addrs: Vec<std::net::SocketAddr> = cluster_nodes
                    .iter()
                    .map(|n| n.addr)
                    .filter(|addr| *addr != local_addr)
                    .collect();

                if !peer_addrs.is_empty() {
                    if let Err(e) = cluster::ClusterManager::save_persisted_peers(&peer_addrs, metadata_dir).await {
                        warn!("Failed to save persisted peers: {}", e);
                    }
                }

                // Announce ourselves to all peers we learned about (except the one we joined through)
                announce_to_peers(&server, &cluster_nodes, *seed_addr).await;

                return Ok(());
            }
            Err(e) => {
                debug!("Failed to join via {}: {}", seed_addr, e);
                continue;
            }
        }
    }

    anyhow::bail!("Failed to join cluster - all {} seed/peer nodes unreachable", unique_seeds.len())
}

/// Send join request to a seed node
async fn send_join_request(
    seed_addr: std::net::SocketAddr,
    server: &std::sync::Arc<server::Server>,
) -> Result<Vec<dfs_common::types::NodeInfo>> {
    use dfs_common::protocol::{ClusterMessage, Message, MessageEnvelope, RequestId};
    use dfs_common::types::NodeInfo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let local_node_id = server.cluster().local_node_id();
    let local_addr = server.cluster().local_addr();

    let node_info = NodeInfo::new(local_node_id, local_addr, None);

    // Send join request
    let request = ClusterMessage::JoinRequest {
        node_info: node_info.clone(),
    };

    // Connect to seed node
    let mut stream = TcpStream::connect(seed_addr).await?;

    // Create message envelope
    let request_id = RequestId::new(1);
    let envelope = MessageEnvelope::new(request_id, Message::Cluster(request));
    let encoded = envelope.to_bytes()?;

    // Send length prefix + message
    stream.write_u32(encoded.len() as u32).await?;
    stream.write_all(&encoded).await?;

    // Read response length
    let response_len = stream.read_u32().await?;

    // Read response
    let mut buf = vec![0u8; response_len as usize];
    stream.read_exact(&mut buf).await?;

    // Deserialize response
    let response_envelope = MessageEnvelope::from_bytes(&buf)?;

    match response_envelope.message {
        Message::Response(dfs_common::protocol::Response::Ok { data }) => {
            // Decode JoinResponse
            if let Some(data) = data {
                let join_response: ClusterMessage = bincode::deserialize(&data)?;

                if let ClusterMessage::JoinResponse {
                    accepted,
                    cluster_nodes,
                } = join_response
                {
                    if !accepted {
                        anyhow::bail!("Join request rejected by seed node");
                    }

                    info!(
                        "Join request accepted, received {} cluster nodes",
                        cluster_nodes.len()
                    );

                    // Clone cluster_nodes before consuming it in the loop
                    let cluster_nodes_clone = cluster_nodes.clone();

                    // Add all cluster nodes (except self)
                    for node in cluster_nodes {
                        if node.id != local_node_id {
                            server.cluster().add_node(node).await?;
                        }
                    }

                    Ok(cluster_nodes_clone)
                } else {
                    anyhow::bail!("Unexpected cluster message type in response")
                }
            } else {
                anyhow::bail!("No data in join response")
            }
        }
        _ => anyhow::bail!("Unexpected response type to join request"),
    }
}

/// Start background task to periodically retry joining cluster
async fn start_periodic_rejoin(
    server: std::sync::Arc<server::Server>,
    join_targets: Vec<std::net::SocketAddr>,
    metadata_dir: std::path::PathBuf,
    local_addr: std::net::SocketAddr,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // Skip first tick (immediate)

        loop {
            interval.tick().await;

            // Check if we're still isolated (only know about ourselves)
            let node_count = server.cluster().get_all_nodes().await.len();

            if node_count <= 1 {
                debug!("Isolated node detected ({} nodes), attempting to rejoin cluster...", node_count);

                match join_cluster(server.clone(), &join_targets, &metadata_dir, local_addr).await {
                    Ok(_) => {
                        info!("✓ Successfully rejoined cluster via periodic retry");
                    }
                    Err(e) => {
                        debug!("Periodic rejoin attempt failed: {}", e);
                    }
                }
            }
        }
    });
}

/// Announce our presence to all peers we learned about
async fn announce_to_peers(
    server: &std::sync::Arc<server::Server>,
    cluster_nodes: &[dfs_common::types::NodeInfo],
    joined_via: std::net::SocketAddr,
) {
    use dfs_common::protocol::{ClusterMessage, Message, MessageEnvelope, RequestId};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    let local_node_id = server.cluster().local_node_id();
    let local_addr = server.cluster().local_addr();
    let node_info = dfs_common::types::NodeInfo::new(local_node_id, local_addr, None);

    let announcement = ClusterMessage::NodeJoined {
        node_info: node_info.clone(),
    };

    // Announce to all peers except ourselves and the one we joined through
    for peer in cluster_nodes {
        if peer.id == local_node_id || peer.addr == joined_via {
            continue;
        }

        info!("Announcing to peer {}", peer.addr);

        // Spawn announcement in background - don't block on failures
        let peer_addr = peer.addr;
        let announcement_clone = announcement.clone();
        tokio::spawn(async move {
            match TcpStream::connect(peer_addr).await {
                Ok(mut stream) => {
                    let request_id = RequestId::new(1);
                    let envelope = MessageEnvelope::new(request_id, Message::Cluster(announcement_clone));

                    if let Ok(encoded) = envelope.to_bytes() {
                        let _ = stream.write_u32(encoded.len() as u32).await;
                        let _ = stream.write_all(&encoded).await;
                        debug!("Successfully announced to {}", peer_addr);
                    }
                }
                Err(e) => {
                    debug!("Failed to announce to {}: {}", peer_addr, e);
                }
            }
        });
    }
}

/// Show server status
fn show_status(config_path: PathBuf) -> Result<()> {
    let config = Config::from_file(&config_path)?;

    info!("DFS Node Status");
    info!("===============");
    info!("");
    info!("Configuration:");
    info!("  Data directory: {:?}", config.storage.data_dir);
    info!("  Metadata directory: {:?}", config.storage.metadata_dir);
    info!("  Chunk size: {} MB", config.storage.chunk_size_mb);
    info!("  Replication factor: {}", config.replication.replication_factor);
    info!("");

    // Try to load storage stats
    if let Ok(storage) = storage::ChunkStorage::new(config.storage.data_dir.clone()) {
        if let Ok(stats) = storage.get_stats() {
            info!("Storage:");
            info!("  Total chunks: {}", stats.total_chunks);
            info!(
                "  Total size: {:.2} MB",
                stats.total_bytes as f64 / (1024.0 * 1024.0)
            );
            info!("");
        }
    }

    // Try to load metadata stats
    if let Ok(metadata) = metadata::MetadataStore::new(config.storage.metadata_dir.clone()) {
        if let Ok(stats) = metadata.get_stats() {
            info!("Metadata:");
            info!("  Total files: {}", stats.file_count);
            info!(
                "  Database size: {:.2} MB",
                stats.size_on_disk as f64 / (1024.0 * 1024.0)
            );
        }
    }

    Ok(())
}
