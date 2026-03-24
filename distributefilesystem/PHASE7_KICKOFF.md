# Phase 7 Kickoff: Admin Tools

## Quick Status Check

**Current State:** Phase 6 COMPLETE ✅
**Next Task:** Phase 7 - Admin CLI
**Goal:** Management CLI for cluster operations
**Estimated Effort:** 1-2 days

---

## What's Already Done (Phases 1-6)

✅ **Fully functional distributed filesystem:**
- Backend storage with 3x replication
- Self-healing with 300s delay
- FUSE client with full read/write support
- Mount/unmount capability
- All core features working

✅ **Current binaries:**
- `dfs-server` - Storage node daemon (fully functional)
- `dfs-client` - FUSE mount client (fully functional)
- `dfs-admin` - Admin CLI (placeholder - THIS IS PHASE 7)

✅ **Code stats:**
- ~5,700 lines of Rust
- 28 tests passing
- All core functionality complete

---

## Phase 7: Admin Tools Overview

### Goal
Create a management CLI (`dfs-admin`) for cluster operations, monitoring, and troubleshooting.

### Why This Matters
- Operators need visibility into cluster health
- Manual operations (scrub, rebalance) need triggers
- Troubleshooting requires file/chunk inspection
- Automation requires scriptable interface (JSON output)

---

## What to Build

### 1. Cluster Management Commands

```bash
# Show cluster status (nodes, health, connectivity)
dfs-admin cluster status

# Detailed cluster statistics
dfs-admin cluster info

# List all nodes with details
dfs-admin node list

# Add node to cluster (optional - normally joins via config)
dfs-admin node add <addr>

# Remove node (marks for decommission)
dfs-admin node remove <id>
```

### 2. Storage Management Commands

```bash
# Show total storage, usage, available space
dfs-admin storage stats

# Trigger manual rebalance (after adding/removing nodes)
dfs-admin storage rebalance

# Trigger manual scrub (checksum verification)
dfs-admin storage scrub

# Verify specific chunk integrity
dfs-admin storage verify <chunk_id>
```

### 3. Healing Management Commands

```bash
# Show pending healing operations
dfs-admin healing status

# Enable/disable auto-healing
dfs-admin healing enable
dfs-admin healing disable

# Force immediate healing check (bypass 300s delay)
dfs-admin healing trigger
```

### 4. File Inspection Commands

```bash
# Show file metadata, chunks, locations
dfs-admin file info <path>

# List all chunks for a file
dfs-admin file chunks <path>

# Show where a chunk is replicated
dfs-admin file replicas <chunk_id>
```

### 5. Global Options

```bash
# All commands support:
--cluster <node1:port,node2:port>  # Cluster nodes
--json                              # JSON output for scripting
--verbose                           # Detailed output
```

---

## Implementation Plan

### File to Create/Modify

**Main file:** `dfs-admin/src/main.rs` (currently just "Hello, world!")

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    dfs-admin CLI                             │
│                                                               │
│  - Parse commands (clap)                                     │
│  - Connect to any cluster node                               │
│  - Send admin requests                                       │
│  - Format output (tables or JSON)                            │
└────────────────────┬────────────────────────────────────────┘
                     │ TCP + bincode (same as client)
                     ▼
┌─────────────────────────────────────────────────────────────┐
│              DFS Cluster (any node)                          │
│                                                               │
│  Handles admin requests:                                     │
│  - Query cluster state                                       │
│  - Trigger operations (scrub, heal, rebalance)              │
│  - Inspect files/chunks                                      │
└─────────────────────────────────────────────────────────────┘
```

### Implementation Steps

#### Step 1: Protocol Extensions
Add new request types to `dfs-common/src/protocol.rs`:

```rust
pub enum Request {
    // ... existing requests ...

    // Cluster management
    GetClusterStatus,
    GetClusterInfo,
    GetNodeList,
    AddNode { addr: SocketAddr },
    RemoveNode { node_id: NodeId },

