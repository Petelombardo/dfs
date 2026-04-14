mod client;
mod fuse_impl;
mod locks;

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

        /// Disable write-behind buffering (enabled by default for better performance)
        #[arg(long)]
        no_write_buffer: bool,

        /// Allow all users to access the mounted filesystem (requires user_allow_other in /etc/fuse.conf)
        #[arg(long)]
        allow_other: bool,

        /// Log file path (default: stderr in foreground, /var/log/dfs-client.log in daemon mode)
        #[arg(long)]
        log_file: Option<PathBuf>,

        /// Log level (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,
    },

    /// Unmount the DFS filesystem
    Unmount {
        /// Mount point to unmount
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,
    },

    /// Install and enable systemd service
    SystemdInstall {
        /// Mount point (local directory)
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: PathBuf,

        /// Cluster nodes (comma-separated, e.g., 192.168.1.10:8900,192.168.1.11:8900)
        #[arg(short, long, value_delimiter = ',')]
        cluster: Vec<String>,

        /// Disable write-behind buffering (enabled by default for better performance)
        #[arg(long)]
        no_write_buffer: bool,

        /// Allow all users to access the mounted filesystem (requires user_allow_other in /etc/fuse.conf)
        #[arg(long)]
        allow_other: bool,

        /// Log file path (default: /var/log/dfs-client.log)
        #[arg(long)]
        log_file: Option<PathBuf>,

        /// Log level (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,

        /// Service name (default: dfs-client)
        #[arg(long, default_value = "dfs-client")]
        service_name: String,
    },

    /// Uninstall systemd service
    SystemdUninstall {
        /// Service name (default: dfs-client)
        #[arg(long, default_value = "dfs-client")]
        service_name: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Mount {
            mountpoint,
            cluster,
            foreground,
            no_write_buffer,
            allow_other,
            log_file,
            log_level,
        } => {
            // Set up logging before anything else
            let _guard = setup_logging(foreground, log_file.as_deref(), &log_level)?;
            // write_buffer is enabled by default, disabled if no_write_buffer flag is set
            let write_buffer = !no_write_buffer;
            mount_filesystem(mountpoint, cluster, foreground, write_buffer, allow_other)?;
            // _guard is dropped here, flushing remaining logs
        }
        Commands::Unmount { mountpoint } => {
            // Initialize basic logging for unmount
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_target(false)
                .init();

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(unmount_filesystem(mountpoint))?;
        }
        Commands::SystemdInstall {
            mountpoint,
            cluster,
            no_write_buffer,
            allow_other,
            log_file,
            log_level,
            service_name,
        } => {
            // Initialize basic logging
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_target(false)
                .init();

            // write_buffer is enabled by default, disabled if no_write_buffer flag is set
            let write_buffer = !no_write_buffer;
            systemd_install(mountpoint, cluster, write_buffer, allow_other, log_file, &log_level, &service_name)?;
        }
        Commands::SystemdUninstall { service_name } => {
            // Initialize basic logging
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_target(false)
                .init();

            systemd_uninstall(&service_name)?;
        }
    }

    Ok(())
}

fn mount_filesystem(
    mountpoint: PathBuf,
    cluster_nodes: Vec<String>,
    foreground: bool,
    write_buffer: bool,
    allow_other: bool,
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
        info!("Write-behind buffering ENABLED (default) - better performance, data flushed on file close/fsync");
    } else {
        info!("Write-behind buffering DISABLED - immediate writes with lower performance");
    }

    // Create filesystem WITH a tokio runtime running in a background thread
    // This is necessary because FUSE callbacks run on non-tokio threads
    // and need to call async functions
    use std::sync::Arc;
    use std::thread;

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(16)
            .enable_all()
            .build()?
    );
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
        // Set max_read to 1MB for optimal parallelism
        // Smaller reads allow more concurrent FUSE requests, hiding network latency
        fuser::MountOption::CUSTOM("max_read=1048576".to_string()),
    ];

    // Note: AllowRoot and AllowOther are mutually exclusive
    if allow_other {
        info!("Enabling AllowOther - all users can access the mount");
        options.push(fuser::MountOption::AllowOther);
    } else {
        // Allow root to access (needed for write-back caching)
        options.push(fuser::MountOption::AllowRoot);
    }

    info!("Mounting filesystem at {:?}", mountpoint);

    // Mount the filesystem using spawn_mount2 for multi-threaded FUSE dispatch.
    // Unlike mount2 (which serializes all FUSE callbacks on the calling thread),
    // spawn_mount2 processes requests on a thread pool so reads and writes can
    // proceed concurrently instead of queuing behind each other.
    // BackgroundSession keeps the mount alive; dropping it unmounts.
    let _session = fuser::spawn_mount2(fs, mountpoint, &options)
        .context("Failed to mount filesystem")?;

    // Block the main thread until the mount is torn down.
    // The background FUSE threads handle all requests independently.
    #[allow(clippy::empty_loop)]
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
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

