use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfs_common::{ChunkId, Message, Request, Response};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{error, Level};

#[derive(Parser)]
#[command(name = "dfs-admin")]
#[command(about = "DFS cluster administration tool", long_about = None)]
struct Cli {
    /// Cluster nodes (comma-separated, e.g., 192.168.1.10:8900,192.168.1.11:8900)
    #[arg(short, long, value_delimiter = ',', required = true)]
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
    let cluster_addrs = parse_cluster_addrs(&cli.cluster)?;
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

async fn handle_cluster_command(
    cmd: ClusterCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
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
                        println!("{:<40} {:<20}", "ID", "Address");
                        println!("{}", "-".repeat(60));

                        for node in nodes {
                            println!("{:<40} {:<20}", node.id.to_string(), node.addr);
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
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "total_chunks": total_chunks,
                            "total_size": total_size,
                            "total_size_mb": total_size / (1024 * 1024),
                            "replication_factor": replication_factor,
                            "nodes_count": nodes_count,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Storage Statistics");
                        println!("======================");
                        println!("Total Chunks:       {}", total_chunks);
                        println!("Total Size:         {} MB", total_size / (1024 * 1024));
                        println!("Replication Factor: {}", replication_factor);
                        println!("Nodes Count:        {}", nodes_count);
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

async fn send_request(addr: SocketAddr, request: Request) -> Result<Response> {
    // Connect to node
    let mut stream = TcpStream::connect(addr)
        .await
        .context("Failed to connect to cluster node")?;

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

    // Deserialize response
    let response_message: Message =
        bincode::deserialize(&buf).context("Failed to deserialize response")?;

    match response_message {
        Message::Response(response) => Ok(response),
        _ => anyhow::bail!("Expected Response message"),
    }
}
