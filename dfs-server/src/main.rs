// Use jemalloc to reduce heap fragmentation from repeated large (4MB chunk) allocations.
// glibc malloc holds onto freed arenas, causing RSS to grow proportionally to peak throughput.
// jemalloc returns unused memory to the OS aggressively, keeping RSS close to live set size.
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod chunker;
mod cluster;
mod healing;
mod metadata;
mod metadata_sql;
mod network;
mod server;
mod stats;
mod storage;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dfs_common::Config;
use std::path::PathBuf;
use tracing::{debug, error, info, warn, Level};
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

        /// Log level (trace, debug, info, warn, error)
        #[arg(long, default_value = "info")]
        log_level: String,

        /// Write logs to this file instead of stderr/journal. Recommended on
        /// durable storage (e.g. under the same mount as data_dir/metadata_dir)
        /// rather than relying on journald: several storage-node hosts run
        /// journald with Storage=volatile and a small (~20MB) ring buffer,
        /// which gets overwritten within minutes under heavy write load — real
        /// pre-crash evidence has been lost this way. See CLAUDE.md's local
        /// suite notes and the 2026-07-11 gluster1/gluster4 incident.
        #[arg(long)]
        log_file: Option<PathBuf>,
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
    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            data_dir,
            meta_dir,
            config,
        } => {
            // Initialize basic logging for init
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_target(false)
                .init();
            init_node(data_dir, meta_dir, config)?;
        }
        Commands::Start { config, log_level, log_file } => {
            // --log-level flag wins over RUST_LOG env var. Without this, an
            // Environment="RUST_LOG=warn" in the systemd unit silently overrides
            // the explicit CLI flag and suppresses INFO/ERROR messages.
            std::env::set_var("RUST_LOG", &log_level);

            let _guard = setup_logging(log_file.as_deref(), &log_level)?;
            start_server(config).await?;
            // _guard is dropped here, flushing remaining logs
        }
        Commands::Status { config } => {
            // Initialize basic logging for status
            tracing_subscriber::fmt()
                .with_max_level(Level::INFO)
                .with_target(false)
                .init();
            show_status(config)?;
        }
    }

    Ok(())
}

