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
    },

    /// Unmount the DFS filesystem
    Unmount {
        /// Mount point to unmount
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,
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

    match cli.command {
        Commands::Mount {
            mountpoint,
            cluster,
            foreground,
        } => {
            mount_filesystem(mountpoint, cluster, foreground).await?;
        }
        Commands::Unmount { mountpoint } => {
            unmount_filesystem(mountpoint).await?;
        }
    }

    Ok(())
}

async fn mount_filesystem(
    mountpoint: PathBuf,
    cluster_nodes: Vec<String>,
    foreground: bool,
) -> Result<()> {
    info!("Mounting DFS at {:?}", mountpoint);

    // Parse cluster node addresses
    let mut addrs = Vec::new();
    for node_str in cluster_nodes {
        let addr: SocketAddr = node_str
            .parse()
            .with_context(|| format!("Invalid node address: {}", node_str))?;
        addrs.push(addr);
    }

    if addrs.is_empty() {
        anyhow::bail!("No cluster nodes specified. Use --cluster to specify at least one node.");
    }

    info!("Connecting to cluster nodes: {:?}", addrs);

    // Create filesystem
    let fs = DfsFilesystem::new(addrs)?;

    // Mount options
    let mut options = vec![
        fuser::MountOption::FSName("dfs".to_string()),
        fuser::MountOption::AutoUnmount,
    ];

    if foreground {
        options.push(fuser::MountOption::AllowOther);
    }

    info!("Mounting filesystem at {:?}", mountpoint);

    // Mount the filesystem
    // Note: This blocks until unmount
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
