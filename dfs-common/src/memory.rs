use anyhow::{Context, Result};
use std::fs;
use std::num::NonZeroUsize;

/// Get available system memory in bytes by reading /proc/meminfo
pub fn get_available_memory() -> Result<u64> {
    read_meminfo_field("MemAvailable")
}

/// Get total system memory in bytes by reading /proc/meminfo. General-purpose;
/// server-side cache sizing uses `get_available_memory` instead (see
/// `calculate_server_cache_budget_mb`'s doc comment) since total RAM includes
/// memory a given box may never actually get back (GPU/codec reservations,
/// other daemons), which total-RAM-based sizing can't see.
pub fn get_total_memory() -> Result<u64> {
    read_meminfo_field("MemTotal")
}

fn read_meminfo_field(field: &str) -> Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo")
        .context("Failed to read /proc/meminfo")?;

    // Format: "MemAvailable:    1234567 kB"
    let prefix = format!("{}:", field);
    for line in meminfo.lines() {
        if line.starts_with(&prefix) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: u64 = parts[1]
                    .parse()
                    .with_context(|| format!("Failed to parse {} value", field))?;
                return Ok(kb * 1024); // Convert to bytes
            }
        }
    }

    anyhow::bail!("{} not found in /proc/meminfo", field)
}

/// One-time snapshot of `MemAvailable`, frozen the first time anything asks
/// for it. Backs `calculate_server_cache_budget_mb` — see that function's doc
/// comment for why this must be read exactly once, early, rather than fresh
/// per caller.
static STARTUP_AVAILABLE_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

fn startup_available_mb() -> u64 {
    *STARTUP_AVAILABLE_MB.get_or_init(|| {
        get_available_memory()
            .map(|bytes| bytes / (1024 * 1024))
            .unwrap_or(4096)
    })
}

/// MB that dfs-server's chunk caches — the main chunk cache (storage.rs's
/// ChunkStorage) plus chunk_ring and delta_ring (server.rs) — may collectively
/// consume. A single shared budget instead of three independently sized
/// tables, added 2026-07-19 after gluster1 plateaued with only ~77MB
/// `MemAvailable` on a 3.8GB node: the three caches had been sized off total
/// RAM independently (each picking its own MB tier), so nothing enforced a
/// combined ceiling — a 3.8GB node ended up committing ~1GB (27%) to caches
/// before any real workload data existed. Callers split this three ways (see
/// storage.rs::calculate_cache_size and server.rs::calculate_ring_capacity);
/// each may still be overridden individually via its own env var for manual
/// tuning, but the *default* now comes from one number.
///
/// Basis changed from total RAM to a one-time `MemAvailable` snapshot
/// (2026-07-31, same incident as the 18%->10% drop below): total RAM was
/// silently wrong on these boards because it counts memory this process can
/// never actually get — the panfrost/hantro GPU and video-codec drivers this
/// storage workload never touches still carve out a fixed reservation the
/// kernel never returns, on top of whatever the OS and other daemons
/// (glusterd, systemd, etc.) hold at boot. Budgeting off total RAM means two
/// otherwise-identical nodes with different fixed overhead get the same
/// cache budget while having different real ceilings — exactly backwards.
/// `MemAvailable` already nets out reclaimable cache/buffers and gives the
/// real number, so it's the right quantity; the only reason it was avoided
/// before was reading it *repeatedly, mid-startup*, which self-inflates the
/// problem it's trying to measure (chunk_cache's own allocation shows up as
/// reduced `MemAvailable` by the time chunk_ring reads it moments later,
/// cascading into ever-smaller budgets for each successive cache). Reading
/// it exactly once, via `startup_available_mb`'s `OnceLock`, before any of
/// the three caches have allocated anything, keeps the "stable basis" property
/// total RAM was chosen for while actually reflecting this specific box's
/// real headroom rather than a number it never had to begin with.
///
/// `DFS_SERVER_CACHE_BUDGET_PERCENT` overrides the default (10%, lowered from
/// 18% the same day: even total-RAM-based 18%, ~684MB across chunk_cache+
/// chunk_ring+delta_ring, was leaving MemAvailable dipping under 200MB under
/// real write load with zero swap configured — close enough to the edge that
/// a single memory-heavy operation, e.g. redb's compact_db_blocking, needs no
/// additional RSS of its own to tip the kernel into reclaim pressure severe
/// enough to stall I/O for minutes without ever invoking the OOM killer; see
/// main.rs's wedge-detection timeout added the same incident. This board's
/// chunk_map/chunk_to_file/file_write_seqs tables — correctness state, not a
/// cache, not governed by this budget at all — already claim roughly 1-1.3GB
/// on their own and grow with the dataset, so this budget is the only lever
/// available here). Bounded to [64, 2048] MB so a misconfigured percentage
/// can't zero out caching entirely on a tiny box or claim unbounded RAM on a
/// huge one.
pub fn calculate_server_cache_budget_mb() -> u64 {
    let available_mb = startup_available_mb();

    let percent: f64 = std::env::var("DFS_SERVER_CACHE_BUDGET_PERCENT")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(10.0);

    let budget_mb = (available_mb as f64 * (percent / 100.0)) as u64;
    let bounded = budget_mb.clamp(64, 2048);

    tracing::info!(
        "Server cache budget: startup_available={}MB, target {}%, budget={}MB (chunk_cache gets 50%, chunk_ring+delta_ring split the rest)",
        available_mb, percent, bounded
    );

    bounded
}

/// Calculate optimal LRU cache capacity based on available system memory
///
/// # Arguments
/// * `chunk_size` - Size of each chunk in bytes (e.g., 4MB)
/// * `target_percent` - Target percentage of available memory to use (0-100)
/// * `min_chunks` - Minimum cache capacity in chunks
/// * `max_chunks` - Maximum cache capacity in chunks
///
/// # Returns
/// Recommended cache capacity in number of chunks, bounded by min/max
pub fn calculate_cache_capacity(
    chunk_size: usize,
    target_percent: u8,
    min_chunks: usize,
    max_chunks: usize,
) -> Result<NonZeroUsize> {
    let available_bytes = get_available_memory()?;

    // Calculate target cache size
    let target_cache_bytes = (available_bytes as f64 * (target_percent as f64 / 100.0)) as u64;
    let target_chunks = (target_cache_bytes / chunk_size as u64) as usize;

    // Apply bounds
    let capacity = target_chunks.max(min_chunks).min(max_chunks);

    // Log the decision
    let capacity_mb = (capacity * chunk_size) / (1024 * 1024);
    let available_mb = available_bytes / (1024 * 1024);

    tracing::info!(
        "Chunk cache sizing: {} MB available, target {}%, using {} chunks ({} MB)",
        available_mb,
        target_percent,
        capacity,
        capacity_mb
    );

    Ok(NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(min_chunks).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_available_memory() {
        // This will only work on Linux systems
        if let Ok(mem) = get_available_memory() {
            assert!(mem > 0, "Available memory should be positive");
            assert!(mem < 1024 * 1024 * 1024 * 1024, "Available memory should be less than 1TB");
        }
    }

    #[test]
    fn test_calculate_cache_capacity() {
        let chunk_size = 4 * 1024 * 1024; // 4MB

        // Mock scenario: 1GB available, 10% target = ~100MB = ~25 chunks
        // But we'll use actual system memory
        if let Ok(capacity) = calculate_cache_capacity(chunk_size, 10, 10, 500) {
            let cap = capacity.get();
            assert!(cap >= 10, "Capacity should be at least min_chunks");
            assert!(cap <= 500, "Capacity should be at most max_chunks");
        }
    }
}
