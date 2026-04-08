#!/bin/bash
# Build script for DFS distributed filesystem

export PATH=$PATH:/home/petelombardo/.cargo/bin

set -e  # Exit on error

echo "Building DFS release binaries..."
source "/home/petelombardo/.cargo/env"
cargo build --release

echo ""
echo "Build complete!"
echo "Binaries located at:"
echo "  - target/release/dfs-server"
echo "  - target/release/dfs-client"
echo "  - target/release/dfs-admin"
