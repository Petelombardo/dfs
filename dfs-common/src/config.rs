use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use crate::NodeId;

/// Main configuration for a DFS node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Node configuration
    pub node: NodeConfig,

    /// Storage configuration
    pub storage: StorageConfig,

    /// Cluster configuration
    pub cluster: ClusterConfig,

    /// Replication configuration
    pub replication: ReplicationConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node: NodeConfig::default(),
            storage: StorageConfig::default(),
            cluster: ClusterConfig::default(),
            replication: ReplicationConfig::default(),
        }
    }
}

/// Node-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Address this node listens on
    pub listen_addr: SocketAddr,

    /// Address this node advertises to other nodes (optional)
    /// If not set, will auto-detect from incoming connections
    /// Use this to override auto-detection or when behind NAT
    #[serde(default)]
    pub advertise_addr: Option<SocketAddr>,

    /// Persistent node identity. Generated once on first start and written back
    /// to this file. Subsequent restarts read it here — the config is the single
    /// source of truth for node identity.
    #[serde(default)]
    pub node_id: Option<NodeId>,

    /// Optional human-readable name (defaults to hostname)
    pub name: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8900".parse().unwrap(),
            advertise_addr: None,
            node_id: None,
            name: None,
        }
    }
}

/// Storage paths and settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Path to store data chunks
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Path to store metadata
    #[serde(default = "default_metadata_dir")]
    pub metadata_dir: PathBuf,

    /// Chunk size in megabytes (default: 4MB)
    #[serde(default = "default_chunk_size_mb")]
    pub chunk_size_mb: usize,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/dfs/data")
}

fn default_metadata_dir() -> PathBuf {
    PathBuf::from("/var/lib/dfs/metadata")
}

fn default_chunk_size_mb() -> usize {
    4
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            metadata_dir: default_metadata_dir(),
            chunk_size_mb: default_chunk_size_mb(),
        }
    }
}

/// Cluster membership and discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterConfig {
    /// Seed nodes to join on startup (bootstrap)
    #[serde(default)]
    pub seed_nodes: Vec<SocketAddr>,

    /// Heartbeat interval in seconds
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// Node failure detection timeout in seconds
    #[serde(default = "default_failure_timeout")]
    pub failure_timeout_secs: u64,
}

fn default_heartbeat_interval() -> u64 {
    10
}

fn default_failure_timeout() -> u64 {
    30
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            seed_nodes: Vec::new(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            failure_timeout_secs: default_failure_timeout(),
        }
    }
}

/// Replication and healing settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationConfig {
    /// Number of replicas to maintain (default: 3)
    #[serde(default = "default_replication_factor")]
    pub replication_factor: usize,

    /// Delay before starting healing after node failure (seconds, default: 300)
    #[serde(default = "default_healing_delay")]
    pub healing_delay_secs: u64,

    /// Enable automatic healing
    #[serde(default = "default_auto_heal")]
    pub auto_heal: bool,

    /// Scrubbing interval in hours (background verification)
    #[serde(default = "default_scrub_interval")]
    pub scrub_interval_hours: u64,

    /// Assumed node-to-node link bandwidth in MB/s, used as the 100% baseline for the
    /// adaptive healing bandwidth controller (default: 100). `None` until resolved by
    /// `Config::load_or_migrate_healing_tuning` — same Option-until-migrated shape as
    /// `node.node_id`, so config always wins over a leftover `DFS_LINK_BANDWIDTH_MB` env
    /// var once this has been set once (by migration or by `dfs-admin healing set`).
    #[serde(default)]
    pub link_bandwidth_mb: Option<usize>,

    /// Maximum percentage of link bandwidth the healer may use, 10-100 (default: 60).
    /// See `link_bandwidth_mb` doc for the Option/migration rationale.
    #[serde(default)]
    pub heal_max_pct: Option<f64>,

    /// Maximum concurrent outstanding PushChunkTo heal transfers (default: 8).
    /// See `link_bandwidth_mb` doc for the Option/migration rationale.
    #[serde(default)]
    pub heal_max_concurrent: Option<usize>,

    /// Maximum concurrent heal transfers any single node may be party to (as source or
    /// target, combined) at once (default: 3). Bounds how many transfers pile onto one
    /// busy node while `heal_max_concurrent` is the overall cluster-wide ceiling; kept
    /// <= `heal_max_concurrent` since a larger per-node value could never bind. See
    /// `link_bandwidth_mb` doc for the Option/migration rationale.
    #[serde(default)]
    pub heal_max_concurrent_per_node: Option<usize>,

    /// Per-transfer timeout for a single heal push, in seconds (default: 120).
    /// See `link_bandwidth_mb` doc for the Option/migration rationale.
    #[serde(default)]
    pub heal_transfer_timeout_secs: Option<u64>,
}

