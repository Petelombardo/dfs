use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfs_common::{ChunkId, ChunkLocation, FileId, Message, MessageEnvelope, NodeId, Request, RequestId, Response};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{error, warn, Level};

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

    /// Show per-node ops/sec statistics (reads, writes, metadata)
    Stats {
        /// Refresh display every second (like watch)
        #[arg(long)]
        watch: bool,
    },

    /// Show per-node RPC counts by class (peer healing/delete/fold/gossip/
    /// other, client full-patch/multi-patch/fold/other, admin), plus local
    /// chunk-delete counts by reason. Cumulative since process start,
    /// in-memory only, not durable.
    RpcStats {
        /// Refresh display every second (like watch)
        #[arg(long)]
        watch: bool,
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
    /// Live-update cluster-wide settings (currently: replication factor). Fans out to
    /// every reachable node and persists to config.toml on each. A node that's
    /// unreachable during the change reconciles to the leader's value on rejoin.
    Set {
        /// New replication factor (number of copies to maintain per chunk)
        #[arg(long)]
        replication_factor: usize,
    },
    /// Show current cluster-wide settings (currently: replication factor)
    Get,
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
    /// Live-update one or more healing tuning knobs cluster-wide. Omitted flags are
    /// left unchanged. Applied immediately (no restart) and persisted to config.toml
    /// on every node, so a restart doesn't revert it — a stale env var left in a
    /// systemd unit can never override this once set.
    Set {
        /// Assumed node-to-node link bandwidth in MB/s (the adaptive controller's 100% mark)
        #[arg(long)]
        link_bandwidth_mb: Option<usize>,
        /// Max percentage of link bandwidth the healer may use, 10-100
        #[arg(long)]
        max_pct: Option<f64>,
        /// Max concurrent outstanding heal transfers
        #[arg(long)]
        max_concurrent: Option<usize>,
        /// Max concurrent heal transfers any single node may be party to (as source or
        /// target, combined). Clamped to never exceed --max-concurrent.
        #[arg(long)]
        max_concurrent_per_node: Option<usize>,
        /// Per-transfer timeout in seconds
        #[arg(long)]
        transfer_timeout_secs: Option<u64>,
        /// Delay (seconds) before a "never fully replicated" chunk becomes eligible
        /// for healing — see HealingManager::should_heal. Production default is 300s;
        /// lower this for tests that need faster convergence.
        #[arg(long)]
        healing_delay_secs: Option<u64>,
    },
    /// Show current healing tuning values (bandwidth ceiling, concurrency, timeout)
    Get,
    /// Trigger immediate healing check
    Trigger,
    /// Rebuild path index and chunk map from file records (non-blocking, runs in background)
    Repair,
    /// Trigger immediate healing for a specific file (path or UUID)
    File {
        /// File path or UUID
        path: String,
    },
    /// Trigger an immediate orphan-reconciliation sweep on every node (not leader-only —
    /// each node reconciles its own disk against its own metadata). Safety gating
    /// (age grace, two-pass confirmation, leader cross-check or all-nodes-stability)
    /// still applies; this only skips the wait between scheduled cycles.
    Cleanup,
    /// Trigger an immediate phantom-replica reconciliation pass (leader-only):
    /// verifies actual presence on every listed node for every live chunk and
    /// prunes confirmed-absent ones, queuing under-RF results for immediate
    /// healing. Runs automatically every 10 minutes; this skips the wait.
    Reconcile,
    /// Diagnostic: list the oldest entries in the leader's pending_healing
    /// queue, with age and status — for figuring out WHY something is stuck
    /// (no confirmed source yet? already at replication factor but never
    /// cleared? genuinely mid-transfer?) when the aggregate counts in
    /// `status` aren't enough on their own.
    Pending {
        /// Max entries to show, oldest first
        #[arg(long, default_value = "50")]
        limit: usize,
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
    /// Debug: show CHUNK_TABLE's raw stored record for a chunk_id on every node, with
    /// no inline-merge/resolve_chunk_nodes fallback — ground truth when `file info`'s
    /// merged view is suspected of masking what's actually persisted.
    RawLocation {
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
    /// Verify chunk hashes and repair a file: removes corrupt replicas, heals
    /// under-replicated chunks, and trims over-replicated ones. Bypasses the
    /// post-election leadership grace period so it can be used immediately after
    /// a leader change or incident recovery.
    Repair {
        /// File path or UUID
        path: String,
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
        Commands::Stats { watch } => handle_stats_command(&cluster_addrs, watch).await?,
        Commands::RpcStats { watch } => handle_rpc_stats_command(&cluster_addrs, watch).await?,
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
                        println!("{:<10} {:<40} {:<20} {:<12} {:<8} {:<14} {}", "Short ID", "ID", "Address", "Status", "Role", "Free Space", "Last Heartbeat");
                        println!("{}", "-".repeat(130));

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
                            // total_bytes == 0 means this node's capacity hasn't been
                            // gossiped to us yet (just joined, or we're its leader and
                            // haven't seen a heartbeat back from it).
                            let space_str = if node.total_bytes > 0 {
                                format!("{:.1}/{:.1}GB", node.available_bytes as f64 / 1_073_741_824.0,
                                    node.total_bytes as f64 / 1_073_741_824.0)
                            } else {
                                "?".to_string()
                            };
                            println!(
                                "{:<10} {:<40} {:<20} {:<12} {:<8} {:<14} {}",
                                short_id,
                                id_str,
                                node.addr,
                                status_display,
                                role,
                                space_str,
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
        ClusterCommands::Set { replication_factor } => {
            let all_addrs = discover_all_addrs(cluster_addrs).await;
            let mut failed = 0usize;
            for addr in &all_addrs {
                match send_request(*addr, Request::SetReplicationFactor { replication_factor }).await {
                    Ok(Response::Ok { .. }) => {}
                    Ok(Response::Error { message, .. }) => {
                        error!("Set replication factor failed on {}: {}", addr, message);
                        failed += 1;
                    }
                    Err(e) => {
                        error!("Set replication factor error on {}: {}", addr, e);
                        failed += 1;
                    }
                    _ => { failed += 1; }
                }
            }
            if failed == 0 {
                println!("Replication factor set to {} on all {} node(s)", replication_factor, all_addrs.len());
                println!();
                println!("Note: writes only durably sync 2 replicas even at RF>=3 (3rd is async) —");
                println!("this does not change write-path durability unless RF crosses the 3 boundary.");
                println!("Over-replication trim is throttled (~200 chunks/15s tick) and gated by");
                println!("cluster-health checks — a decrease will drain gradually, not instantly.");
            } else {
                anyhow::bail!("Set replication factor failed on {}/{} node(s)", failed, all_addrs.len());
            }
        }
        ClusterCommands::Get => {
            let response = send_request(cluster_addrs[0], Request::GetClusterStatus).await?;
            match response {
                Response::ClusterStatus { replication_factor, .. } => {
                    if json_output {
                        let output = serde_json::json!({ "replication_factor": replication_factor });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Cluster Settings");
                        println!("=====================");
                        println!("Replication factor: {}", replication_factor);
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
            // Discover all cluster nodes first, then query them all in parallel.
            // The caller may only know one address (e.g. auto-detected local node).
            let all_addrs: Vec<SocketAddr> =
                if let Ok(Response::ClusterStatus { nodes, .. }) =
                    send_request(cluster_addrs[0], Request::GetClusterStatus).await
                {
                    nodes.iter().map(|n| n.addr).collect()
                } else {
                    cluster_addrs.to_vec()
                };

            // Query all nodes in parallel — each returns its raw local disk stats.
            // We aggregate here with a single RF division so the numbers match `df`.
            let handles: Vec<_> = all_addrs.iter().map(|&addr| {
                tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        send_request(addr, Request::GetStorageStats),
                    ).await;
                    (addr, result)
                })
            }).collect();

            struct NodeStat {
                addr: SocketAddr,
                total: u64,
                available: u64,
            }
            let mut node_stats: Vec<NodeStat> = Vec::new();
            let mut rf = 3usize;

            for handle in handles {
                if let Ok((addr, Ok(Ok(Response::StorageStats {
                    total_space, available_space, replication_factor, ..
                })))) = handle.await {
                    node_stats.push(NodeStat { addr, total: total_space, available: available_space });
                    rf = replication_factor;
                }
            }

            if node_stats.is_empty() {
                anyhow::bail!("No nodes responded to storage stats query");
            }

            let total_raw: u64 = node_stats.iter().map(|n| n.total).sum();
            let avail_raw: Vec<u64> = node_stats.iter().map(|n| n.available).collect();
            let usable_total = total_raw / rf as u64;
            let usable_avail = dfs_common::calculate_usable_capacity(&avail_raw, rf);
            let usable_used = usable_total.saturating_sub(usable_avail);

            let gb = |b: u64| b as f64 / 1_073_741_824.0;

            if json_output {
                let per_node: Vec<_> = node_stats.iter().map(|n| {
                    serde_json::json!({
                        "addr": n.addr.to_string(),
                        "total_gb": gb(n.total),
                        "used_gb": gb(n.total.saturating_sub(n.available)),
                        "available_gb": gb(n.available),
                        "pct_used": if n.total > 0 { 100 * n.total.saturating_sub(n.available) / n.total } else { 0 },
                    })
                }).collect();
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                    "replication_factor": rf,
                    "nodes_responding": node_stats.len(),
                    "nodes_total": all_addrs.len(),
                    "cluster": {
                        "total_gb": gb(usable_total),
                        "used_gb": gb(usable_used),
                        "available_gb": gb(usable_avail),
                    },
                    "nodes": per_node,
                }))?);
            } else {
                println!("DFS Storage Statistics");
                println!("======================");
                println!("Replication Factor:  {}", rf);
                println!("Nodes:               {}/{}", node_stats.len(), all_addrs.len());
                println!();
                println!("Per-node disk usage:");
                for n in &node_stats {
                    let used = n.total.saturating_sub(n.available);
                    let pct = if n.total > 0 { 100 * used / n.total } else { 0 };
                    println!("  {}  {:6.1} GB used / {:6.1} GB total  ({:3}% full)",
                        n.addr, gb(used), gb(n.total), pct);
                }
                println!();
                println!("Cluster totals (logical, RF={}):", rf);
                println!("  Total:     {:8.1} GB  ({:.2} TB)", gb(usable_total), gb(usable_total) / 1024.0);
                println!("  Used:      {:8.1} GB  ({:.2} TB)", gb(usable_used), gb(usable_used) / 1024.0);
                println!("  Available: {:8.1} GB  ({:.2} TB)", gb(usable_avail), gb(usable_avail) / 1024.0);
                println!("  Use%:      {:8.1}%",
                    if usable_total > 0 { 100.0 * usable_used as f64 / usable_total as f64 } else { 0.0 });
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

/// Discover every node's address via GetClusterStatus, for commands that fan out to
/// the whole cluster (e.g. `healing enable/disable/set`, `cluster set`). Falls back to
/// the caller-supplied addresses if the query fails, so the command still does
/// *something* useful rather than erroring out entirely.
async fn discover_all_addrs(cluster_addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    if let Ok(Response::ClusterStatus { nodes, .. }) =
        send_request(cluster_addrs[0], Request::GetClusterStatus).await
    {
        nodes.iter().map(|n| n.addr).collect()
    } else {
        cluster_addrs.to_vec()
    }
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
                    bandwidth_mb,
                    link_bandwidth_mb,
                    heal_max_pct,
                    heal_max_concurrent,
                    heal_max_concurrent_per_node,
                    heal_transfer_timeout_secs,
                    pending_patches_outstanding,
                    healing_delay_secs,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "enabled": enabled,
                            "pending_count": pending_count,
                            "in_flight_count": in_flight_count,
                            "stalled_count": stalled_count,
                            "last_check": last_check,
                            "bandwidth_mb": bandwidth_mb,
                            "link_bandwidth_mb": link_bandwidth_mb,
                            "heal_max_pct": heal_max_pct,
                            "heal_max_concurrent": heal_max_concurrent,
                            "heal_max_concurrent_per_node": heal_max_concurrent_per_node,
                            "heal_transfer_timeout_secs": heal_transfer_timeout_secs,
                            "pending_patches_outstanding": pending_patches_outstanding,
                            "healing_delay_secs": healing_delay_secs,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Healing Status (leader: {})", leader);
                        println!("==================");
                        println!("Enabled:       {}", if enabled { "Yes" } else { "No" });
                        println!("Pending:       {}", pending_count);
                        println!("In-flight:     {}", in_flight_count);
                        println!("Stalled:       {}", stalled_count);
                        println!("Bandwidth:     {}MB/s (ceiling: {}% of {}MB/s link)", bandwidth_mb, heal_max_pct, link_bandwidth_mb);
                        println!("Max Concurrent: {}", heal_max_concurrent);
                        println!("Max Concurrent Per Node: {}", heal_max_concurrent_per_node);
                        println!("Transfer Timeout: {}s", heal_transfer_timeout_secs);
                        println!("Healing Delay: {}s", healing_delay_secs);
                        println!("Last Check:    {} seconds ago", last_check);
                        println!("Pending patches outstanding: {}", pending_patches_outstanding);
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
            let all_addrs = discover_all_addrs(cluster_addrs).await;
            let mut failed = 0usize;
            for addr in &all_addrs {
                match send_request(*addr, Request::EnableHealing).await {
                    Ok(Response::Ok { .. }) => {}
                    Ok(Response::Error { message, .. }) => {
                        error!("Enable healing failed on {}: {}", addr, message);
                        failed += 1;
                    }
                    Err(e) => {
                        error!("Enable healing error on {}: {}", addr, e);
                        failed += 1;
                    }
                    _ => { failed += 1; }
                }
            }
            if failed == 0 {
                println!("Healing enabled on all {} node(s)", all_addrs.len());
            } else {
                anyhow::bail!("Healing enable failed on {}/{} node(s)", failed, all_addrs.len());
            }
        }
        HealingCommands::Disable => {
            let all_addrs = discover_all_addrs(cluster_addrs).await;
            let mut failed = 0usize;
            for addr in &all_addrs {
                match send_request(*addr, Request::DisableHealing).await {
                    Ok(Response::Ok { .. }) => {}
                    Ok(Response::Error { message, .. }) => {
                        error!("Disable healing failed on {}: {}", addr, message);
                        failed += 1;
                    }
                    Err(e) => {
                        error!("Disable healing error on {}: {}", addr, e);
                        failed += 1;
                    }
                    _ => { failed += 1; }
                }
            }
            if failed == 0 {
                println!("Healing disabled on all {} node(s)", all_addrs.len());
            } else {
                anyhow::bail!("Healing disable failed on {}/{} node(s)", failed, all_addrs.len());
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
        HealingCommands::Cleanup => {
            // Broadcast to all nodes — orphan reconciliation is per-node now, not
            // leader-only. Each node still applies its own safety gating (age grace,
            // two-pass confirmation, leader cross-check or all-nodes-stability) —
            // this just skips the wait for the next scheduled cycle.
            for &addr in cluster_addrs {
                let response = send_request(addr, Request::TriggerOrphanCleanup).await?;
                match response {
                    Response::Ok { .. } => {
                        println!("{}: orphan cleanup triggered", addr);
                    }
                    Response::Error { message, .. } => {
                        eprintln!("{}: error — {}", addr, message);
                    }
                    _ => {}
                }
            }
        }
        HealingCommands::Reconcile => {
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::TriggerPhantomReconciliation).await?;

            match response {
                Response::Ok { .. } => {
                    println!("Phantom reconciliation triggered on leader ({})", leader);
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
        HealingCommands::Pending { limit } => {
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::GetPendingHealingSample { limit }).await?;

            match response {
                Response::PendingHealingSample { entries, total_pending } => {
                    if json_output {
                        let items: Vec<_> = entries.iter().map(|e| serde_json::json!({
                            "chunk_id": e.chunk_id.to_string(),
                            "age_secs": e.age_secs,
                            "in_flight": e.in_flight,
                            "stalled": e.stalled,
                            "cached_alive_count": e.cached_alive_count,
                        })).collect();
                        let output = serde_json::json!({
                            "total_pending": total_pending,
                            "shown": entries.len(),
                            "entries": items,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("Pending healing queue (leader: {}) — {} total, showing {} oldest\n",
                            leader, total_pending, entries.len());
                        println!("{:<66} {:>8} {:>10} {:>8} {:>12}", "Chunk ID", "Age(s)", "In-flight", "Stalled", "Alive-cache");
                        println!("{}", "-".repeat(112));
                        for e in &entries {
                            let alive = match e.cached_alive_count {
                                Some(n) => n.to_string(),
                                None => "MISS".to_string(),
                            };
                            println!("{:<66} {:>8} {:>10} {:>8} {:>12}",
                                e.chunk_id, e.age_secs, e.in_flight, e.stalled, alive);
                        }
                        if entries.is_empty() {
                            println!("(none)");
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
        HealingCommands::Set { link_bandwidth_mb, max_pct, max_concurrent, max_concurrent_per_node, transfer_timeout_secs, healing_delay_secs } => {
            if link_bandwidth_mb.is_none() && max_pct.is_none() && max_concurrent.is_none()
                && max_concurrent_per_node.is_none() && transfer_timeout_secs.is_none() && healing_delay_secs.is_none() {
                anyhow::bail!(
                    "healing set requires at least one of --link-bandwidth-mb, --max-pct, --max-concurrent, --max-concurrent-per-node, --transfer-timeout-secs, --healing-delay-secs"
                );
            }

            let all_addrs = discover_all_addrs(cluster_addrs).await;
            let mut failed = 0usize;
            let mut applied: Option<(usize, f64, usize, usize, u64, u64)> = None;
            for addr in &all_addrs {
                match send_request(*addr, Request::SetHealingTuning {
                    link_bandwidth_mb,
                    heal_max_pct: max_pct,
                    heal_max_concurrent: max_concurrent,
                    heal_max_concurrent_per_node: max_concurrent_per_node,
                    heal_transfer_timeout_secs: transfer_timeout_secs,
                    healing_delay_secs,
                }).await {
                    Ok(Response::HealingStatus { link_bandwidth_mb, heal_max_pct, heal_max_concurrent, heal_max_concurrent_per_node, heal_transfer_timeout_secs, healing_delay_secs, .. }) => {
                        applied = Some((link_bandwidth_mb, heal_max_pct, heal_max_concurrent, heal_max_concurrent_per_node, heal_transfer_timeout_secs, healing_delay_secs));
                    }
                    Ok(Response::Error { message, .. }) => {
                        error!("Healing set failed on {}: {}", addr, message);
                        failed += 1;
                    }
                    Err(e) => {
                        error!("Healing set error on {}: {}", addr, e);
                        failed += 1;
                    }
                    _ => { failed += 1; }
                }
            }
            if failed == 0 {
                println!("Healing tuning updated on all {} node(s)", all_addrs.len());
                if let Some((bw, pct, conc, conc_per_node, timeout, delay)) = applied {
                    println!("  link_bandwidth_mb:          {}", bw);
                    println!("  heal_max_pct:               {}", pct);
                    println!("  heal_max_concurrent:        {}", conc);
                    println!("  heal_max_concurrent_per_node: {}", conc_per_node);
                    println!("  heal_transfer_timeout_secs: {}", timeout);
                    println!("  healing_delay_secs:         {}", delay);
                }
            } else {
                anyhow::bail!("Healing set failed on {}/{} node(s)", failed, all_addrs.len());
            }
        }
        HealingCommands::Get => {
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::GetHealingStatus).await?;

            match response {
                Response::HealingStatus { link_bandwidth_mb, heal_max_pct, heal_max_concurrent, heal_max_concurrent_per_node, heal_transfer_timeout_secs, healing_delay_secs, .. } => {
                    if json_output {
                        let output = serde_json::json!({
                            "link_bandwidth_mb": link_bandwidth_mb,
                            "heal_max_pct": heal_max_pct,
                            "heal_max_concurrent": heal_max_concurrent,
                            "heal_max_concurrent_per_node": heal_max_concurrent_per_node,
                            "heal_transfer_timeout_secs": heal_transfer_timeout_secs,
                            "healing_delay_secs": healing_delay_secs,
                        });
                        println!("{}", serde_json::to_string_pretty(&output)?);
                    } else {
                        println!("DFS Healing Tuning (leader: {})", leader);
                        println!("====================");
                        println!("link_bandwidth_mb:          {}", link_bandwidth_mb);
                        println!("heal_max_pct:               {}", heal_max_pct);
                        println!("heal_max_concurrent:        {}", heal_max_concurrent);
                        println!("heal_max_concurrent_per_node: {}", heal_max_concurrent_per_node);
                        println!("heal_transfer_timeout_secs: {}", heal_transfer_timeout_secs);
                        println!("healing_delay_secs:         {}", healing_delay_secs);
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

            // Query the leader, not cluster_addrs[0] — FILE_TABLE is only
            // guaranteed complete/authoritative on the leader; followers catch up
            // asynchronously via dissemination and can legitimately lag by however
            // long that takes. Querying whichever node happens to be first in
            // --cluster (which isn't necessarily the leader — leadership isn't
            // pinned to any particular node) reports a stale, incomplete chunk
            // count for a file that's actually fully committed and correct on the
            // leader. Root-caused live via the T48 background-tick test's
            // intermittent "got 7, want 8" failures under full-suite load: the
            // leader's own persisted record was always correct; this command was
            // just asking the wrong node.
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, request).await?;

            match response {
                Response::FileInfo {
                    metadata,
                    chunk_locations,
                } => {
                    if json_output {
                        let output = serde_json::json!({
                            "path": metadata.path,
                            "size": metadata.size,
                            "chunks": chunk_locations.len(),
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
                        println!("Chunks:     {}", chunk_locations.len());
                        println!("Created:    {}", metadata.created_at);
                        println!("Modified:   {}", metadata.modified_at);
                        println!("Mode:       {:o}", metadata.mode);
                        println!("UID:        {}", metadata.uid);
                        println!("GID:        {}", metadata.gid);
                        println!("Type:       {:?}", metadata.file_type);
                        println!();
                        println!("Chunk Locations:");
                        println!("{:<20} {:<14} {:<10} {}", "Chunk ID", "Offset", "Size", "Nodes");
                        println!("{}", "-".repeat(80));

                        // Detect gaps: sort by offset and flag any missing ranges.
                        let mut sorted = chunk_locations.clone();
                        sorted.sort_by_key(|l| l.file_offset.unwrap_or(u64::MAX));
                        let mut expected_offset: u64 = 0;
                        for loc in &sorted {
                            let offset = loc.file_offset.unwrap_or(u64::MAX);
                            if offset != u64::MAX && offset > expected_offset {
                                println!("  *** GAP: missing bytes {:#x}–{:#x} ({} bytes) ***",
                                    expected_offset, offset - 1, offset - expected_offset);
                            }
                            let chunk_id_str = loc.chunk_id.to_string();
                            let chunk_id_short = &chunk_id_str[..16.min(chunk_id_str.len())];
                            let offset_str = loc.file_offset
                                .map(|o| format!("{:#x}", o))
                                .unwrap_or_else(|| "?".to_string());
                            let nodes_str = loc
                                .nodes
                                .iter()
                                .map(|n| {
                                    let s = n.to_string();
                                    s[..8.min(s.len())].to_string()
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("{:<20} {:<14} {:<10} {}", chunk_id_short, offset_str, loc.size, nodes_str);
                            if offset != u64::MAX {
                                expected_offset = offset + loc.size as u64;
                            }
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
        FileCommands::RawLocation { chunk_id } => {
            let cid = parse_chunk_id(&chunk_id)?;
            for &addr in cluster_addrs {
                let response = send_request(addr, Request::DebugGetRawChunkLocation { chunk_id: cid }).await;
                match response {
                    Ok(Response::DebugRawChunkLocation { location: Some(loc) }) => {
                        println!("{}: nodes={:?} size={} written_at={:?} client_write_seq={:?} file_id={:?}",
                            addr, loc.nodes, loc.size, loc.written_at, loc.client_write_seq, loc.file_id);
                    }
                    Ok(Response::DebugRawChunkLocation { location: None }) => {
                        println!("{}: no CHUNK_TABLE record", addr);
                    }
                    Ok(other) => println!("{}: unexpected response {:?}", addr, other),
                    Err(e) => println!("{}: request failed — {}", addr, e),
                }
            }
        }
        FileCommands::List => {
            // Query the leader, not cluster_addrs[0] — see FileCommands::Info's
            // matching comment for why.
            let leader = find_leader_addr(cluster_addrs).await;
            let response = send_request(leader, Request::ListAllFiles).await?;

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
        FileCommands::Repair { path } => {
            let response = send_request(
                cluster_addrs[0],
                Request::RepairFile { path: path.clone(), force: true },
            ).await?;
            match response {
                Response::Ok { data } => {
                    let msg = data.map(|b| String::from_utf8_lossy(&b).to_string())
                        .unwrap_or_else(|| "Repair complete".to_string());
                    println!("{}", msg);
                }
                Response::Error { message, .. } => {
                    anyhow::bail!("Repair failed: {}", message);
                }
                _ => anyhow::bail!("Unexpected response from server"),
            }
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
        Response::FileInfo { metadata, chunk_locations, .. } => (metadata, chunk_locations),
        Response::Error { message, code } => {
            if code == dfs_common::ErrorCode::NotFound {
                anyhow::bail!("File not found: {}", path);
            }
            anyhow::bail!("Failed to get file info: {}", message);
        }
        _ => anyhow::bail!("Unexpected response"),
    };

    let old_chunk_count = existing_locations.len();
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

    for (i, loc) in existing_locations.iter().enumerate() {
        let chunk_id = &loc.chunk_id;
        // Pick the best node to read from
        let read_addr = chunk_node_map.get(chunk_id)
            .and_then(|addrs| addrs.first().copied())
            .unwrap_or(cluster_addrs[0]);

        let chunk_data = match send_request(read_addr, Request::ReadChunk {
            chunk_id: *chunk_id,
            sequential_hint: Some((i as u64, old_chunk_count as u64)),
            client_write_seq: None,
            file_id: None,
            chunk_idx: None,
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
                        file_id: None,
                        chunk_idx: None,
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
                file_id: metadata.id,
            }).await? {
                Response::ChunkIds { chunk_ids, chunk_sizes, .. } => (chunk_ids, chunk_sizes, node1),
                Response::Error { message, .. } => anyhow::bail!("Write to {} failed: {}", node1, message),
                _ => anyhow::bail!("Unexpected write response from {}", node1),
            };

            let (ids2, _sizes2, addr2) = match send_request(node2, Request::WriteFileLocalOnly {
                data: flush_data,
                file_offset: 0,
                file_id: metadata.id,
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
                    client_write_seq: None,
                    file_id: Some(metadata.id),
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
    new_metadata.chunk_locations = std::sync::Arc::new(new_chunk_locations);
    new_metadata.modified_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Write updated metadata to all nodes
    println!("Updating metadata on {} nodes...", all_nodes.len());
    let mut metadata_ok = 0;
    for &node in &all_nodes {
        match send_request(node, Request::PutFileMetadata {
            metadata: new_metadata.clone(),
            covers_from_write_seq: new_metadata.write_seq,
        }).await {
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
    tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        send_request_inner(addr, request),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Request to {} timed out after 10s", addr))?
}

async fn send_request_inner(addr: SocketAddr, request: Request) -> Result<Response> {
    // Connect to node
    let mut stream = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Connect to {} timed out", addr))?
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

async fn handle_stats_command(cluster_addrs: &[SocketAddr], watch: bool) -> Result<()> {
    // Discover all node addresses from cluster status so the user only needs to
    // pass one seed address; we fan out to every node automatically.
    let all_addrs: Vec<SocketAddr> = match send_request(cluster_addrs[0], Request::GetClusterStatus).await {
        Ok(Response::ClusterStatus { nodes, .. }) => {
            let mut addrs: Vec<SocketAddr> = nodes.iter().map(|n| n.addr).collect();
            if addrs.is_empty() {
                addrs = cluster_addrs.to_vec();
            }
            addrs.sort();
            addrs
        }
        _ => cluster_addrs.to_vec(),
    };

    // Identify the leader by querying cluster status once.
    let leader_addr: Option<SocketAddr> = match send_request(cluster_addrs[0], Request::GetClusterStatus).await {
        Ok(Response::ClusterStatus { nodes, leader_node_id, .. }) => {
            let lid = leader_node_id.or_else(|| {
                nodes.iter()
                    .filter(|n| n.status == dfs_common::NodeStatus::Online)
                    .map(|n| n.id)
                    .min()
            });
            lid.and_then(|lid| nodes.iter().find(|n| n.id == lid).map(|n| n.addr))
        }
        _ => None,
    };

    // For --watch: maintain one persistent connection per node so we don't open and
    // close a new TCP connection every second. Each poll reuses the same stream;
    // on failure the stream is discarded and reconnected on the next iteration.
    let mut persistent: Vec<(SocketAddr, Option<TcpStream>)> = if watch {
        all_addrs.iter().map(|&a| (a, None)).collect()
    } else {
        Vec::new()
    };

    loop {
        let mut rows: Vec<(SocketAddr, Option<Response>)> = Vec::new();

        if watch {
            for (addr, slot) in &mut persistent {
                // Ensure we have a live connection.
                if slot.is_none() {
                    *slot = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        TcpStream::connect(*addr),
                    ).await.ok().and_then(|r| r.ok());
                    if let Some(s) = slot.as_mut() {
                        let _ = s.set_nodelay(true);
                    }
                }

                let resp = if let Some(stream) = slot.as_mut() {
                    match stats_poll_persistent(stream, *addr).await {
                        Ok(r) => Some(r),
                        Err(_) => {
                            // Connection broke — drop it; reconnect next iteration.
                            *slot = None;
                            None
                        }
                    }
                } else {
                    None
                };
                rows.push((*addr, resp));
            }
        } else {
            for &addr in &all_addrs {
                let resp = send_request(addr, Request::GetNodeStats).await.ok();
                rows.push((addr, resp));
            }
        }

        if watch {
            print!("\x1b[2J\x1b[H"); // clear screen, move to top-left
        }

        print_stats_table(&rows, leader_addr);

        if !watch {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Send a single GetNodeStats request over an already-open stream and read the response.
/// Both send and receive are bounded to 5 seconds so a hung node doesn't stall the display.
async fn stats_poll_persistent(stream: &mut TcpStream, addr: SocketAddr) -> Result<Response> {
    let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
    let envelope = MessageEnvelope::new(request_id, Message::Request(Request::GetNodeStats));
    let encoded = envelope.to_bytes()?;
    let len = (encoded.len() as u32).to_be_bytes();

    tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
        stream.write_all(&len).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; msg_len];
        stream.read_exact(&mut buf).await?;

        let resp_env = MessageEnvelope::from_bytes(&buf)?;
        match resp_env.message {
            Message::Response(r) => Ok(r),
            _ => anyhow::bail!("unexpected message type"),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("stats poll to {} timed out", addr))?
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

fn print_stats_table(rows: &[(SocketAddr, Option<Response>)], leader_addr: Option<SocketAddr>) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let hh = (ts % 86400) / 3600;
    let mm = (ts % 3600) / 60;
    let ss = ts % 60;

    println!(
        "DFS Cluster Ops/sec                                         {:02}:{:02}:{:02} UTC",
        hh, mm, ss
    );
    println!();

    let hdr = format!(
        "{:<23} {:>7}  {:>7}  {:>6}  {:>7}  {:>8}  {:>7}  {:>10}  {}",
        "Node", "Reads/s", "Writ/s", "Meta/s", "Total/s", "Peak 1h", "Avg 1h", "Conns", "Uptime"
    );
    println!("{}", hdr);
    println!("{}", "─".repeat(hdr.chars().count()));

    let mut cluster_reads_live: u64 = 0;
    let mut cluster_writes_live: u64 = 0;
    let mut cluster_meta_live: u64 = 0;
    let mut cluster_total_peak: u64 = 0;
    let mut cluster_reads_avg: u64 = 0;
    let mut cluster_writes_avg: u64 = 0;
    let mut cluster_meta_avg: u64 = 0;
    let mut nodes_with_data: usize = 0;

    for (addr, resp) in rows {
        let is_leader = leader_addr == Some(*addr);
        let label = if is_leader {
            format!("{} [L]", addr)
        } else {
            format!("{}", addr)
        };

        match resp {
            Some(Response::NodeStats {
                reads_live, writes_live, meta_live,
                total_peak_1h, reads_avg_1h, writes_avg_1h, meta_avg_1h,
                uptime_secs, active_connections, max_connections, ..
            }) => {
                let total_live = reads_live + writes_live + meta_live;
                let total_avg = reads_avg_1h + writes_avg_1h + meta_avg_1h;
                cluster_reads_live += reads_live;
                cluster_writes_live += writes_live;
                cluster_meta_live += meta_live;
                cluster_total_peak = cluster_total_peak.max(*total_peak_1h);
                cluster_reads_avg += reads_avg_1h;
                cluster_writes_avg += writes_avg_1h;
                cluster_meta_avg += meta_avg_1h;
                nodes_with_data += 1;
                let conn_str = if *max_connections > 0 {
                    let pct = 100 * active_connections / max_connections;
                    if pct >= 75 {
                        format!("{}/{} !", active_connections, max_connections)
                    } else {
                        format!("{}/{}", active_connections, max_connections)
                    }
                } else {
                    String::new()
                };
                println!(
                    "{:<23} {:>7}  {:>7}  {:>6}  {:>7}  {:>8}  {:>7}  {:>10}  {}",
                    label,
                    reads_live, writes_live, meta_live, total_live,
                    total_peak_1h, total_avg, conn_str,
                    format_uptime(*uptime_secs)
                );
            }
            _ => {
                println!("{:<23} {}", label, "(unavailable)");
            }
        }
    }

    println!("{}", "─".repeat(hdr.chars().count()));

    let cluster_total_live = cluster_reads_live + cluster_writes_live + cluster_meta_live;
    let cluster_total_avg = cluster_reads_avg + cluster_writes_avg + cluster_meta_avg;
    let avg_divisor = nodes_with_data.max(1) as u64;
    println!(
        "{:<23} {:>7}  {:>7}  {:>6}  {:>7}  {:>8}  {:>7}  {:>10}",
        "Cluster",
        cluster_reads_live, cluster_writes_live, cluster_meta_live, cluster_total_live,
        cluster_total_peak,
        cluster_total_avg / avg_divisor,
        "",
    );
    println!();
}

async fn handle_rpc_stats_command(cluster_addrs: &[SocketAddr], watch: bool) -> Result<()> {
    // Same node-discovery + persistent-connection pattern as handle_stats_command.
    let all_addrs: Vec<SocketAddr> = match send_request(cluster_addrs[0], Request::GetClusterStatus).await {
        Ok(Response::ClusterStatus { nodes, .. }) => {
            let mut addrs: Vec<SocketAddr> = nodes.iter().map(|n| n.addr).collect();
            if addrs.is_empty() {
                addrs = cluster_addrs.to_vec();
            }
            addrs.sort();
            addrs
        }
        _ => cluster_addrs.to_vec(),
    };

    let leader_addr: Option<SocketAddr> = match send_request(cluster_addrs[0], Request::GetClusterStatus).await {
        Ok(Response::ClusterStatus { nodes, leader_node_id, .. }) => {
            let lid = leader_node_id.or_else(|| {
                nodes.iter()
                    .filter(|n| n.status == dfs_common::NodeStatus::Online)
                    .map(|n| n.id)
                    .min()
            });
            lid.and_then(|lid| nodes.iter().find(|n| n.id == lid).map(|n| n.addr))
        }
        _ => None,
    };

    let mut persistent: Vec<(SocketAddr, Option<TcpStream>)> = if watch {
        all_addrs.iter().map(|&a| (a, None)).collect()
    } else {
        Vec::new()
    };

    loop {
        let mut rows: Vec<(SocketAddr, Option<Response>)> = Vec::new();

        if watch {
            for (addr, slot) in &mut persistent {
                if slot.is_none() {
                    *slot = tokio::time::timeout(
                        tokio::time::Duration::from_secs(5),
                        TcpStream::connect(*addr),
                    ).await.ok().and_then(|r| r.ok());
                    if let Some(s) = slot.as_mut() {
                        let _ = s.set_nodelay(true);
                    }
                }

                let resp = if let Some(stream) = slot.as_mut() {
                    match rpc_stats_poll_persistent(stream, *addr).await {
                        Ok(r) => Some(r),
                        Err(_) => {
                            *slot = None;
                            None
                        }
                    }
                } else {
                    None
                };
                rows.push((*addr, resp));
            }
        } else {
            for &addr in &all_addrs {
                let resp = send_request(addr, Request::GetRpcClassCounts).await.ok();
                rows.push((addr, resp));
            }
        }

        if watch {
            print!("\x1b[2J\x1b[H");
        }

        print_rpc_stats_table(&rows, leader_addr);

        if !watch {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }

    Ok(())
}

/// Send a single GetRpcClassCounts request over an already-open stream and
/// read the response. See stats_poll_persistent for the identical pattern.
async fn rpc_stats_poll_persistent(stream: &mut TcpStream, addr: SocketAddr) -> Result<Response> {
    let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
    let envelope = MessageEnvelope::new(request_id, Message::Request(Request::GetRpcClassCounts));
    let encoded = envelope.to_bytes()?;
    let len = (encoded.len() as u32).to_be_bytes();

    tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
        stream.write_all(&len).await?;
        stream.write_all(&encoded).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let msg_len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; msg_len];
        stream.read_exact(&mut buf).await?;

        let resp_env = MessageEnvelope::from_bytes(&buf)?;
        match resp_env.message {
            Message::Response(r) => Ok(r),
            _ => anyhow::bail!("unexpected message type"),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("rpc-stats poll to {} timed out", addr))?
}

fn print_rpc_stats_table(rows: &[(SocketAddr, Option<Response>)], leader_addr: Option<SocketAddr>) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let hh = (ts % 86400) / 3600;
    let mm = (ts % 3600) / 60;
    let ss = ts % 60;

    println!(
        "DFS RPC Class Counts (cumulative since startup, in-memory only)   {:02}:{:02}:{:02} UTC",
        hh, mm, ss
    );
    println!();

    // Cluster-wide totals, accumulated while printing each node's row.
    let mut peer_healing = 0u64;
    let mut peer_delete_ops = 0u64;
    let mut peer_fold = 0u64;
    let mut peer_gossip = 0u64;
    let mut peer_other = 0u64;
    let mut client_full_patch = 0u64;
    let mut client_multi_patch = 0u64;
    let mut client_fold = 0u64;
    let mut client_other = 0u64;
    let mut admin = 0u64;
    let mut delete_reasons: HashMap<String, u64> = HashMap::new();

    for (addr, resp) in rows {
        let is_leader = leader_addr == Some(*addr);
        let label = if is_leader { format!("{} [L]", addr) } else { format!("{}", addr) };

        match resp {
            Some(Response::RpcClassCounts {
                peer_healing: ph, peer_delete_ops: pd, peer_fold: pf, peer_gossip: pg, peer_other: po,
                client_full_patch: cfp, client_multi_patch: cmp, client_fold: cf, client_other: co,
                admin: ad, delete_reasons: dr,
            }) => {
                let peer_total = ph + pd + pf + pg + po;
                let client_total = cfp + cmp + cf + co;
                let total = (peer_total + client_total + ad).max(1); // avoid div-by-zero in the % below
                println!("{}", label);
                println!(
                    "  peer:   healing={:<8} delete={:<8} fold={:<8} gossip={:<8} other={:<8} ({:>5.1}% of total)",
                    ph, pd, pf, pg, po, 100.0 * peer_total as f64 / total as f64
                );
                println!(
                    "  client: full-patch={:<8} multi-patch={:<8} fold={:<8} other={:<8} ({:>5.1}% of total)",
                    cfp, cmp, cf, co, 100.0 * client_total as f64 / total as f64
                );
                println!(
                    "  admin:  {:<8} ({:>5.1}% of total)",
                    ad, 100.0 * *ad as f64 / total as f64
                );
                if !dr.is_empty() {
                    let mut sorted: Vec<&(String, u64)> = dr.iter().collect();
                    sorted.sort_by(|a, b| b.1.cmp(&a.1));
                    let reasons_str: Vec<String> = sorted.iter().map(|(r, c)| format!("{}={}", r, c)).collect();
                    println!("  local deletes by reason: {}", reasons_str.join(", "));
                }
                println!();

                peer_healing += ph; peer_delete_ops += pd; peer_fold += pf; peer_gossip += pg; peer_other += po;
                client_full_patch += cfp; client_multi_patch += cmp; client_fold += cf; client_other += co;
                admin += ad;
                for (reason, count) in dr {
                    *delete_reasons.entry(reason.clone()).or_insert(0) += count;
                }
            }
            _ => {
                println!("{}", label);
                println!("  (unavailable)");
                println!();
            }
        }
    }

    println!("{}", "─".repeat(60));
    let peer_total = peer_healing + peer_delete_ops + peer_fold + peer_gossip + peer_other;
    let client_total = client_full_patch + client_multi_patch + client_fold + client_other;
    let total = (peer_total + client_total + admin).max(1);
    println!("Cluster totals:");
    println!(
        "  peer:   healing={:<8} delete={:<8} fold={:<8} gossip={:<8} other={:<8} ({:>5.1}% of total)",
        peer_healing, peer_delete_ops, peer_fold, peer_gossip, peer_other, 100.0 * peer_total as f64 / total as f64
    );
    println!(
        "  client: full-patch={:<8} multi-patch={:<8} fold={:<8} other={:<8} ({:>5.1}% of total)",
        client_full_patch, client_multi_patch, client_fold, client_other, 100.0 * client_total as f64 / total as f64
    );
    println!(
        "  admin:  {:<8} ({:>5.1}% of total)",
        admin, 100.0 * admin as f64 / total as f64
    );
    if !delete_reasons.is_empty() {
        let mut sorted: Vec<(&String, &u64)> = delete_reasons.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        let reasons_str: Vec<String> = sorted.iter().map(|(r, c)| format!("{}={}", r, c)).collect();
        println!("  local deletes by reason: {}", reasons_str.join(", "));
    }
    println!();
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
