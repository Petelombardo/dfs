/// Calculate usable cluster capacity from a set of per-node byte values
/// (e.g. available bytes), given a replication factor.
///
/// Greedy bottleneck algorithm: repeatedly pick the RF nodes with the most
/// remaining capacity, add their bottleneck (the smallest of that top-RF
/// group) to the total, and subtract that bottleneck from exactly those RF
/// nodes. Repeat until fewer than RF nodes have capacity remaining.
///
/// This correctly handles heterogeneous clusters where smart replica-set
/// selection can dramatically increase usable capacity — e.g. RF=3 with
/// nodes (100G, 100G, 100G, 10G) yields 100G usable, not the 13.3G a naive
/// sum/RF formula would give.
pub fn calculate_usable_capacity(node_capacities: &[u64], replication_factor: usize) -> u64 {
    if node_capacities.is_empty() || replication_factor == 0 {
        return 0;
    }

    let mut capacities = node_capacities.to_vec();
    let mut total = 0u64;

    loop {
        // Filter out zeros and sort descending
        let mut non_zero: Vec<u64> = capacities.iter()
            .copied()
            .filter(|&c| c > 0)
            .collect();

        // Check if we have at least RF nodes with capacity > 0
        if non_zero.len() < replication_factor {
            break;
        }

        // Sort descending
        non_zero.sort_by(|a, b| b.cmp(a));

        // The decrement is the minimum of the top RF nodes (the RF-th largest value)
        let decrement = non_zero[replication_factor - 1];
        total += decrement;

        // Subtract decrement ONLY from the top RF nodes
        let mut decremented_count = 0;
        for val in &non_zero[0..replication_factor] {
            // Find this value in the original capacities array and decrement it
            for capacity in &mut capacities {
                if *capacity == *val && decremented_count < replication_factor {
                    *capacity = capacity.saturating_sub(decrement);
                    decremented_count += 1;
                    break; // Move to next value in the top RF
                }
            }
        }
    }

    total
}