/// Set up logging to file or stderr/journal. Mirrors dfs-client's setup_logging
/// (see dfs-client/src/main.rs) — same non-blocking-writer/WorkerGuard shape,
/// same append-mode file open. Returns the WorkerGuard that must be kept alive
/// for the duration of the program (dropping it flushes remaining log lines).
fn setup_logging(
    log_file: Option<&std::path::Path>,
    log_level: &str,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use std::fs::OpenOptions;

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

    let Some(path) = log_file else {
        // No file given — keep the original behavior: non-blocking stderr,
        // which systemd captures into the journal.
        let (non_blocking, guard) = tracing_appender::non_blocking(std::io::stderr());
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_target(false)
            .with_writer(non_blocking)
            .init();
        info!("Starting DFS server with log level: {} (non-blocking mode, stderr/journal)", log_level);
        return Ok(guard);
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create log directory: {:?}", parent))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Failed to open log file: {:?}", path))?;

    // Non-blocking: a background thread with a bounded channel (default 8192
    // messages) drains it. If the channel fills, messages are DROPPED rather
    // than blocking the process — a full/slow disk must never freeze the server.
    let (non_blocking, guard) = tracing_appender::non_blocking(file);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(non_blocking)
        .init();

    info!("Starting DFS server with log level: {} (non-blocking mode). Logging to: {:?}", log_level, path);
    Ok(guard)
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

    // Load configuration (mutable so we can write node_id back if needed)
    let mut config = Config::from_file(&config_path)?;

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

    // Load or create persistent node ID. Pass config_path so the ID is written
    // back to config on first generation (migrating from node_id.json if present).
    let node_id = config.load_or_create_node_id(Some(&config_path))?;
    info!("Node ID: {}", node_id);

    // Resolve the healing-tuning knobs: config wins if already set, otherwise migrate
    // from the legacy DFS_* env vars (once) and persist — same pattern as node_id above.
    config.load_or_migrate_healing_tuning(Some(&config_path))?;

    // The address we register in the cluster and advertise to peers.
    // Prefer advertise_addr; fall back to listen_addr. Either way the client
    // now gets the real address from ClusterStatus and can map it to node_id.
    let peer_addr = config.peer_addr();
    info!("Peer address: {}", peer_addr);

    // Initialize cluster manager
    let cluster = std::sync::Arc::new(cluster::ClusterManager::new(
        node_id,
        peer_addr,
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
        config_path.clone(),
        config.storage.metadata_batch_drain_enabled,
    ));
    info!("✓ Server instance created");

    // Rebuild the fold-origin set BEFORE the chunk map rebuild below: the chunk
    // map rebuild's per-slot merge consults this set for the fold-vs-client
    // tiebreak, so it must already be populated when that merge runs.
    server.rebuild_fold_result_chunk_ids();
    info!("✓ Fold-result chunk_id set rebuilt");

    // Rebuild in-memory chunk map from persistent metadata.
    // This is required on every startup — GetFileChunkMap is served from this
    // in-memory map, so without it every file returns "no chunk map from leader".
    server.rebuild_chunk_map_from_metadata();
    info!("✓ Chunk map rebuild started (background)");

    // Start failure detector
    server.cluster().start_failure_detector().await;
    info!("✓ Failure detector started");

    // Start heartbeat sender
    server.cluster().start_heartbeat_sender().await;
    info!("✓ Heartbeat sender started");

    // Start local disk-capacity refresh loop — keeps this node's entry in the cluster
    // capacity map fresh so outgoing heartbeats always carry a real, recent number for
    // peers (and the leader) to make capacity-aware placement decisions with.
    server.clone().start_capacity_refresh_loop().await;
    info!("✓ Capacity refresh loop started");

    server.clone().start_chunk_ring_stats_loop().await;
    info!("✓ chunk_ring stats loop started");

    server.clone().start_memory_diag_loop().await;
    info!("✓ memory diagnostics loop started");

    // Start healing manager on its own dedicated, lower-priority runtime — kept
    // separate from the main multi-threaded runtime that serves live client RPCs
    // (writes/reads/patches). Healing's PushChunkTo transfers move full 4MB chunks
    // and used to compete on equal footing with live request handling for the same
    // CPU cores AND the same disk I/O scheduling class; under a write-heavy workload
    // that meant healing could visibly eat into client-facing throughput even though
    // it's inherently best-effort/eventual work. A single worker thread is enough —
    // healing is already bandwidth-budget limited by heal_semaphore — and
    // on_thread_start lowers both that thread's CPU scheduling priority (higher nice
    // value) and its disk I/O scheduling class (IOPRIO_CLASS_IDLE) so the kernel
    // favors the main runtime's request-handling threads for both CPU and disk
    // access whenever both have runnable/pending work on a contended resource. This
    // only affects scheduling priority between dfs-server's own threads; it does not
    // throttle healing's already-existing bandwidth/rate limits.
    let healer_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("dfs-healer")
        .on_thread_start(|| {
            // SAFETY: nice() only ever adjusts the calling thread's own scheduling
            // priority; it cannot affect other threads. -1 is both a valid returned
            // niceness and the error sentinel, so errno must be cleared first and
            // checked after — per nice(2).
            unsafe {
                *libc::__errno_location() = 0;
                let prev = libc::nice(10);
                if prev == -1 && *libc::__errno_location() != 0 {
                    warn!("Failed to lower healer thread niceness (continuing at default priority)");
                }
            }

            // Drop this thread's disk I/O scheduling class to IDLE — it only yields
            // I/O bandwidth to other classes (RT/BE) on the SAME block device when
            // they have pending I/O; it never blocks outright. ioprio_set has no
            // libc wrapper, so it's issued as a raw syscall (SYS_ioprio_set, like
            // SYS_gettid below, is resolved correctly per-target-arch by the libc
            // crate — verified for x86_64 and aarch64). IOPRIO_CLASS_IDLE = 3,
            // IOPRIO_CLASS_SHIFT = 13 — see ioprio_set(2).
            unsafe {
                const IOPRIO_WHO_PROCESS: libc::c_int = 1;
                const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
                const IOPRIO_CLASS_IDLE: libc::c_int = 3;
                let tid = libc::syscall(libc::SYS_gettid) as libc::c_int;
                let ioprio = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
                let ret = libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, tid, ioprio);
                if ret != 0 {
                    warn!("Failed to set IDLE I/O priority for healer thread: {}",
                        std::io::Error::last_os_error());
                }
            }
        })
        .enable_all()
        .build()?;

    let healing = std::sync::Arc::new(healing::HealingManager::new(
        storage,
        metadata,
        cluster,
        server.network_client(),
        server.replication_factor_handle(),
        config.replication.healing_delay_secs,
        config.replication.scrub_interval_hours,
        config.replication.auto_heal,
        server.chunk_map_ref(),
        server.fold_result_chunk_ids_ref(),
        server.chunk_generations_ref(),
        server.last_cluster_write_ms(),
        config.replication.link_bandwidth_mb.expect("resolved by load_or_migrate_healing_tuning"),
        config.replication.heal_max_pct.expect("resolved by load_or_migrate_healing_tuning"),
        config.replication.heal_max_concurrent.expect("resolved by load_or_migrate_healing_tuning"),
        config.replication.heal_max_concurrent_per_node.expect("resolved by load_or_migrate_healing_tuning"),
        config.replication.heal_transfer_timeout_secs.expect("resolved by load_or_migrate_healing_tuning"),
        server.compaction_quiescing_handle(),
    ));
    healer_runtime.spawn(healing.clone().start());
    server.set_healing_manager(healing.clone()).await;
    server.clone().start_compaction_loop();
    server.clone().start_patch_state_gc_loop();
    server.clone().start_patch_state_resume_sweep();
    server.clone().start_ops_tracker_loop();
    server.clone().start_metadata_dissemination_loop();
    server.clone().start_leader_forward_loop();
    server.clone().start_chunk_location_sync_loop();
    server.clone().start_metadata_gossip_loop();
    server.clone().start_metadata_healer_loop();
    server.clone().start_patch_fold_sweep_loop();
    server.clone().start_chunk_patch_locks_sweep_loop();
    server.clone().start_fold_lock_grants_sweep_loop();
    server.clone().start_patch_fold_rebroadcast_loop();
    server.clone().start_durability_flush_timer();
    server.clone().start_periodic_reconciliation_loop();
    server.clone().start_delete_drain_loop();
    server.clone().start_chunk_tombstone_cleanup_loop();
    info!("✓ Healing manager started");

    // Start network servers — a client-facing listener and a separate
    // peer-only listener (see network::PEER_PORT_OFFSET's doc comment for why
    // inter-node RPC traffic needs its own port, not just its own semaphore on
    // a shared port). Share both semaphores with the server before spawning.
    let mut net_server = network::NetworkServer::new(config.node.listen_addr, server.clone(), network::MAX_CONNECTIONS);
    server.set_conn_semaphore(net_server.conn_semaphore.clone()).await;

    let peer_capacity = std::env::var("DFS_RESERVED_PEER_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(network::RESERVED_PEER_CONNECTIONS);
    let mut peer_net_server = network::NetworkServer::new(
        network::peer_port_addr(config.node.listen_addr), server.clone(), peer_capacity,
    );
    server.set_peer_conn_semaphore(peer_net_server.conn_semaphore.clone(), peer_net_server.capacity()).await;

    server.clone().start_conn_pressure_watchdog();
    let mut server_handle = tokio::spawn(async move {
        let (client_result, peer_result) = tokio::join!(net_server.start(), peer_net_server.start());
        if let Err(e) = client_result {
            tracing::error!("Network server error: {}", e);
        }
        if let Err(e) = peer_result {
            tracing::error!("Peer network server error: {}", e);
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

    // Periodically refresh the address file. /tmp is subject to systemd-tmpfiles
    // cleanup (files untouched for 30 days get swept), and this process can run for
    // months without restarting — refresh the mtime (and recreate the file if it was
    // swept) so `dfs-admin`'s auto-discovery keeps working for the life of the server.
    {
        let addr_file = addr_file.clone();
        let listen_addr = config.node.listen_addr;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            interval.tick().await; // skip immediate tick; already wrote above
            loop {
                interval.tick().await;
                if let Err(e) = std::fs::write(&addr_file, listen_addr.to_string()) {
                    warn!("Failed to refresh address file {}: {}", addr_file, e);
                }
            }
        });
    }

    // Try to join cluster using both seed nodes AND persisted peers
    // This ensures any node can rejoin even if the seed node is down
    // peers.json lives next to config.toml (not under metadata_dir), so a data/metadata
    // format/reset doesn't also wipe the node's memory of the cluster it belongs to.
    let config_dir = config_path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let local_addr = peer_addr; // Use peer_addr (our advertised address) for self-filtering

    // Start with configured seed nodes
    let mut all_join_targets = config.cluster.seed_nodes.clone();

    // Load persisted peers (excluding our own address)
    match cluster::ClusterManager::load_persisted_peers(&config_dir).await {
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

        match join_cluster(server.clone(), &all_join_targets, &config_dir, local_addr).await {
            Ok(_) => {
                info!("✓ Successfully joined cluster");
                // Wake the chunk-location sync loop — now that we have a leader, push
                // our local locations immediately rather than waiting for the 30s poll.
                server.cluster().node_recovered_notify.notify_waiters();
                reconcile_replication_factor_with_leader(&server, &config_path).await;
            }
            Err(e) => warn!("Failed to join cluster: {}", e),
        }

        // Start periodic rejoin attempts in background
        start_periodic_rejoin(server.clone(), all_join_targets.clone(), config_dir.clone(), local_addr, config_path.clone()).await;
    } else {
        info!("No seed nodes or peers configured - running as standalone node");
    }

    // Offline-compaction request channel — see Server::offline_compaction_tx's
    // doc comment. start_compaction_loop sends on this (via the Server-side
    // handle wired below) when it's won the CompactionIntent race and every
    // peer is Online; the loop just below owns the actual pause/compact/resume
    // sequence since it's the only place that owns server_handle's lifecycle.
    let (offline_compaction_tx, mut offline_compaction_rx) =
        tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<bool>>();
    server.set_offline_compaction_channel(offline_compaction_tx).await;

    // Wait for either a clean shutdown signal, an unexpected network task exit, or
    // a planned-offline-compaction request. Listens for both SIGINT (Ctrl+C) and
    // SIGTERM (systemctl stop / kill). If the network task exits (via panic or
    // unexpected return) we exit immediately so the process manager
    // (systemd Restart=always) can bring us back up cleanly. A compaction request
    // loops back afterward instead of exiting — see run_planned_offline_compaction.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate()
    )?;
    let sig = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break "SIGINT",
            _ = sigterm.recv() => break "SIGTERM",
            Some(reply_tx) = offline_compaction_rx.recv() => {
                let succeeded = run_planned_offline_compaction(
                    &server, config.node.listen_addr, &mut server_handle,
                ).await;
                let _ = reply_tx.send(succeeded);
                // Loop back — keep listening for the real shutdown signals.
            }
            result = &mut server_handle => {
                match result {
                    Ok(()) => tracing::error!("Network server exited unexpectedly — listener is dead"),
                    Err(e) if e.is_panic() => tracing::error!("Network server panicked: {:?}", e),
                    Err(e) => tracing::error!("Network server task error: {}", e),
                }
                // Broadcast GracefulLeave so peers immediately elect a new leader
                // rather than waiting for the heartbeat timeout (30-120s).
                // Use a short timeout — if we can't reach peers we still need to exit.
                let _ = tokio::time::timeout(
                    tokio::time::Duration::from_millis(500),
                    server.cluster().announce_leaving(dfs_common::LeaveReason::Shutdown),
                ).await;
                let _ = std::fs::remove_file(&addr_file);
                std::process::exit(1);
            }
        }
    };
    {
            info!("Shutting down ({sig}) — broadcasting GracefulLeave to peers...");
            // Capture this BEFORE announce_leaving(), which marks our own node Leaving —
            // is_leader() would already read false afterward.
            let was_leader = server.cluster().is_leader().await;
            server.cluster().announce_leaving(dfs_common::LeaveReason::Shutdown).await;
            // Brief pause so peers process the broadcast before we close connections.
            // 100ms is sufficient — announce_leaving() already awaits the TCP sends.
            // Keeping this short avoids racing with test-suite teardown (pkill + sleep 0.5).
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            // If we were the leader, stay up a bit longer so the listener keeps answering
            // requests instead of going dark. Every leader-affinity handler already checks
            // is_leader() and returns NotLeader{leader_addr} — and since announce_leaving()
            // above already flipped our own status to Leaving, get_leader_addr() already
            // resolves to the successor (next-lowest-ID online peer) with no election
            // round-trip needed. The client's send_to_leader_with_retry redirects on that
            // response immediately with zero backoff — so the only thing standing between
            // a client and the successor is whether our port is still open when it retries.
            // Defaults to 2000ms — safe for real clients, NOT zero.
            //
            // This previously defaulted to 0 with a comment saying staging/production
            // "should set" it via the systemd unit. Nobody did: on 2026-07-22 all five
            // staging nodes had it unset, and a rolling restart performed while a VM was
            // running produced client stalls of 4.7s, 12.8s and 27.3s, a guest SCSI
            // timeout, and an I/O error mid-install. The client log across that whole
            // window contained ZERO NotLeader responses, ZERO NodeLeaving responses, and
            // ZERO leader-change events — the entire graceful-handoff path was never
            // exercised, because with grace 0 the leader is gone before it can answer the
            // retry that would have carried the redirect. The client just sat on its 30s
            // send/receive timeout instead.
            //
            // A default that only works when an operator remembers to override it is not a
            // default; the safe value belongs here and the fast-teardown value belongs with
            // the thing that wants fast teardown (the local suite exports 0 explicitly).
            // Cost when it fires is ~2s of extra shutdown on the ONE node that was leader,
            // against clients otherwise blocking for tens of seconds.
            if was_leader {
                let grace_ms = std::env::var("DFS_LEADER_HANDOFF_GRACE_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2000);
                if grace_ms > 0 {
                    info!("Was leader — staying up {}ms to redirect clients to the successor before exiting", grace_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(grace_ms)).await;
                }
            }
            // Drain the sled-write worker: commits any queued PutFileMetadata writes
            // before the process exits. Without this, writes queued but not yet committed
            // are lost on restart (the worker std::thread is killed when main() returns).
            let _ = tokio::time::timeout(
                tokio::time::Duration::from_secs(5),
                server.drain_sled_writes(),
            ).await;
            info!("Shutting down...");
            server_handle.abort();
            // Clean up addr file so dfs-admin auto-discovery doesn't see a stale entry.
            // Must happen here, before exit — std::process::exit() below terminates the
            // process immediately and skips any code after the select! block entirely.
            let _ = std::fs::remove_file(&addr_file);
            // Exit immediately rather than returning normally from main() and letting
            // tokio's default runtime-drop behavior take over — that waits for every
            // in-flight spawn_blocking task to finish before the process can exit, with
            // no timeout of its own. All of our own explicit graceful-shutdown steps
            // (GracefulLeave broadcast, leader handoff grace, drain_sled_writes) have
            // already completed by this point via awaited, bounded calls above — the
            // only thing std::process::exit skips is that unbounded wait on whatever
            // else might still be running in the background (e.g. an in-flight
            // metadata compaction), which is exactly what should NOT be allowed to
            // block a shutdown that has already done its own cleanup.
            //
            // Root-caused 2026-07-11: a wedged compaction (see compact_db_prepare's
            // now-added Phase 1-2 timeout) meant a `systemctl restart` hung the full
            // 90s systemd TimeoutStopSec waiting on exactly this before SIGKILL forced
            // it — during which the node was unreachable, cascading into a real
            // healing backlog once it came back. The Phase 1-2 timeout prevents this
            // specific trigger from recurring, but this exit fixes the general case:
            // shutdown should never be held hostage by an unrelated stuck background
            // task once our own explicit cleanup is done.
            std::process::exit(0);
    }
    // Every path that can reach here exits the process directly
    // (std::process::exit), so nothing after this point ever runs — including
    // healer_runtime's drop, which is fine: process exit doesn't run Drop glue
    // anyway. The only loop-back path (a planned offline compaction request)
    // stays inside the `sig = loop { ... }` above and never reaches this point.
}

