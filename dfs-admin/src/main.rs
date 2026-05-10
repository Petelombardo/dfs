use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfs_common::{ChunkId, ChunkLocation, FileId, Message, MessageEnvelope, NodeId, Request, RequestId, Response};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{error, info, warn, Level};

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

    /// Delete queue management commands
    Delete {
        #[command(subcommand)]
        cmd: DeleteCommands,
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
    /// Rebuild path index and chunk map from file records (non-blocking, runs in background)
    Repair,
    /// Trigger immediate healing for a specific file (path or UUID)
    File {
        /// File path or UUID
        path: String,
    },
}

#[derive(Subcommand)]
enum FileCommands {
    /// Show file information with chunk locations
    Info {
        /// File path
        path: String,
    },
    /// Find which file(s) own a given chunk ID
    FindChunk {
        /// Chunk ID (hex string)
        chunk_id: String,
    },
    /// List all files in metadata database
    List,
    /// Purge file metadata from database (without deleting chunks). Accepts path or UUID.
    Purge {
        /// File path or UUID
        path: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Repack a fragmented file into 4MB chunks for better read performance
    Repack {
        /// File path
        path: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum DeleteCommands {
    /// Show pending delete queue entries across all nodes
    Queue,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Reset SIGPIPE to default so piping to `head`, `grep`, etc. terminates cleanly
    // instead of panicking with "failed printing to stdout: Broken pipe".
    #[cfg(unix)]
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }

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
        Commands::Delete { cmd } => handle_delete_command(cmd, &cluster_addrs, json_output).await?,
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
                    local_node_id,
                    ..
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "total_nodes": total_nodes,
                            "healthy_nodes": healthy_nodes,
                            "nodes": nodes.iter().map(|n| {
                                let id_str = n.id.to_string();
                                let short_id = &id_str[..8.min(id_str.len())];
                                serde_json::json!({
                                    "id": id_str,
                                    "short_id": short_id,
                                    "address": n.addr.to_string(),
                                    "status": format!("{:?}", n.status),
                                    "last_heartbeat": n.last_heartbeat,
                                })
                            }).collect::<Vec<_>>()
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        // Leader = online node with minimum NodeId (same logic as server)
                        let leader_id = nodes.iter()
                            .filter(|n| n.status == dfs_common::NodeStatus::Online)
                            .map(|n| n.id)
                            .min();

                        println!("DFS Cluster Status");
                        println!("==================");
                        println!("Total Nodes:   {}", total_nodes);
                        println!("Healthy Nodes: {}", healthy_nodes);
                        if let Some(lid) = leader_id {
                            let lid_str = lid.to_string();
                            println!("Leader:        {}", &lid_str[..8.min(lid_str.len())]);
                        }
                        println!();
                        println!("Nodes:");
                        println!("{:<10} {:<40} {:<20} {:<12} {:<8} {}", "Short ID", "ID", "Address", "Status", "Role", "Last Heartbeat");
                        println!("{}", "-".repeat(115));

                        for node in nodes {
                            let id_str = node.id.to_string();
                            let short_id = &id_str[..8.min(id_str.len())];
                            let status_str = format!("{:?}", node.status);
                            let status_display = match node.status {
                                dfs_common::NodeStatus::Online => format!("✓ {}", status_str),
                                dfs_common::NodeStatus::Suspected => format!("? {}", status_str),
                                dfs_common::NodeStatus::Failed => format!("✗ {}", status_str),
                                dfs_common::NodeStatus::Leaving => format!("← {}", status_str),
                            };
                            let role = if Some(node.id) == leader_id { "LEADER" } else { "follower" };
                            let heartbeat_str = if local_node_id == Some(node.id) {
                                "-".to_string()
                            } else {
                                let now = dfs_common::types::current_timestamp();
                                let seconds_ago = now.saturating_sub(node.last_heartbeat);
                                format!("{}s ago", seconds_ago)
                            };
                            println!(
                                "{:<10} {:<40} {:<20} {:<12} {:<8} {}",
                                short_id,
                                id_str,
                                node.addr,
                                status_display,
                                role,
                                heartbeat_str
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

/// Resolve the leader's socket address by querying GetClusterStatus.
/// Falls back to cluster_addrs[0] if the leader can't be determined.
async fn find_leader_addr(cluster_addrs: &[SocketAddr]) -> SocketAddr {
    if let Ok(Response::ClusterStatus { nodes, leader_node_id: Some(leader_id), .. }) =
        send_request(cluster_addrs[0], Request::GetClusterStatus).await
    {
        if let Some(node) = nodes.iter().find(|n| n.id == leader_id) {
            return node.addr;
        }
    }
    cluster_addrs[0]
}

async fn handle_healing_command(
    cmd: HealingCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        HealingCommands::Status => {
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::GetHealingStatus).await?;

            match response {
                Response::HealingStatus {
                    enabled,
                    pending_count,
                    in_flight_count,
                    stalled_count,
                    last_check,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "enabled": enabled,
                            "pending_count": pending_count,
                            "in_flight_count": in_flight_count,
                            "stalled_count": stalled_count,
                            "last_check": last_check,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Healing Status (leader: {})", leader);
                        println!("==================");
                        println!("Enabled:       {}", if enabled { "Yes" } else { "No" });
                        println!("Pending:       {}", pending_count);
                        println!("In-flight:     {}", in_flight_count);
                        println!("Stalled:       {}", stalled_count);
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
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::EnableHealing).await?;

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
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::DisableHealing).await?;

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
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::TriggerHealing).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Healing triggered on leader ({})", leader);
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
        HealingCommands::Repair => {
            // Broadcast to all nodes — each repairs its own metadata independently
            for &addr in cluster_addrs {
                let response = send_request(addr, Request::TriggerMetadataRepair).await?;
                match response {
                    Response::Ok { .. } => {
                        println!("{}: metadata repair started in background", addr);
                    }
                    Response::Error { message, .. } => {
                        eprintln!("{}: error — {}", addr, message);
                    }
                    _ => {}
                }
            }
        }
        HealingCommands::File { path } => {
            // Send to leader only — healing is leader-coordinated
            let addr = cluster_addrs[0];
            let response = send_request(addr, Request::HealFile { path: path.clone() }).await?;
            match response {
                Response::Ok { data } => {
                    let msg = data.and_then(|d| String::from_utf8(d).ok())
                        .unwrap_or_else(|| "healing queued".to_string());
                    println!("{}", msg);
                }
                Response::Error { message, .. } => {
                    eprintln!("Error: {}", message);
                }
                _ => {}
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
            // Detect whether the argument is a UUID (file ID) or a file path
            let request = if let Ok(uuid) = uuid::Uuid::parse_str(&path) {
                let file_id = dfs_common::FileId::from_uuid(uuid);
                Request::GetFileInfoById { file_id }
            } else {
                Request::GetFileInfo { path: path.clone() }
            };

            let response = send_request(cluster_addrs[0], request).await?;

            match response {
                Response::FileInfo {
                    metadata,
                    chunk_locations,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "path": metadata.path,
                            "size": metadata.size,
                            "chunks": metadata.chunk_locations.len(),
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
                        println!("File Information: {}", metadata.path);
                        println!("==================");
                        println!("Path:       {}", metadata.path);
                        println!("Size:       {} bytes", metadata.size);
                        println!("Chunks:     {}", metadata.chunk_locations.len());
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
        FileCommands::FindChunk { chunk_id } => {
            let response = send_request(cluster_addrs[0], Request::ListAllFiles).await?;
            let files = match response {
                Response::FileList { files, .. } => files,
                _ => anyhow::bail!("Unexpected response"),
            };

            let needle = chunk_id.to_lowercase();
            let mut found = false;
            for file in &files {
                for (idx, loc) in file.chunk_locations.iter().enumerate() {
                    if loc.chunk_id.to_string().to_lowercase().contains(&needle) {
                        found = true;
                        println!("File:    {}", file.path);
                        println!("File ID: {}", file.id);
                        println!("Chunk:   index={} offset={} nodes={:?}",
                            idx,
                            idx as u64 * 4 * 1024 * 1024,
                            loc.nodes);
                        println!();
                    }
                }
            }
            if !found {
                println!("Chunk {} not found in any file's metadata.", chunk_id);
            }
        }
        FileCommands::List => {
            let response = send_request(cluster_addrs[0], Request::ListAllFiles).await?;

            match response {
                Response::FileList { files, total_count } => {
                    if json_output {
                        let output = serde_json::json!({
                            "total_count": total_count,
                            "files": files.iter().map(|f| {
                                serde_json::json!({
                                    "id": f.id.to_string(),
                                    "path": f.path,
                                    "size": f.size,
                                    "chunks": f.chunk_locations.len(),
                                    "created": f.created_at,
                                    "modified": f.modified_at,
                                })
                            }).collect::<Vec<_>>()
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("All Files in Metadata Database");
                        println!("==============================");
                        println!("Total Files: {}", total_count);
                        println!();
                        println!("{:<38} {:<80} {:<12} {:<8} {}", "File ID", "Path", "Size", "Chunks", "Modified");
                        println!("{}", "-".repeat(180));

                        for file in files {
                            let size_str = format_size(file.size);
                            println!("{:<38} {:<80} {:<12} {:<8} {}",
                                file.id.to_string(),
                                truncate_path(&file.path, 80),
                                size_str,
                                file.chunk_locations.len(),
                                file.modified_at
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
        FileCommands::Purge { path, yes } => {
            if !yes {
                print!("Are you sure you want to purge metadata for '{}'? This will NOT delete chunks. [y/N]: ", path);
                std::io::Write::flush(&mut std::io::stdout())?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    println!("Cancelled.");
                    return Ok(());
                }
            }

            // Detect UUID vs path and send the appropriate request
            let request = if let Ok(uuid) = uuid::Uuid::parse_str(&path) {
                let file_id = dfs_common::FileId::from_uuid(uuid);
                Request::PurgeFileMetadataById { file_id, propagate: true }
            } else {
                Request::PurgeFileMetadata { path: path.clone() }
            };

            let response = send_request(cluster_addrs[0], request).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Successfully purged metadata for: {}", path);
                    println!();
                    println!("Note: Chunks are still stored on disk.");
                    println!("Run 'dfs-admin healing trigger' to clean up orphaned chunks.");
                }
                Response::Error { message, code } => {
                    error!("Error: {}", message);
                    if code == dfs_common::ErrorCode::NotFound {
                        anyhow::bail!("File not found: {}", path);
                    } else {
                        anyhow::bail!("Command failed: {}", message);
                    }
                }
                _ => anyhow::bail!("Unexpected response type"),
            }
        }

        FileCommands::Repack { path, yes } => {
            handle_repack(path, yes, cluster_addrs).await?;
        }
    }

    Ok(())
}

async fn handle_repack(path: String, yes: bool, cluster_addrs: &[SocketAddr]) -> Result<()> {
    const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB target chunk size

    // 1. Fetch current metadata
    let response = send_request(
        cluster_addrs[0],
        Request::GetFileInfo { path: path.clone() },
    ).await?;

    let (metadata, existing_locations) = match response {
        Response::FileInfo { metadata, chunk_locations } => (metadata, chunk_locations),
        Response::Error { message, code } => {
            if code == dfs_common::ErrorCode::NotFound {
                anyhow::bail!("File not found: {}", path);
            }
            anyhow::bail!("Failed to get file info: {}", message);
        }
        _ => anyhow::bail!("Unexpected response"),
    };

    let old_chunk_count = metadata.chunk_locations.len();
    let file_size = metadata.size;

    // Skip if already well-packed (average chunk size >= 2MB)
    let avg_chunk_size = if old_chunk_count > 0 { file_size / old_chunk_count as u64 } else { 0 };
    if avg_chunk_size >= 2 * 1024 * 1024 {
        println!("File '{}' is already well-packed ({} chunks, avg {:.1} MB each). No repack needed.",
                 path, old_chunk_count, avg_chunk_size as f64 / (1024.0 * 1024.0));
        return Ok(());
    }

    println!("File: {}", path);
    println!("Size: {} ({:.1} MB)", file_size, file_size as f64 / (1024.0 * 1024.0));
    println!("Current chunks: {} (avg {:.1} KB each)", old_chunk_count, avg_chunk_size as f64 / 1024.0);
    println!("Target chunks:  ~{}", (file_size as usize + CHUNK_SIZE - 1) / CHUNK_SIZE);
    println!();

    if !yes {
        print!("Repack '{}' into 4MB chunks? This rewrites the file on the cluster. [y/N]: ", path);
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Build chunk_id -> node list mapping from existing locations (for smart reads)
    let mut chunk_node_map: HashMap<ChunkId, Vec<SocketAddr>> = HashMap::new();
    // Build node_id -> addr mapping from cluster status
    let mut node_id_to_addr: HashMap<String, SocketAddr> = HashMap::new();

    if let Ok(Response::ClusterStatus { nodes, .. }) = send_request(cluster_addrs[0], Request::GetClusterStatus).await {
        for node in &nodes {
            let id_str = node.id.to_string();
            node_id_to_addr.insert(id_str, node.addr);
        }
    }

    for loc in &existing_locations {
        let addrs: Vec<SocketAddr> = loc.nodes.iter()
            .filter_map(|nid| node_id_to_addr.get(&nid.to_string()).copied())
            .collect();
        if !addrs.is_empty() {
            chunk_node_map.insert(loc.chunk_id, addrs);
        }
    }

    // Choose 2 nodes for writing new chunks (round-robin from cluster)
    let all_nodes: Vec<SocketAddr> = if let Ok(Response::ClusterStatus { nodes, .. }) =
        send_request(cluster_addrs[0], Request::GetClusterStatus).await
    {
        nodes.iter()
            .filter(|n| n.status == dfs_common::NodeStatus::Online)
            .map(|n| n.addr)
            .collect()
    } else {
        cluster_addrs.to_vec()
    };

    if all_nodes.len() < 2 {
        anyhow::bail!("Need at least 2 online nodes for repack");
    }

    println!("Reading and repacking {} chunks...", old_chunk_count);

    let mut new_chunk_locations: Vec<ChunkLocation> = Vec::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK_SIZE);
    let mut file_offset: u64 = 0;
    let mut chunks_read = 0usize;
    let mut write_node_idx = 0usize;

    // Helper closure to write a buffer to 2 nodes and return ChunkLocations
    // We'll do this inline in the loop below

    for (i, loc) in metadata.chunk_locations.iter().enumerate() {
        let chunk_id = &loc.chunk_id;
        // Pick the best node to read from
        let read_addr = chunk_node_map.get(chunk_id)
            .and_then(|addrs| addrs.first().copied())
            .unwrap_or(cluster_addrs[0]);

        let chunk_data = match send_request(read_addr, Request::ReadChunk {
            chunk_id: *chunk_id,
            sequential_hint: Some((i as u64, old_chunk_count as u64)),
            client_write_seq: None,
        }).await? {
            Response::ChunkData { data, .. } => data,
            Response::Error { message, .. } => {
                // Try fallback nodes
                let mut data_opt = None;
                for &node in cluster_addrs {
                    if node == read_addr { continue; }
                    if let Ok(Response::ChunkData { data, .. }) = send_request(node, Request::ReadChunk {
                        chunk_id: *chunk_id,
                        sequential_hint: None,
                        client_write_seq: None,
                    }).await {
                        data_opt = Some(data);
                        break;
                    }
                }
                match data_opt {
                    Some(d) => d,
                    None => anyhow::bail!("Failed to read chunk {} (idx {}): {}", chunk_id, i, message),
                }
            }
            _ => anyhow::bail!("Unexpected response reading chunk {}", chunk_id),
        };

        buffer.extend_from_slice(&chunk_data);
        chunks_read += 1;

        // Flush when buffer reaches 4MB or this is the last chunk
        let is_last = i + 1 == old_chunk_count;
        while buffer.len() >= CHUNK_SIZE || (is_last && !buffer.is_empty()) {
            let flush_size = buffer.len().min(CHUNK_SIZE);
            let flush_data: Vec<u8> = buffer.drain(..flush_size).collect();

            // Write to 2 nodes
            let node1 = all_nodes[write_node_idx % all_nodes.len()];
            let node2 = all_nodes[(write_node_idx + 1) % all_nodes.len()];
            write_node_idx += 2;

            let (ids1, sizes1, addr1) = match send_request(node1, Request::WriteFileLocalOnly {
                data: flush_data.clone(),
                file_offset: 0,
            }).await? {
                Response::ChunkIds { chunk_ids, chunk_sizes, .. } => (chunk_ids, chunk_sizes, node1),
                Response::Error { message, .. } => anyhow::bail!("Write to {} failed: {}", node1, message),
                _ => anyhow::bail!("Unexpected write response from {}", node1),
            };

            let (ids2, _sizes2, addr2) = match send_request(node2, Request::WriteFileLocalOnly {
                data: flush_data,
                file_offset: 0,
            }).await? {
                Response::ChunkIds { chunk_ids, chunk_sizes, .. } => (chunk_ids, chunk_sizes, node2),
                Response::Error { message, .. } => anyhow::bail!("Write to {} failed: {}", node2, message),
                _ => anyhow::bail!("Unexpected write response from {}", node2),
            };

            // Build ChunkLocation for each new chunk
            // Get node IDs from the addr->node mapping
            let node_id_map: HashMap<SocketAddr, NodeId> = node_id_to_addr.iter()
                .filter_map(|(id_str, &addr)| {
                    uuid::Uuid::parse_str(id_str).ok()
                        .map(|uuid| (addr, NodeId::from_uuid(uuid)))
                })
                .collect();

            let node1_id = node_id_map.get(&addr1)
                .copied()
                .unwrap_or_else(|| NodeId::from_uuid(uuid::Uuid::new_v4()));
            let node2_id = node_id_map.get(&addr2)
                .copied()
                .unwrap_or_else(|| NodeId::from_uuid(uuid::Uuid::new_v4()));

            let mut current_offset = file_offset;
            for (chunk_id, &chunk_size) in ids1.iter().zip(sizes1.iter()) {
                new_chunk_locations.push(ChunkLocation {
                    chunk_id: *chunk_id,
                    nodes: vec![node1_id, node2_id],
                    size: chunk_size as usize,
                    checksum: chunk_id.hash,
                    file_offset: Some(current_offset),
                    written_at: None,
                });
                current_offset += chunk_size;
            }
            file_offset = current_offset;

            if !buffer.is_empty() && buffer.len() < CHUNK_SIZE && !is_last {
                // Remaining data < 4MB and more chunks to come; keep accumulating
                break;
            }
        }

        if chunks_read % 500 == 0 {
            println!("  Progress: {}/{} chunks read, {} new chunks written so far",
                     chunks_read, old_chunk_count, new_chunk_locations.len());
        }
    }

    let new_chunk_count = new_chunk_locations.len();
    println!("Repack complete: {} chunks → {} chunks", old_chunk_count, new_chunk_count);

    // Build updated metadata
    let mut new_metadata = metadata.clone();
    new_metadata.chunk_locations = new_chunk_locations;
    new_metadata.modified_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Write updated metadata to all nodes
    println!("Updating metadata on {} nodes...", all_nodes.len());
    let mut metadata_ok = 0;
    for &node in &all_nodes {
        match send_request(node, Request::PutFileMetadata { metadata: new_metadata.clone() }).await {
            Ok(Response::Ok { .. }) => { metadata_ok += 1; }
            Ok(Response::Error { message, .. }) => { warn!("Metadata update failed on {}: {}", node, message); }
            Err(e) => { warn!("Failed to reach {} for metadata update: {}", node, e); }
            _ => {}
        }
    }

    if metadata_ok == 0 {
        anyhow::bail!("Failed to update metadata on any node — repack data written but metadata not updated!");
    }

    println!("Metadata updated on {}/{} nodes.", metadata_ok, all_nodes.len());
    println!("Done. Run 'dfs-admin healing trigger' to clean up the old {} chunks.", old_chunk_count);

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

fn parse_file_id(s: &str) -> Result<FileId> {
    // Parse UUID from string (supports both hyphenated and non-hyphenated formats)
    let uuid = uuid::Uuid::parse_str(s)
        .context("Invalid file ID format - expected UUID")?;
    Ok(FileId(uuid))
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

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", size, UNITS[unit_index])
    }
}

async fn handle_delete_command(
    cmd: DeleteCommands,
    cluster_addrs: &[SocketAddr],
    json_output: bool,
) -> Result<()> {
    match cmd {
        DeleteCommands::Queue => {
            // Poll every node for its pending delete queue and display the union.
            // Entries may appear on multiple nodes (quorum write); we dedup by file_id.
            let mut seen: std::collections::HashMap<dfs_common::FileId, (String, usize, Vec<SocketAddr>)> = std::collections::HashMap::new();

            for &addr in cluster_addrs {
                match send_request(addr, Request::GetDeleteQueue).await {
                    Ok(Response::DeleteQueue { entries }) => {
                        for entry in entries {
                            seen.entry(entry.file_id)
                                .and_modify(|e| e.2.push(addr))
                                .or_insert((entry.path, entry.chunk_ids.len(), vec![addr]));
                        }
                    }
                    Ok(_) => eprintln!("Unexpected response from {}", addr),
                    Err(e) => eprintln!("Failed to query {}: {}", addr, e),
                }
            }

            if seen.is_empty() {
                if json_output {
                    println!("[]");
                } else {
                    println!("Delete queue is empty.");
                }
                return Ok(());
            }

            if json_output {
                let entries: Vec<_> = seen.values().map(|(path, chunks, nodes)| {
                    serde_json::json!({
                        "path": path,
                        "chunks": chunks,
                        "queued_on_nodes": nodes.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                println!("{} pending deletion(s):", seen.len());
                println!("{:<6} {:<10} {}", "Chunks", "Nodes", "Path");
                println!("{}", "-".repeat(70));
                let mut entries: Vec<_> = seen.values().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, chunks, nodes) in entries {
                    println!("{:<6} {:<10} {}", chunks, nodes.len(), path);
                }
            }
        }
    }
    Ok(())
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let start = &path[..max_len/2 - 2];
        let end = &path[path.len() - (max_len/2 - 1)..];
        format!("{}...{}", start, end)
    }
}
