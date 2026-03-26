mod client;
mod fuse_impl;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fuse_impl::DfsFilesystem;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing::{info, Level};
use tracing_subscriber;

#[derive(Parser)]
#[command(name = "dfs-client")]
#[command(about = "DFS FUSE client - mount distributed filesystem", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Mount the DFS filesystem
    Mount {
        /// Mount point (local directory)
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,

        /// Cluster nodes (comma-separated, e.g., 192.168.1.10:8900,192.168.1.11:8900)
        #[arg(short, long, value_delimiter = ',')]
        cluster: Vec<String>,

        /// Run in foreground (don't daemonize)
        #[arg(short, long)]
        foreground: bool,

        /// Enable write-behind buffering for better performance (may lose unflushed data on crash)
        #[arg(long)]
        write_buffer: bool,
    },

    /// Unmount the DFS filesystem
    Unmount {
        /// Mount point to unmount
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,
    },
}

fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Mount {
            mountpoint,
            cluster,
            foreground,
            write_buffer,
        } => {
            mount_filesystem(mountpoint, cluster, foreground, write_buffer)?;
        }
        Commands::Unmount { mountpoint } => {
            // For unmount, we can use a temporary runtime
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(unmount_filesystem(mountpoint))?;
        }
    }

    Ok(())
}

fn mount_filesystem(
    mountpoint: PathBuf,
    cluster_nodes: Vec<String>,
    foreground: bool,
    write_buffer: bool,
) -> Result<()> {
    info!("Mounting DFS at {:?}", mountpoint);

    // Parse cluster node addresses - add default port 8900 if not specified
    let mut seed_addrs = Vec::new();
    for node_str in cluster_nodes {
        let addr = parse_node_address(&node_str)
            .with_context(|| format!("Invalid node address: {}", node_str))?;
        seed_addrs.push(addr);
    }

    if seed_addrs.is_empty() {
        anyhow::bail!("No cluster nodes specified. Use --cluster to specify at least one seed node.");
    }

    info!("Connecting to seed node(s): {:?}", seed_addrs);

    // Auto-discover full cluster by querying the first reachable seed node
    let addrs = discover_cluster_nodes(&seed_addrs)?;

    info!("Discovered {} cluster nodes: {:?}", addrs.len(), addrs);

    if write_buffer {
        info!("Write-behind buffering ENABLED - better performance, may lose unflushed data on crash");
    } else {
        info!("Write-behind buffering DISABLED - all writes go through Raft immediately");
    }

    // Create filesystem WITH a tokio runtime running in a background thread
    // This is necessary because FUSE callbacks run on non-tokio threads
    // and need to call async functions
    use std::sync::Arc;
    use std::thread;

    let runtime = Arc::new(tokio::runtime::Runtime::new()?);
    let runtime_clone = runtime.clone();

    // Spawn a thread to keep the runtime alive
    // It will just park itself, keeping the runtime alive for the duration
    let _runtime_thread = thread::spawn(move || {
        runtime_clone.block_on(async {
            // Just sleep forever - this keeps the runtime alive
            // The thread will be killed when the process exits
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            }
        })
    });

    // Give the runtime thread a moment to start
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Now create the filesystem with access to the runtime handle
    let runtime_handle = runtime.handle().clone();
    let fs = DfsFilesystem::new_with_runtime(addrs, write_buffer, runtime_handle)?;

    // Mount options
    let mut options = vec![
        fuser::MountOption::FSName("dfs".to_string()),
        fuser::MountOption::AutoUnmount,
        // Enable write-back caching for better performance
        fuser::MountOption::Async,
        // Allow root to access (needed for write-back caching)
        fuser::MountOption::AllowRoot,
    ];

    if foreground {
        options.push(fuser::MountOption::AllowOther);
    }

    info!("Mounting filesystem at {:?}", mountpoint);

    // Mount the filesystem
    // Note: This blocks until unmount
    // The runtime stays alive because _rt is in scope
    fuser::mount2(fs, mountpoint, &options)
        .context("Failed to mount filesystem")?;

    info!("Filesystem unmounted");

    Ok(())
}

async fn unmount_filesystem(mountpoint: PathBuf) -> Result<()> {
    info!("Unmounting DFS at {:?}", mountpoint);

    // FUSE unmount is handled automatically with AutoUnmount option
    // Or can be done manually with: fusermount -u <mountpoint>

    #[cfg(target_os = "linux")]
    {
        use std::process::Command;

        let output = Command::new("fusermount")
            .arg("-u")
            .arg(&mountpoint)
            .output()
            .context("Failed to execute fusermount")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to unmount: {}", stderr);
        }

        info!("Successfully unmounted {:?}", mountpoint);
    }

    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!("Unmount command not supported on this platform. Use 'umount' command.");
    }

    Ok(())
}

/// Parse node address, adding default port 8900 if not specified
fn parse_node_address(node_str: &str) -> Result<SocketAddr> {
    // If it already has a port, parse directly
    if node_str.contains(':') {
        return node_str.parse().context("Invalid address format");
    }

    // Otherwise, add default port 8900
    let with_port = format!("{}:8900", node_str);
    with_port.parse().context("Invalid IP address")
}

/// Discover all cluster nodes by querying GetClusterStatus from a seed node
fn discover_cluster_nodes(seed_addrs: &[SocketAddr]) -> Result<Vec<SocketAddr>> {
    use dfs_common::{Message, MessageEnvelope, Request, RequestId, Response};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

    // Try each seed node until one responds
    for seed_addr in seed_addrs {
        info!("Querying cluster status from {}", seed_addr);

        let result = tokio::runtime::Runtime::new()?.block_on(async {
            // Connect to seed node
            let mut stream = match TcpStream::connect(seed_addr).await {
                Ok(s) => s,
                Err(e) => {
                    info!("Failed to connect to {}: {}", seed_addr, e);
                    return Err(anyhow::anyhow!("Connection failed"));
                }
            };

            // Send GetClusterStatus request
            let request = Request::GetClusterStatus;
            let request_id = RequestId::new(REQUEST_COUNTER.fetch_add(1, Ordering::SeqCst));
            let envelope = MessageEnvelope::new(request_id, Message::Request(request));
            let encoded = envelope.to_bytes()?;

            stream.write_u32(encoded.len() as u32).await?;
            stream.write_all(&encoded).await?;
            stream.flush().await?;

            // Read response
            let response_len = stream.read_u32().await?;
            let mut buf = vec![0u8; response_len as usize];
            stream.read_exact(&mut buf).await?;

            let response_envelope = MessageEnvelope::from_bytes(&buf)?;

            match response_envelope.message {
                Message::Response(Response::ClusterStatus { nodes, .. }) => {
                    // Extract addresses from all online nodes
                    let addrs: Vec<SocketAddr> = nodes
                        .iter()
                        .filter(|n| n.status == dfs_common::NodeStatus::Online)
                        .map(|n| n.addr)
                        .collect();

                    if addrs.is_empty() {
                        anyhow::bail!("No online nodes in cluster");
                    }

                    info!("Discovered {} online nodes from {}", addrs.len(), seed_addr);
                    Ok(addrs)
                }
                _ => anyhow::bail!("Unexpected response to GetClusterStatus"),
            }
        });

        if let Ok(addrs) = result {
            return Ok(addrs);
        }
    }

    anyhow::bail!("Failed to discover cluster: all seed nodes unreachable")
}
