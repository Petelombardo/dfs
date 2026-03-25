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

    /// Optional node name (defaults to hostname)
    pub name: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:8900".parse().unwrap(),
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

    /// Load or create a persistent node ID
    ///
    /// The node ID is stored in the metadata directory to ensure it persists across restarts.
    /// This prevents a node from joining the cluster with a new ID after every restart.
    pub fn load_or_create_node_id(&self) -> anyhow::Result<NodeId> {
        let node_id_path = self.storage.metadata_dir.join("node_id.json");

        // Try to load existing node ID
        if node_id_path.exists() {
            let contents = std::fs::read_to_string(&node_id_path)?;
            let node_id: NodeId = serde_json::from_str(&contents)?;
            Ok(node_id)
        } else {
            // Create new node ID and save it
            let node_id = NodeId::new();

            // Ensure metadata directory exists
            std::fs::create_dir_all(&self.storage.metadata_dir)?;

            // Save node ID to file
            let contents = serde_json::to_string_pretty(&node_id)?;
            std::fs::write(&node_id_path, contents)?;

            Ok(node_id)
        }
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
