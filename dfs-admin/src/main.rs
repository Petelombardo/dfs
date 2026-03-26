use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfs_common::{ChunkId, Message, MessageEnvelope, Request, RequestId, Response};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{error, Level};

#[derive(Parser)]
#[command(name = "dfs-admin")]
#[command(about = "DFS cluster administration tool", long_about = None)]
struct Cli {
    /// Cluster nodes (comma-separated, e.g., 192.168.1.10:8900,192.168.1.11:8900)
    /// If not specified, will attempt to auto-detect local server
    #[arg(short, long, value_delimiter = ',')]
    cluster: Vec<String>,

    /// Output format (text or json)
    #[arg(long, default_value = "text")]
    format: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Cluster management commands
    Cluster {
        #[command(subcommand)]
        cmd: ClusterCommands,
    },

    /// Storage management commands
    Storage {
        #[command(subcommand)]
        cmd: StorageCommands,
    },

    /// Healing management commands
    Healing {
        #[command(subcommand)]
        cmd: HealingCommands,
    },

    /// File inspection commands
    File {
        #[command(subcommand)]
        cmd: FileCommands,
    },
}

#[derive(Subcommand)]
enum ClusterCommands {
    /// Show cluster status
    Status,
    /// Remove a node from the cluster
    RemoveNode {
        /// Node ID to remove
        node_id: String,
    },
}

#[derive(Subcommand)]
enum StorageCommands {
    /// Show storage statistics
    Stats,
    /// Trigger manual scrub
    Scrub,
}

#[derive(Subcommand)]
enum HealingCommands {
    /// Show healing status
    Status,
    /// Enable auto-healing
    Enable,
    /// Disable auto-healing
    Disable,
    /// Trigger immediate healing check
    Trigger,
}