    // Storage management
    GetStorageStats,
    TriggerRebalance,
    TriggerScrub,
    VerifyChunk { chunk_id: ChunkId },

    // Healing management
    GetHealingStatus,
    EnableHealing,
    DisableHealing,
    TriggerHealing,

    // File inspection
    GetFileInfo { path: String },
    GetFileChunks { path: String },
    GetChunkReplicas { chunk_id: ChunkId },
}

pub enum Response {
    // ... existing responses ...

    ClusterStatus {
        nodes: Vec<NodeStatus>,
        total_capacity: u64,
        used_capacity: u64,
    },

    StorageStats {
        total_chunks: usize,
        total_size: u64,
        replication_factor: usize,
    },

    HealingStatus {
        pending_count: usize,
        in_progress: Vec<ChunkId>,
        last_check: u64,
    },

    FileInfo {
        metadata: FileMetadata,
        chunk_locations: Vec<ChunkLocation>,
    },

    ChunkReplicas {
        chunk_id: ChunkId,
        nodes: Vec<NodeId>,
    },

    // ... etc
}

pub struct NodeStatus {
    pub id: NodeId,
    pub addr: SocketAddr,
    pub status: NodeHealthStatus,
    pub last_seen: u64,
    pub chunks_stored: usize,
}

pub enum NodeHealthStatus {
    Healthy,
    Degraded,
    Failed,
}
```

#### Step 2: Server Handlers
Add handlers in `dfs-server/src/server.rs`:

```rust
impl Server {
    pub async fn handle_request(&self, request: Request) -> Response {
        match request {
            // ... existing handlers ...

            Request::GetClusterStatus => self.handle_get_cluster_status().await,
            Request::GetStorageStats => self.handle_get_storage_stats().await,
            Request::GetHealingStatus => self.handle_get_healing_status().await,
            Request::GetFileInfo { path } => self.handle_get_file_info(path).await,
            // ... etc
        }
    }

    async fn handle_get_cluster_status(&self) -> Response {
        let nodes = self.cluster.get_all_nodes().await;
        let mut statuses = Vec::new();

        for node in nodes {
            let status = NodeStatus {
                id: node.id,
                addr: node.addr,
                status: self.cluster.get_node_health(&node.id).await,
                last_seen: self.cluster.get_last_heartbeat(&node.id).await,
                chunks_stored: 0, // TODO: implement
            };
            statuses.push(status);
        }

        Response::ClusterStatus {
            nodes: statuses,
            total_capacity: 0, // TODO: calculate
            used_capacity: 0,  // TODO: calculate
        }
    }

    // ... implement other handlers
}
```

#### Step 3: Admin Client
Create admin client in `dfs-admin/src/main.rs`:

```rust
use clap::{Parser, Subcommand};
use anyhow::Result;
use dfs_common::{Message, Request, Response};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Parser)]
#[command(name = "dfs-admin")]
#[command(about = "DFS cluster administration tool")]
struct Cli {
    /// Cluster nodes (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    cluster: Vec<String>,

    /// Output format
    #[arg(long)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Cluster management
    Cluster {
        #[command(subcommand)]
        cmd: ClusterCommands,
    },

    /// Storage management
    Storage {
        #[command(subcommand)]
        cmd: StorageCommands,
    },

    /// Healing management
    Healing {
        #[command(subcommand)]
        cmd: HealingCommands,
    },

    /// File inspection
    File {
        #[command(subcommand)]
        cmd: FileCommands,
    },

    /// Node management
    Node {
        #[command(subcommand)]
        cmd: NodeCommands,
    },
}

#[derive(Subcommand)]
enum ClusterCommands {
    /// Show cluster status
    Status,
    /// Show detailed cluster info
    Info,
}

#[derive(Subcommand)]
enum StorageCommands {
    /// Show storage statistics
    Stats,
    /// Trigger rebalance
    Rebalance,
    /// Trigger scrub
    Scrub,
    /// Verify specific chunk
    Verify { chunk_id: String },
}