/// Pause serving (listener + heartbeats), run a full offline metadata
/// compaction with zero concurrent traffic, then resume — see
/// LeaveReason::PlannedCompaction's doc comment for why this is safe to do
/// routinely rather than only at deploy time. Returns whether compaction
/// itself succeeded (the node always attempts to come back online regardless,
/// even on failure — going offline must never turn into staying offline).
///
/// Takes server_handle by `&mut` rather than by value so the caller's select!
/// loop (which also needs `&mut server_handle` for its own "network task
/// exited" arm) doesn't have an ownership conflict — abort()/await only need a
/// reference, and the handle is replaced in place once the new listener task
/// is spawned.
async fn run_planned_offline_compaction(
    server: &std::sync::Arc<server::Server>,
    listen_addr: std::net::SocketAddr,
    server_handle: &mut tokio::task::JoinHandle<()>,
) -> bool {
    info!("Planned offline compaction: pausing to compact with no concurrent traffic...");
    // Same reasoning as the real Shutdown path: capture before announce_leaving()
    // flips our own status (is_leader() would already read false afterward).
    let was_leader = server.cluster().is_leader().await;
    server.cluster().announce_leaving(dfs_common::LeaveReason::PlannedCompaction).await;
    server.cluster().pause_heartbeats();
    // Brief pause so peers process the broadcast before we close connections —
    // same 100ms as the real shutdown path's identical comment.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    if was_leader {
        let grace_ms = std::env::var("DFS_LEADER_HANDOFF_GRACE_MS")
            .ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        if grace_ms > 0 {
            info!("Was leader — staying up {}ms to redirect clients to the successor before pausing", grace_ms);
            tokio::time::sleep(tokio::time::Duration::from_millis(grace_ms)).await;
        }
    }
    let _ = tokio::time::timeout(
        tokio::time::Duration::from_secs(5),
        server.drain_sled_writes(),
    ).await;

    server_handle.abort();
    let _ = (&mut *server_handle).await; // wait for the listener task to actually stop before rebinding

    // Give the OS a moment to fully release the socket before rebinding — this
    // process has never rebound its own listen_addr mid-run before today, so
    // there's no existing precedent to lean on for how long that takes.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Quiesce compaction_quiescing-respecting background writers (fold_slot_now,
    // HealingManager) for the duration of compact_db() below — without this, they
    // keep running independently of the listener/heartbeat pause just performed above
    // (they're driven by their own timers/backlogs, not client RPCs), so the "zero
    // concurrent traffic" this function's whole premise rests on was never actually
    // true for internal writers, only external ones.
    //
    // Root-caused 2026-07-15 (gluster1 incident): "Planned offline compaction failed:
    // ... sustained write churn" fired repeatedly immediately after the listener was
    // already paused — self-inflicted churn from this node's own unpaused healing and
    // patch-fold-sweep traffic, not real external load. compact_db_prepare's Phase 2
    // catch-up can't converge against a live db that this process itself keeps writing
    // to. server.rs's online-path compaction already quiesces around its own Phase 3 —
    // this path just never had the equivalent, since it calls compact_db() directly
    // rather than going through that loop.
    server.begin_compaction_quiesce();
    let metadata = server.metadata_store();
    // compact_db_blocking (redb's own compact()), NOT compact_db (the shadow-copy
    // rebuild). The shadow copy exists solely so the ONLINE path can compact without
    // blocking live writers — it copies every table into a fresh db under a shared
    // lock and swaps it in. None of that applies here: this node has already left the
    // cluster, paused its listener, drained its writes and quiesced its internal
    // writers, so it can simply take the exclusive lock and let redb do it properly.
    //
    // And redb does do it better. Measured 2026-07-16
    // (fragmented_bytes_stays_high_even_at_redbs_own_compaction_floor): on the same
    // churned db, the shadow copy got 14.51MB -> 4.53MB, and redb's compact() then
    // took that same file a further 22% down to 3.52MB — space the shadow rebuild
    // left on the floor, because building a B-tree by insertion re-fragments it as
    // pages split, whereas compact() relocates live pages and truncates. It also
    // converges (a second pass is a no-op), so there's no iteration to manage.
    //
    // Wrapped in the same 60s wedge-detection timeout every other compact_db_blocking/
    // compact_db_prepare call site in this codebase already carries (server.rs's periodic
    // loop: 60s around compact_db_prepare, 20s around compact_db_finish, 60s around its own
    // compact_db_blocking escalation) — this was the one call site that didn't have one.
    // Root-caused 2026-07-31 (gluster1): this call wedged holding MetadataStore's exclusive
    // lock with nothing watching it; the node only came back because a LATER, unrelated
    // periodic-compaction-loop iteration queued behind the same lock and its OWN 60s
    // Phase 1-2 timeout fired first, killing the whole process out from under this still-
    // stuck task ~3 minutes in — an incidental save, not a designed one. Every peer had
    // already marked this node Failed and evicted it from the ring by the time that
    // happened. Same remedy as every other site: if the exclusive lock is still wedged
    // after 60s, there is no way to un-stick it from here — restart so HA replicas keep
    // serving and a fresh process gets a clean redb handle.
    let compact_task = tokio::task::spawn_blocking(move || metadata.compact_db_blocking());
    let compact_result = match tokio::time::timeout(std::time::Duration::from_secs(60), compact_task).await {
        Ok(r) => r,
        Err(_) => {
            error!(
                "Planned offline compaction: compact_db_blocking() exceeded 60s — exclusive \
                 metadata write lock is wedged on this node (already offline, out of the \
                 cluster ring). Restarting so HA replicas can continue serving."
            );
            std::process::exit(1);
        }
    };
    server.end_compaction_quiesce();
    let succeeded = match &compact_result {
        Ok(Ok((before, after))) => {
            info!("Planned offline compaction finished: {:.1}MB -> {:.1}MB",
                *before as f64 / 1_048_576.0, *after as f64 / 1_048_576.0);
            true
        }
        Ok(Err(e)) => { warn!("Planned offline compaction failed: {}", e); false }
        Err(e) => { warn!("Planned offline compaction task panicked: {}", e); false }
    };

    // The drain above permanently stopped the metadata persist worker (that's what
    // a drain IS — see drain_sled_writes) — respawn it BEFORE anything can accept
    // or generate new metadata writes. Without this, every PutFileMetadata after a
    // planned offline compaction was acked and silently never persisted (2026-07-16
    // root cause; see Server::restart_sled_writes' doc comment for the fallout).
    server.restart_sled_writes();

    // Rebind and come back online regardless of compaction's own outcome —
    // going offline must never turn into staying offline. Same dual-listener
    // shape as the initial startup path above.
    let mut net_server = network::NetworkServer::new(listen_addr, server.clone(), network::MAX_CONNECTIONS);
    server.set_conn_semaphore(net_server.conn_semaphore.clone()).await;

    let peer_capacity = std::env::var("DFS_RESERVED_PEER_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(network::RESERVED_PEER_CONNECTIONS);
    let mut peer_net_server = network::NetworkServer::new(
        network::peer_port_addr(listen_addr), server.clone(), peer_capacity,
    );
    server.set_peer_conn_semaphore(peer_net_server.conn_semaphore.clone(), peer_net_server.capacity()).await;

    *server_handle = tokio::spawn(async move {
        let (client_result, peer_result) = tokio::join!(net_server.start(), peer_net_server.start());
        if let Err(e) = client_result {
            tracing::error!("Network server error (post-compaction respawn): {}", e);
        }
        if let Err(e) = peer_result {
            tracing::error!("Peer network server error (post-compaction respawn): {}", e);
        }
    });
    // Catch up on anything that landed on the interim leader while we were paused,
    // BEFORE resume_heartbeats() lets peers see us as Online again — is_leader() is
    // purely min(NodeId) among Online peers (cluster.rs), recomputed instantly on
    // every node the moment our heartbeat resumes, with no gate of its own. Without
    // this, a lowest-NodeId node reclaims leadership (and starts being read from)
    // before it has any idea what changed while it was gone.
    //
    // Root-caused 2026-07-15 (gluster1 incident): this function already paused
    // external traffic correctly, but resumed heartbeats — and thus reclaimed
    // leadership eligibility — with zero synchronization against the interim
    // leader's state. A client wrote and closed a 128MB file entirely against the
    // interim leader (10.25.1.64) while this node was paused; this node came back
    // "Online" and was immediately treated as leader again by every node's own
    // is_leader() computation, 6+ seconds before start_metadata_dissemination_loop's
    // own background catch-up (spawned on its own 5s poll cadence, fire-and-forget)
    // happened to land — during that window a client read of the just-written file
    // returned ENOENT against this node. run_metadata_catchup already existed
    // (pulls anything followers have that we don't, then pushes back anything they're
    // missing) but was only ever invoked asynchronously after the fact; calling it
    // synchronously here, before we're visible as Online, closes the gap directly
    // instead of just narrowing the race window.
    let catchup_result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        server.run_metadata_catchup(),
    ).await;
    if catchup_result.is_err() {
        warn!("Planned offline compaction: metadata catch-up timed out after 30s — \
               resuming anyway (dissemination loop will cover remaining gaps), but a \
               brief stale-read window is possible");
    }

    server.cluster().resume_heartbeats();
    server.cluster().announce_recovery().await;
    info!("Planned offline compaction: back online");

    succeeded
}