/// Set up logging to file or console
/// Returns a WorkerGuard that must be kept alive for the duration of the program
fn setup_logging(
    foreground: bool,
    log_file: Option<&std::path::Path>,
    log_level: &str,
) -> Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    use std::fs::OpenOptions;
    use tracing_subscriber::fmt::writer::MakeWriterExt;

    // Parse log level
    let level = match log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "info" => Level::INFO,
        "warn" => Level::WARN,
        "error" => Level::ERROR,
        _ => {
            eprintln!("Invalid log level '{}', using 'info'", log_level);
            Level::INFO
        }
    };

    // Determine log file path
    let log_path = if let Some(path) = log_file {
        path.to_path_buf()
    } else if !foreground {
        PathBuf::from("/var/log/dfs-client.log")
    } else {
        // Foreground mode with no log file - use non-blocking stderr
        let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stderr());
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .with_writer(non_blocking)
            .init();
        return Ok(Some(guard));
    };

    // Open log file in append mode
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open log file: {:?}", log_path))?;

    // Set up NON-BLOCKING file logging
    // This uses a background thread with a bounded channel (default 8192 messages)
    // If the channel fills up, log messages are DROPPED instead of blocking the process
    // This prevents the entire DFS process from freezing if disk is full or slow
    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(non_blocking)
        .init();

    info!("Logging to: {:?} at level: {} (non-blocking mode)", log_path, log_level);

    Ok(Some(guard))
}

/// Install systemd service for DFS client
fn systemd_install(
    mountpoint: PathBuf,
    cluster: Vec<String>,
    write_buffer: bool,
    allow_other: bool,
    log_file: Option<PathBuf>,
    log_level: &str,
    service_name: &str,
) -> Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    // Get current binary path
    let binary_path = std::env::current_exe()
        .context("Failed to get current executable path")?;

    // Ensure mount point exists
    if !mountpoint.exists() {
        fs::create_dir_all(&mountpoint)
            .with_context(|| format!("Failed to create mount point: {:?}", mountpoint))?;
        info!("Created mount point: {:?}", mountpoint);
    }

    // Build command line
    let cluster_arg = cluster.join(",");
    let log_arg = log_file
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "/var/log/dfs-client.log".to_string());

    let mut exec_start = format!(
        "{} mount {:?} --cluster {}",
        binary_path.display(),
        mountpoint,
        cluster_arg
    );

    // write_buffer is now enabled by default, so only add flag if DISABLED
    if !write_buffer {
        exec_start.push_str(" --no-write-buffer");
    }

    if allow_other {
        exec_start.push_str(" --allow-other");
    }

    exec_start.push_str(&format!(" --log-file {}", log_arg));
    exec_start.push_str(&format!(" --log-level {}", log_level));

    // Generate systemd service file
    let service_content = format!(
        r#"[Unit]
Description=DFS FUSE Client
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={}
Restart=on-failure
RestartSec=5
# FUSE requires these capabilities
CapabilityBoundingSet=CAP_SYS_ADMIN
AmbientCapabilities=CAP_SYS_ADMIN
# Allow clean unmount on stop
ExecStop=/bin/fusermount -u {:?}
TimeoutStopSec=10

[Install]
WantedBy=multi-user.target
"#,
        exec_start, mountpoint
    );

    // Write service file
    let service_file = format!("/etc/systemd/system/{}.service", service_name);
    fs::write(&service_file, service_content)
        .with_context(|| format!("Failed to write service file: {}", service_file))?;

    info!("Created systemd service file: {}", service_file);

    // Reload systemd daemon
    let output = Command::new("systemctl")
        .arg("daemon-reload")
        .output()
        .context("Failed to reload systemd daemon")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to reload systemd daemon: {}", stderr);
    }

    // Enable service
    let output = Command::new("systemctl")
        .arg("enable")
        .arg(&service_name)
        .output()
        .with_context(|| format!("Failed to enable service: {}", service_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to enable service: {}", stderr);
    }

    info!("Enabled systemd service: {}", service_name);

    // Start service
    let output = Command::new("systemctl")
        .arg("start")
        .arg(&service_name)
        .output()
        .with_context(|| format!("Failed to start service: {}", service_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to start service: {}", stderr);
    }

    info!("Started systemd service: {}", service_name);
    info!("");
    info!("DFS client installed and started!");
    info!("  Service: {}", service_name);
    info!("  Mount point: {:?}", mountpoint);
    info!("  Log file: {}", log_arg);
    info!("");
    info!("Useful commands:");
    info!("  systemctl status {}", service_name);
    info!("  systemctl stop {}", service_name);
    info!("  systemctl start {}", service_name);
    info!("  journalctl -u {} -f", service_name);
    info!("  tail -f {}", log_arg);

    Ok(())
}

/// Uninstall systemd service
fn systemd_uninstall(service_name: &str) -> Result<()> {
    use std::fs;
    use std::process::Command;

    let service_file = format!("/etc/systemd/system/{}.service", service_name);

    // Stop service
    info!("Stopping service: {}", service_name);
    let output = Command::new("systemctl")
        .arg("stop")
        .arg(&service_name)
        .output()
        .context("Failed to stop service")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        info!("Warning: Failed to stop service: {}", stderr);
    }

    // Disable service
    info!("Disabling service: {}", service_name);
    let output = Command::new("systemctl")
        .arg("disable")
        .arg(&service_name)
        .output()
        .context("Failed to disable service")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        info!("Warning: Failed to disable service: {}", stderr);
    }

    // Remove service file
    if std::path::Path::new(&service_file).exists() {
        fs::remove_file(&service_file)
            .with_context(|| format!("Failed to remove service file: {}", service_file))?;
        info!("Removed service file: {}", service_file);
    }

    // Reload systemd daemon
    let output = Command::new("systemctl")
        .arg("daemon-reload")
        .output()
        .context("Failed to reload systemd daemon")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to reload systemd daemon: {}", stderr);
    }

    info!("Systemd service uninstalled: {}", service_name);

    Ok(())
}