fn default_replication_factor() -> usize {
    3
}

fn default_healing_delay() -> u64 {
    300
}

fn default_auto_heal() -> bool {
    true
}

fn default_scrub_interval() -> u64 {
    24
}

/// Hardcoded final-fallback defaults for the healing-tuning knobs, used only when
/// neither config nor the legacy env var provides a value. Kept as plain fns (not
/// `#[serde(default = ...)]`) since the fields are now `Option<T>` — see
/// `load_or_migrate_healing_tuning`.
pub fn default_link_bandwidth_mb() -> usize {
    100
}

pub fn default_heal_max_pct() -> f64 {
    60.0
}

pub fn default_heal_max_concurrent() -> usize {
    8
}

pub fn default_heal_max_concurrent_per_node() -> usize {
    3
}

pub fn default_heal_transfer_timeout_secs() -> u64 {
    120
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            replication_factor: default_replication_factor(),
            healing_delay_secs: default_healing_delay(),
            auto_heal: default_auto_heal(),
            scrub_interval_hours: default_scrub_interval(),
            link_bandwidth_mb: None,
            heal_max_pct: None,
            heal_max_concurrent: None,
            heal_max_concurrent_per_node: None,
            heal_transfer_timeout_secs: None,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file
    pub fn from_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Save configuration to a TOML file
    pub fn to_file(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Get chunk size in bytes
    pub fn chunk_size_bytes(&self) -> usize {
        self.storage.chunk_size_mb * 1024 * 1024
    }

    /// Return the address this node should advertise to the cluster.
    ///
    /// Priority:
    ///   1. `node.advertise_addr` if explicitly set in config
    ///   2. `node.listen_addr` if it is not a wildcard (not 0.0.0.0 / ::)
    ///   3. Falls back to `listen_addr` unchanged (callers that need a real IP
    ///      must resolve it themselves from the incoming connection source addr)
    pub fn peer_addr(&self) -> std::net::SocketAddr {
        if let Some(addr) = self.node.advertise_addr {
            return addr;
        }
        self.node.listen_addr
    }

    /// Load or create a persistent node ID.
    ///
    /// Resolution order:
    ///   1. `node.node_id` in config (fastest, canonical once written)
    ///   2. `metadata_dir/node_id.json` legacy file (migrated into config on first read)
    ///   3. Generate a fresh UUID and persist it into the config file at `config_path`
    ///
    /// When `config_path` is provided and the ID was not already in the config, the
    /// resolved ID is written back to the config file so future starts use path 1.
    pub fn load_or_create_node_id(
        &mut self,
        config_path: Option<&std::path::Path>,
    ) -> anyhow::Result<NodeId> {
        // 1. Already in config — done.
        if let Some(id) = self.node.node_id {
            return Ok(id);
        }

        // 2. Migrate from legacy node_id.json if it exists.
        let node_id_path = self.storage.metadata_dir.join("node_id.json");
        let node_id = if node_id_path.exists() {
            let contents = std::fs::read_to_string(&node_id_path)?;
            let id: NodeId = serde_json::from_str(&contents)?;
            id
        } else {
            // 3. Generate a fresh ID.
            std::fs::create_dir_all(&self.storage.metadata_dir)?;
            NodeId::new()
        };

        // Persist into config so future starts hit path 1.
        self.node.node_id = Some(node_id);
        if let Some(path) = config_path {
            if let Err(e) = self.to_file(path) {
                // Non-fatal: node still works, just won't be cached next time.
                tracing::warn!("Could not write node_id back to config {:?}: {}", path, e);
            }
        }

        Ok(node_id)
    }

    /// Resolve the four healing-tuning knobs, mirroring `load_or_create_node_id`'s
    /// resolution order and write-back behavior:
    ///
    ///   1. `replication.<field>` in config (canonical, once written — set either by a
    ///      prior migration below, or live via `dfs-admin healing set`)
    ///   2. The field's legacy `DFS_*` env var, migrated into config on first read
    ///   3. A hardcoded default
    ///
    /// Once a field is `Some` in config it is never re-read from the env var again —
    /// this is deliberate: `dfs-admin healing set` persists to config, and a stale env
    /// var left in a systemd unit must never silently override a value the operator
    /// already changed live. Call once at startup, before constructing `HealingManager`.
    pub fn load_or_migrate_healing_tuning(&mut self, config_path: Option<&std::path::Path>) -> anyhow::Result<()> {
        let mut changed = false;

        if self.replication.link_bandwidth_mb.is_none() {
            let v = std::env::var("DFS_LINK_BANDWIDTH_MB")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(default_link_bandwidth_mb);
            self.replication.link_bandwidth_mb = Some(v);
            changed = true;
        }
        if self.replication.heal_max_pct.is_none() {
            let v = std::env::var("DFS_HEAL_MAX_PCT")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or_else(default_heal_max_pct);
            self.replication.heal_max_pct = Some(v);
            changed = true;
        }
        if self.replication.heal_max_concurrent.is_none() {
            let v = std::env::var("DFS_HEAL_MAX_CONCURRENT")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(default_heal_max_concurrent);
            self.replication.heal_max_concurrent = Some(v);
            changed = true;
        }
        if self.replication.heal_max_concurrent_per_node.is_none() {
            let v = std::env::var("DFS_HEAL_MAX_CONCURRENT_PER_NODE")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or_else(default_heal_max_concurrent_per_node);
            self.replication.heal_max_concurrent_per_node = Some(v);
            changed = true;
        }
        if self.replication.heal_transfer_timeout_secs.is_none() {
            let v = std::env::var("DFS_HEAL_TRANSFER_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(default_heal_transfer_timeout_secs);
            self.replication.heal_transfer_timeout_secs = Some(v);
            changed = true;
        }

        if changed {
            if let Some(path) = config_path {
                if let Err(e) = self.to_file(path) {
                    // Non-fatal: node still works at the resolved values, just won't be
                    // cached in config for next start (will re-migrate from env/defaults).
                    tracing::warn!("Could not write migrated healing tuning back to config {:?}: {}", path, e);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.storage.chunk_size_mb, 4);
        assert_eq!(config.replication.replication_factor, 3);
        assert_eq!(config.replication.healing_delay_secs, 300);
    }

    #[test]
    fn test_chunk_size_bytes() {
        let config = Config::default();
        assert_eq!(config.chunk_size_bytes(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_healing_tuning_migration_uses_defaults_when_no_env() {
        std::env::remove_var("DFS_LINK_BANDWIDTH_MB");
        std::env::remove_var("DFS_HEAL_MAX_PCT");
        std::env::remove_var("DFS_HEAL_MAX_CONCURRENT");
        std::env::remove_var("DFS_HEAL_MAX_CONCURRENT_PER_NODE");
        std::env::remove_var("DFS_HEAL_TRANSFER_TIMEOUT_SECS");

        let mut config = Config::default();
        config.load_or_migrate_healing_tuning(None).unwrap();

        assert_eq!(config.replication.link_bandwidth_mb, Some(100));
        assert_eq!(config.replication.heal_max_pct, Some(60.0));
        assert_eq!(config.replication.heal_max_concurrent, Some(8));
        assert_eq!(config.replication.heal_max_concurrent_per_node, Some(3));
        assert_eq!(config.replication.heal_transfer_timeout_secs, Some(120));
    }

    #[test]
    fn test_healing_tuning_config_wins_over_env_once_set() {
        std::env::set_var("DFS_LINK_BANDWIDTH_MB", "999");

        let mut config = Config::default();
        config.replication.link_bandwidth_mb = Some(50); // already-migrated / admin-set value
        config.load_or_migrate_healing_tuning(None).unwrap();

        // Config value wins — the env var is never consulted once the field is Some.
        assert_eq!(config.replication.link_bandwidth_mb, Some(50));

        std::env::remove_var("DFS_LINK_BANDWIDTH_MB");
    }
}
