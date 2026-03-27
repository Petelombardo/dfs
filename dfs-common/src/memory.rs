use anyhow::{Context, Result};
use std::fs;
use std::num::NonZeroUsize;

/// Get available system memory in bytes by reading /proc/meminfo
pub fn get_available_memory() -> Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo")
        .context("Failed to read /proc/meminfo")?;

    // Parse MemAvailable (best indicator of memory we can use without swapping)
    // Format: "MemAvailable:    1234567 kB"
    for line in meminfo.lines() {
        if line.starts_with("MemAvailable:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: u64 = parts[1]
                    .parse()
                    .context("Failed to parse MemAvailable value")?;
                return Ok(kb * 1024); // Convert to bytes
            }
        }
    }

    anyhow::bail!("MemAvailable not found in /proc/meminfo")
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
