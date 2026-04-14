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

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            replication_factor: default_replication_factor(),
            healing_delay_secs: default_healing_delay(),
            auto_heal: default_auto_heal(),
            scrub_interval_hours: default_scrub_interval(),
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
}