#[derive(Subcommand)]
enum FileCommands {
    /// Show file information with chunk locations
    Info {
        /// File path
        path: String,
    },
    /// Show chunk replica locations
    Replicas {
        /// Chunk ID (hex string)
        chunk_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .init();

    let cli = Cli::parse();

    // Parse cluster addresses
    let cluster_addrs = if cli.cluster.is_empty() {
        // Try to auto-detect local server
        detect_local_servers()?
    } else {
        parse_cluster_addrs(&cli.cluster)?
    };

    if cluster_addrs.is_empty() {
        anyhow::bail!("No valid cluster addresses provided");
    }

    let json_output = cli.format == "json";

    // Execute command
    match cli.command {
        Commands::Cluster { cmd } => {
            handle_cluster_command(cmd, &cluster_addrs, json_output).await?
        }
        Commands::Storage { cmd } => {
            handle_storage_command(cmd, &cluster_addrs, json_output).await?
        }
        Commands::Healing { cmd } => {
            handle_healing_command(cmd, &cluster_addrs, json_output).await?
        }
        Commands::File { cmd } => handle_file_command(cmd, &cluster_addrs, json_output).await?,
    }

    Ok(())
}

fn parse_cluster_addrs(addrs: &[String]) -> Result<Vec<SocketAddr>> {
    let mut result = Vec::new();
    for addr_str in addrs {
        let addr: SocketAddr = addr_str
            .parse()
            .with_context(|| format!("Invalid address: {}", addr_str))?;
        result.push(addr);
    }
    Ok(result)
}

fn detect_local_servers() -> Result<Vec<SocketAddr>> {
    // Scan for /tmp/dfs-server-*.addr files
    let pattern = "/tmp/dfs-server-*.addr";
    let mut servers = Vec::new();

    // Use glob to find matching files
    for entry in std::fs::read_dir("/tmp")? {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if file_name.starts_with("dfs-server-") && file_name.ends_with(".addr") {
            // Read the address from the file
            match std::fs::read_to_string(&path) {
                Ok(addr_str) => match addr_str.trim().parse::<SocketAddr>() {
                    Ok(addr) => servers.push(addr),
                    Err(e) => {
                        tracing::warn!("Invalid address in {}: {}", path.display(), e);
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to read {}: {}", path.display(), e);
                }
            }
        }
    }

    match servers.len() {
        0 => {
            anyhow::bail!(
                "No local dfs-server detected. Either:\n\
                 1. Start a dfs-server instance, or\n\
                 2. Specify --cluster <address> to connect to a remote server"
            );
        }
        1 => {
            println!("Auto-detected local server at {}", servers[0]);
            Ok(servers)
        }
        _ => {
            anyhow::bail!(
                "Multiple dfs-server instances detected:\n{}\n\n\
                 Please specify which server to connect to using --cluster <address>",
                servers
                    .iter()
                    .map(|s| format!("  - {}", s))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

async fn handle_cluster_command(
    cmd: ClusterCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        ClusterCommands::RemoveNode { node_id } => {
            // Parse node ID from UUID string
            let uuid = uuid::Uuid::parse_str(&node_id)
                .with_context(|| format!("Invalid node ID (must be a UUID): {}", node_id))?;
            let node_id_parsed = dfs_common::NodeId::from_uuid(uuid);

            let response = send_request(
                cluster_addrs[0],
                Request::RemoveNode {
                    node_id: node_id_parsed,
                },
            )
            .await?;

            match response {
                Response::Ok { .. } => {
                    if json_output {
                        println!("{{\"success\": true, \"message\": \"Node removed successfully\"}}");
                    } else {
                        println!("Node {} removed successfully", node_id);
                    }
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        ClusterCommands::Status => {
            let response = send_request(cluster_addrs[0], Request::GetClusterStatus).await?;

            match response {
                Response::ClusterStatus {
                    nodes,
                    total_nodes,
                    healthy_nodes,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "total_nodes": total_nodes,
                            "healthy_nodes": healthy_nodes,
                            "nodes": nodes.iter().map(|n| {
                                serde_json::json!({
                                    "id": n.id.to_string(),
                                    "address": n.addr.to_string(),
                                    "status": format!("{:?}", n.status),
                                    "last_heartbeat": n.last_heartbeat,
                                })
                            }).collect::<Vec<_>>()
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Cluster Status");
                        println!("==================");
                        println!("Total Nodes:   {}", total_nodes);
                        println!("Healthy Nodes: {}", healthy_nodes);
                        println!();
                        println!("Nodes:");
                        println!("{:<40} {:<20} {:<12} {}", "ID", "Address", "Status", "Last Heartbeat");
                        println!("{}", "-".repeat(95));

                        for node in nodes {
                            let status_str = format!("{:?}", node.status);
                            let status_display = match node.status {
                                dfs_common::NodeStatus::Online => format!("✓ {}", status_str),
                                dfs_common::NodeStatus::Suspected => format!("? {}", status_str),
                                dfs_common::NodeStatus::Failed => format!("✗ {}", status_str),
                                dfs_common::NodeStatus::Leaving => format!("← {}", status_str),
                            };
                            println!(
                                "{:<40} {:<20} {:<12} {}s ago",
                                node.id.to_string(),
                                node.addr,
                                status_display,
                                node.last_heartbeat
                            );
                        }
                    }
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
    }

    Ok(())
}

async fn handle_storage_command(
    cmd: StorageCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        StorageCommands::Stats => {
            let response = send_request(cluster_addrs[0], Request::GetStorageStats).await?;

            match response {
                Response::StorageStats {
                    total_chunks,
                    total_size,
                    replication_factor,
                    nodes_count,
                    total_space,
                    free_space,
                    available_space,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "total_chunks": total_chunks,
                            "total_size": total_size,
                            "total_size_mb": total_size / (1024 * 1024),
                            "replication_factor": replication_factor,
                            "nodes_count": nodes_count,
                            "total_space_gb": total_space / (1024 * 1024 * 1024),
                            "free_space_gb": free_space / (1024 * 1024 * 1024),
                            "available_space_gb": available_space / (1024 * 1024 * 1024),
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Storage Statistics");
                        println!("======================");
                        println!("Total Chunks:       {}", total_chunks);
                        println!("Total Size:         {} MB", total_size / (1024 * 1024));
                        println!("Replication Factor: {}", replication_factor);
                        println!("Nodes Count:        {}", nodes_count);
                        println!("Total Space:        {} GB", total_space / (1024 * 1024 * 1024));
                        println!("Free Space:         {} GB", free_space / (1024 * 1024 * 1024));
                        println!("Available Space:    {} GB", available_space / (1024 * 1024 * 1024));
                    }
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        StorageCommands::Scrub => {
            let response = send_request(cluster_addrs[0], Request::TriggerScrub).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Scrub triggered successfully");
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
    }

    Ok(())
}

async fn handle_healing_command(
    cmd: HealingCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        HealingCommands::Status => {
            let response = send_request(cluster_addrs[0], Request::GetHealingStatus).await?;

            match response {
                Response::HealingStatus {
                    enabled,
                    pending_count,
                    last_check,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "enabled": enabled,
                            "pending_count": pending_count,
                            "last_check": last_check,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Healing Status");
                        println!("==================");
                        println!("Enabled:       {}", if enabled { "Yes" } else { "No" });
                        println!("Pending Count: {}", pending_count);
                        println!("Last Check:    {} seconds ago", last_check);
                    }
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        HealingCommands::Enable => {
            let response = send_request(cluster_addrs[0], Request::EnableHealing).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Healing enabled successfully");
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        HealingCommands::Disable => {
            let response = send_request(cluster_addrs[0], Request::DisableHealing).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Healing disabled successfully");
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        HealingCommands::Trigger => {
            let response = send_request(cluster_addrs[0], Request::TriggerHealing).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Healing triggered successfully");
                }
                Response::Error { message, .. } => {
                    error!("Error: {}", message);
                    anyhow::bail!("Command failed: {}", message);
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
    }

    Ok(())
}

async fn handle_file_command(
    cmd: FileCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        FileCommands::Info { path } => {
            let response = send_request(cluster_addrs[0], Request::GetFileInfo { path: path.clone() }).await?;

            match response {
                Response::FileInfo {
                    metadata,
                    chunk_locations,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "path": metadata.path,
                            "size": metadata.size,
                            "chunks": metadata.chunks.len(),
                            "created_at": metadata.created_at,
                            "modified_at": metadata.modified_at,
                            "mode": format!("{:o}", metadata.mode),
                            "uid": metadata.uid,
                            "gid": metadata.gid,
                            "type": format!("{:?}", metadata.file_type),
                            "chunk_locations": chunk_locations.iter().map(|loc| {
                                serde_json::json!({
                                    "chunk_id": loc.chunk_id.to_string(),
                                    "size": loc.size,
                                    "nodes": loc.nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>(),
                                })
                            }).collect::<Vec<_>>()
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("File Information: {}", path);
                        println!("==================");
                        println!("Path:       {}", metadata.path);
                        println!("Size:       {} bytes", metadata.size);
                        println!("Chunks:     {}", metadata.chunks.len());
                        println!("Created:    {}", metadata.created_at);
                        println!("Modified:   {}", metadata.modified_at);
                        println!("Mode:       {:o}", metadata.mode);
                        println!("UID:        {}", metadata.uid);
                        println!("GID:        {}", metadata.gid);
                        println!("Type:       {:?}", metadata.file_type);
                        println!();
                        println!("Chunk Locations:");
                        println!("{:<20} {:<10} {}", "Chunk ID", "Size", "Nodes");
                        println!("{}", "-".repeat(70));

                        for loc in chunk_locations {
                            let chunk_id_str = loc.chunk_id.to_string();
                            let chunk_id_short = &chunk_id_str[..16.min(chunk_id_str.len())];
                            let nodes_str = loc
                                .nodes
                                .iter()
                                .map(|n| {
                                    let s = n.to_string();
                                    s[..8.min(s.len())].to_string()
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("{:<20} {:<10} {}", chunk_id_short, loc.size, nodes_str);
                        }
                    }
                }
                Response::Error { message, code } => {
                    error!("Error: {}", message);
                    if code == dfs_common::ErrorCode::NotFound {
                        anyhow::bail!("File not found: {}", path);
                    } else {
                        anyhow::bail!("Command failed: {}", message);
                    }
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
        FileCommands::Replicas { chunk_id } => {
            // Parse chunk ID from hex string
            let chunk_id_parsed = parse_chunk_id(&chunk_id)?;

            let response = send_request(
                cluster_addrs[0],
                Request::GetChunkReplicas {
                    chunk_id: chunk_id_parsed,
                },
            )
            .await?;

            match response {
                Response::ChunkReplicas { chunk_id, nodes } => {
                    if json_output {
                        let output = serde_json::json!({
                            "chunk_id": chunk_id.to_string(),
                            "replicas": nodes.len(),
                            "nodes": nodes.iter().map(|n| n.to_string()).collect::<Vec<_>>()
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("Chunk Replicas");
                        println!("==============");
                        println!("Chunk ID: {}", chunk_id);
                        println!("Replicas: {}", nodes.len());
                        println!();
                        println!("Stored on nodes:");
                        for node in nodes {
                            println!("  - {}", node);
                        }
                    }
                }
                Response::Error { message, code } => {
                    error!("Error: {}", message);
                    if code == dfs_common::ErrorCode::NotFound {
                        anyhow::bail!("Chunk not found: {}", chunk_id);
                    } else {
                        anyhow::bail!("Command failed: {}", message);
                    }
                }
                _ => {
                    anyhow::bail!("Unexpected response type");
                }
            }
        }
    }

    Ok(())
}

fn parse_chunk_id(s: &str) -> Result<ChunkId> {
    // For simplicity, we'll just create a chunk ID from the first 32 chars
    // In a real implementation, you'd parse the hex properly
    let mut hash = [0u8; 32];
    if s.len() >= 64 {
        for i in 0..32 {
            let byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)?;
            hash[i] = byte;
        }
    }
    Ok(ChunkId::from_hash(hash))
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

async fn send_request(addr: SocketAddr, request: Request) -> Result<Response> {
    // Connect to node
    let mut stream = TcpStream::connect(addr)
        .await
        .context("Failed to connect to cluster node")?;

    // Create envelope with request ID
    let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
    let envelope = MessageEnvelope::new(request_id, Message::Request(request));
    let encoded = envelope.to_bytes().context("Failed to serialize message")?;

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

    // Read response length
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("Failed to read response length")?;
    let len = u32::from_be_bytes(len_buf) as usize;

    // Read response
    let mut buf = vec![0u8; len];
    stream
        .read_exact(&mut buf)
        .await
        .context("Failed to read response")?;

    // Deserialize response envelope
    let response_envelope = MessageEnvelope::from_bytes(&buf)
        .context("Failed to deserialize response")?;

    match response_envelope.message {
        Message::Response(response) => Ok(response),
        _ => anyhow::bail!("Expected Response message"),
    }
}
