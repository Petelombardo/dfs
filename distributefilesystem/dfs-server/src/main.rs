mod chunker;
mod metadata;
mod storage;

use anyhow::Result;
use clap::{Parser, Subcommand};
use dfs_common::Config;
use std::path::PathBuf;
use tracing::info;
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
    let _storage = storage::ChunkStorage::new(config.storage.data_dir.clone())?;
    info!("✓ Chunk storage initialized");

    // Initialize metadata store
    let _metadata = metadata::MetadataStore::new(config.storage.metadata_dir.clone())?;
    info!("✓ Metadata store initialized");

    info!("");
    info!("DFS server is ready!");
    info!("Listening on: {}", config.node.listen_addr);

    // TODO: Start network server in Phase 3
    info!("Note: Network layer not yet implemented (Phase 3)");

    // Keep running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
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