#[derive(Subcommand)]
enum HealingCommands {
    /// Show healing status
    Status,
    /// Enable auto-healing
    Enable,
    /// Disable auto-healing
    Disable,
    /// Trigger immediate healing
    Trigger,
}

#[derive(Subcommand)]
enum FileCommands {
    /// Show file information
    Info { path: String },
    /// List file chunks
    Chunks { path: String },
    /// Show chunk replicas
    Replicas { chunk_id: String },
}

#[derive(Subcommand)]
enum NodeCommands {
    /// List all nodes
    List,
    /// Add node
    Add { addr: String },
    /// Remove node
    Remove { node_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Connect to any cluster node
    let addr = parse_cluster_addr(&cli.cluster)?;

    // Execute command
    match cli.command {
        Commands::Cluster { cmd } => handle_cluster_command(cmd, addr, cli.json).await?,
        Commands::Storage { cmd } => handle_storage_command(cmd, addr, cli.json).await?,
        Commands::Healing { cmd } => handle_healing_command(cmd, addr, cli.json).await?,
        Commands::File { cmd } => handle_file_command(cmd, addr, cli.json).await?,
        Commands::Node { cmd } => handle_node_command(cmd, addr, cli.json).await?,
    }

    Ok(())
}

async fn handle_cluster_command(cmd: ClusterCommands, addr: SocketAddr, json: bool) -> Result<()> {
    match cmd {
        ClusterCommands::Status => {
            let response = send_request(addr, Request::GetClusterStatus).await?;

            match response {
                Response::ClusterStatus { nodes, total_capacity, used_capacity } => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&nodes)?);
                    } else {
                        print_cluster_status(&nodes, total_capacity, used_capacity);
                    }
                }
                _ => eprintln!("Unexpected response"),
            }
        }
        ClusterCommands::Info => {
            // TODO: implement
        }
    }

    Ok(())
}

fn print_cluster_status(nodes: &[NodeStatus], total: u64, used: u64) {
    println!("DFS Cluster Status");
    println!("==================");
    println!("Total Capacity: {} GB", total / (1024 * 1024 * 1024));
    println!("Used Capacity:  {} GB", used / (1024 * 1024 * 1024));
    println!("Available:      {} GB", (total - used) / (1024 * 1024 * 1024));
    println!();
    println!("Nodes:");
    println!("{:<40} {:<15} {:<10} {:<20}", "ID", "Address", "Status", "Last Seen");
    println!("{}", "-".repeat(85));

    for node in nodes {
        println!("{:<40} {:<15} {:<10} {}",
                 node.id.to_string(),
                 node.addr.to_string(),
                 format!("{:?}", node.status),
                 format_timestamp(node.last_seen));
    }
}

async fn send_request(addr: SocketAddr, request: Request) -> Result<Response> {
    let mut stream = TcpStream::connect(addr).await?;

    let message = Message::Request(request);
    let encoded = bincode::serialize(&message)?;

    // Send with length prefix
    stream.write_u32(encoded.len() as u32).await?;
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    // Read response
    let len = stream.read_u32().await? as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let response_message: Message = bincode::deserialize(&buf)?;

    match response_message {
        Message::Response(response) => Ok(response),
        _ => anyhow::bail!("Expected response"),
    }
}

// ... implement other command handlers
```

#### Step 4: Pretty Output
Add nice formatting (optional but recommended):

```rust
// Use these crates in Cargo.toml:
// prettytable-rs = "0.10"  // For tables
// colored = "2.0"          // For colors
// humantime = "2.1"        // For time formatting
// bytesize = "1.3"         // For size formatting

use prettytable::{Table, row, cell};
use colored::*;
use humantime::format_duration;
use bytesize::ByteSize;