/// Attempt to join cluster via seed nodes
async fn join_cluster(
    server: std::sync::Arc<server::Server>,
    seed_nodes: &[std::net::SocketAddr],
    config_dir: &std::path::Path,
    local_addr: std::net::SocketAddr,
) -> Result<()> {
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

    // Try every seed/peer rather than stopping at the first that answers. Each one
    // may hold a different, incomplete view of the cluster (e.g. it hasn't heard
    // about a node that's mid-restart yet) — stopping early meant we'd persist
    // whichever partial view we got first, silently and permanently dropping any
    // node absent from it: it never ends up in the saved peer list to retry, and
    // the failure detector only probes nodes it already knows about, not ones it
    // never learned of at all.
    let mut all_learned_nodes: std::collections::HashMap<dfs_common::types::NodeId, dfs_common::types::NodeInfo> =
        std::collections::HashMap::new();
    let mut joined_via: std::collections::HashSet<std::net::SocketAddr> = std::collections::HashSet::new();
    let mut last_err = None;

    for seed_addr in &unique_seeds {
        match send_join_request(*seed_addr, &server).await {
            Ok(cluster_nodes) => {
                info!("✓ Successfully joined cluster via {} - learned about {} total nodes",
                    seed_addr, cluster_nodes.len());
                for node in cluster_nodes {
                    all_learned_nodes.entry(node.id).or_insert(node);
                }
                joined_via.insert(*seed_addr);
            }
            Err(e) => {
                debug!("Failed to join via {}: {}", seed_addr, e);
                last_err = Some(e);
            }
        }
    }

    if joined_via.is_empty() {
        return Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("Failed to join cluster - all {} seed/peer nodes unreachable", unique_seeds.len())
        }));
    }

    // Save the union of peers learned across all successful joins (excluding self)
    let peer_addrs: Vec<std::net::SocketAddr> = all_learned_nodes
        .values()
        .map(|n| n.addr)
        .filter(|addr| *addr != local_addr)
        .collect();

    if !peer_addrs.is_empty() {
        if let Err(e) = cluster::ClusterManager::save_persisted_peers(&peer_addrs, config_dir).await {
            warn!("Failed to save persisted peers: {}", e);
        }
    }

    // Announce ourselves to every peer we learned about, except the ones we joined
    // through directly (they already know about us from the join request itself).
    let all_nodes: Vec<_> = all_learned_nodes.into_values().collect();
    announce_to_peers(&server, &all_nodes, &joined_via).await;

    Ok(())
}

/// Closes the RF cross-node divergence gap: `replication_factor` is otherwise read
/// independently from each node's own local config.toml with no consistency check
/// (unlike healing-tuning knobs, RF also drives the write-path immediate-replica count,
/// so a node that silently kept a stale value could disagree with the rest of the
/// cluster about write durability, not just healing classification). Called after every
/// successful join/rejoin — including periodic rejoin after a partition — so a node
/// that was unreachable during a `dfs-admin cluster set --replication-factor` change
/// self-heals to the cluster's value instead of requiring the operator to notice and
/// re-run the command.
///
/// Deliberately does NOT special-case "am I the leader" and trust my own value in that
/// case — leadership is purely "online node with the lowest NodeId" (see README's
/// Leader Election section), which has no relationship to whether this node's own
/// config is fresh. A node that was down during an RF change can easily reclaim
/// leadership within seconds of rejoining if its NodeId happens to be the lowest
/// online — an earlier version of this function skipped reconciliation whenever
/// `is_leader()` was true and consequently never adopted the cluster's actual value in
/// exactly that case (caught by test T45g/h/i in test_local_suite.sh). Querying every
/// reachable peer and taking a majority vote works regardless of this node's own
/// leadership status.
async fn reconcile_replication_factor_with_leader(
    server: &std::sync::Arc<server::Server>,
    config_path: &std::path::Path,
) {
    let local_rf = server.replication_factor_handle().load(std::sync::atomic::Ordering::Relaxed);

    let local_id = server.cluster().local_node_id();
    let peers: Vec<_> = server.cluster().get_all_nodes().await
        .into_iter()
        .filter(|n| n.id != local_id)
        .collect();

    if peers.is_empty() {
        debug!("RF reconciliation: no peers known yet, skipping");
        return;
    }

    let mut votes: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut reached = 0usize;
    for peer in &peers {
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.network_client().send_message(peer.addr, dfs_common::Message::Request(dfs_common::Request::GetClusterStatus)),
        ).await;
        if let Ok(Ok(envelope)) = resp {
            if let dfs_common::Message::Response(dfs_common::Response::ClusterStatus { replication_factor, .. }) = envelope.message {
                *votes.entry(replication_factor).or_insert(0) += 1;
                reached += 1;
            }
        }
    }

    if reached == 0 {
        debug!("RF reconciliation: could not reach any peer, skipping");
        return;
    }

    // Majority among reached peers. A tie (or no value held by >50%) isn't enough
    // signal to override our own value, so leave it alone rather than guess.
    let Some((&majority_rf, &majority_count)) = votes.iter().max_by_key(|(_, c)| **c) else {
        return;
    };
    if majority_count * 2 <= reached || majority_rf == local_rf {
        return;
    }

    warn!(
        "Local replication_factor ({}) is stale vs. cluster majority ({}/{} reachable peers report {}) — adopting and persisting to config",
        local_rf, majority_count, reached, majority_rf
    );
    server.replication_factor_handle().store(majority_rf, std::sync::atomic::Ordering::Relaxed);

    match Config::from_file(config_path) {
        Ok(mut config) => {
            config.replication.replication_factor = majority_rf;
            if let Err(e) = config.to_file(config_path) {
                warn!("RF reconciliation: failed to persist adopted replication_factor to config {:?}: {}", config_path, e);
            }
        }
        Err(e) => warn!("RF reconciliation: failed to reload config {:?} to persist adopted replication_factor: {}", config_path, e),
    }
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

    // Connect to seed node's peer port — this is genuine inter-node cluster
    // traffic (see network::PEER_PORT_OFFSET's doc comment), not a client request.
    let mut stream = TcpStream::connect(crate::network::peer_port_addr(seed_addr)).await?;

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
    config_dir: std::path::PathBuf,
    local_addr: std::net::SocketAddr,
    config_path: std::path::PathBuf,
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

                match join_cluster(server.clone(), &join_targets, &config_dir, local_addr).await {
                    Ok(_) => {
                        info!("✓ Successfully rejoined cluster via periodic retry");
                        server.cluster().node_recovered_notify.notify_waiters();
                        reconcile_replication_factor_with_leader(&server, &config_path).await;
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
    joined_via: &std::collections::HashSet<std::net::SocketAddr>,
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

    // Announce to all peers except ourselves and the ones we joined through directly
    for peer in cluster_nodes {
        if peer.id == local_node_id || joined_via.contains(&peer.addr) {
            continue;
        }

        info!("Announcing to peer {}", peer.addr);

        // Spawn announcement in background - don't block on failures
        let peer_addr = crate::network::peer_port_addr(peer.addr);
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