fn print_cluster_status_pretty(nodes: &[NodeStatus]) {
    let mut table = Table::new();
    table.add_row(row!["Node ID", "Address", "Status", "Last Seen", "Chunks"]);

    for node in nodes {
        let status_str = match node.status {
            NodeHealthStatus::Healthy => "✓ Healthy".green(),
            NodeHealthStatus::Degraded => "⚠ Degraded".yellow(),
            NodeHealthStatus::Failed => "✗ Failed".red(),
        };

        table.add_row(row![
            node.id.to_string()[..8].to_string(),
            node.addr,
            status_str,
            format_timestamp(node.last_seen),
            node.chunks_stored
        ]);
    }

    table.printstd();
}
```

---

## Dependencies to Add

Add to `dfs-admin/Cargo.toml`:

```toml
[dependencies]
dfs-common = { path = "../dfs-common" }
tokio = { workspace = true }
clap = { workspace = true }
serde = { workspace = true }
serde_json = "1.0"
bincode = { workspace = true }
anyhow = { workspace = true }

# Optional for pretty output
prettytable-rs = "0.10"
colored = "2.1"
humantime = "2.1"
bytesize = "1.3"
```

---

## Testing Strategy

### Manual Testing
```bash
# Start a server
cd dfs-server
cargo run -- start --config ../config.example.toml

# In another terminal, test admin commands
cd dfs-admin
cargo run -- --cluster 127.0.0.1:8900 cluster status
cargo run -- --cluster 127.0.0.1:8900 storage stats
cargo run -- --cluster 127.0.0.1:8900 healing status
```

### Commands to Test
1. ✅ `cluster status` - should show 1 node
2. ✅ `storage stats` - should show chunk count
3. ✅ `healing status` - should show healing state
4. ✅ `node list` - should list nodes
5. ✅ `file info /test.txt` - inspect file (if exists)

---

## Success Criteria

- [ ] All cluster commands working
- [ ] All storage commands working
- [ ] All healing commands working
- [ ] All file inspection commands working
- [ ] JSON output option works
- [ ] Error handling is robust
- [ ] Help text is clear
- [ ] Pretty output looks good

---

## Estimated Time Breakdown

1. **Protocol extensions** - 30 min
2. **Server handlers** - 2 hours
3. **Admin CLI structure** - 1 hour
4. **Command implementations** - 3 hours
5. **Pretty output** - 1 hour
6. **Testing** - 1 hour

**Total: ~8 hours (1 day)**

---

## Key Files to Reference

- `dfs-common/src/protocol.rs` - Add request/response types
- `dfs-server/src/server.rs` - Add request handlers
- `dfs-server/src/cluster.rs` - Query cluster state
- `dfs-server/src/healing.rs` - Query healing state
- `dfs-server/src/storage.rs` - Query storage stats
- `dfs-server/src/metadata.rs` - Query file info
- `dfs-admin/src/main.rs` - Implement CLI

---

## After Phase 7

**Phase 8:** Testing & Refinement (integration tests, stress tests)
**Phase 9:** Performance optimization (profiling, benchmarks)
**Phase 10:** Production features (systemd, monitoring, optional features)

---

## Notes

- Admin tool is **not critical** for basic usage
- It's primarily for **operations and troubleshooting**
- Can be minimal (just cluster status) or extensive (all commands)
- JSON output is important for automation
- Nice formatting improves UX but is optional

**You can skip Phase 7 and go straight to testing if you want!**

---

## Quick Start Commands (Next Session)

```bash
# Check current state
cd /home/petelombardo/distributefilesystem
git log --oneline -5
cargo test  # Should pass 28/28

# Read this file
cat PHASE7_KICKOFF.md

# Start implementing
cd dfs-admin
# Edit src/main.rs
# Add protocol types to dfs-common/src/protocol.rs
# Add handlers to dfs-server/src/server.rs

# Test as you go
cargo build --bin dfs-admin
cargo run --bin dfs-admin -- --help
```

---

**Current Status:** Phase 6 complete, ready for Phase 7!
**All code committed:** Yes ✅
**Tests passing:** 28/28 ✅
**Ready to go:** YES! 🚀
