# DFS Admin Tools Upgrade

## Overview
Extended the dfs-admin tool with new commands for debugging and managing file metadata corruption.

## Protocol Changes Added
File: `dfs-common/src/protocol.rs`

### New Request Types:
1. **ListAllFiles** - List all files in the metadata database
2. **PurgeFileMetadata { path: String }** - Delete file metadata without deleting chunks (for fixing corruption)

### New Response Type:
- **FileList { files: Vec<FileMetadata>, total_count: usize }** - Response for listing all files

## Implementation Still Needed

### 1. Server-side Handlers
File: `dfs-server/src/server.rs`

Add handlers in the `handle_request` method:

```rust
Request::ListAllFiles => {
    let files = self.metadata.list_files()?;
    let total_count = files.len();
    Response::FileList { files, total_count }
}

Request::PurgeFileMetadata { path } => {
    // Get metadata to find file ID
    if let Some(metadata) = self.metadata.get_file_by_path(&path)? {
        // Delete from metadata store only (not chunks)
        self.metadata.delete_file(&metadata.id)?;
        info!("Purged metadata for file: {}", path);
        Response::Ok { data: None }
    } else {
        Response::Error {
            message: format!("File not found: {}", path),
            code: ErrorCode::NotFound,
        }
    }
}
```

### 2. Admin Tool Commands
File: `dfs-admin/src/main.rs`

Add new commands to the FileCommands enum:

```rust
#[derive(Subcommand)]
enum FileCommands {
    /// Show file information with chunk locations
    Info {
        path: String,
    },
    /// Show chunk replica locations
    Replicas {
        chunk_id: String,
    },
    /// List all files in metadata database
    List,
    /// Purge file metadata from database (without deleting chunks)
    Purge {
        /// File path to purge
        path: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
}
```

Add handlers in `handle_file_command`:

```rust
FileCommands::List => {
    let response = send_request(cluster_addrs[0], Request::ListAllFiles).await?;

    match response {
        Response::FileList { files, total_count } => {
            if json_output {
                let output = serde_json::json!({
                    "total_count": total_count,
                    "files": files.iter().map(|f| {
                        serde_json::json!({
                            "path": f.path,
                            "size": f.size,
                            "chunks": f.chunks.len(),
                            "created": f.created_at,
                            "modified": f.modified_at,
                        })
                    }).collect::<Vec<_>>()
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("All Files in Metadata Database");
                println!("==============================");
                println!("Total Files: {}", total_count);
                println!();
                println!("{:<50} {:<12} {:<8} {}", "Path", "Size", "Chunks", "Modified");
                println!("{}", "-".repeat(100));

                for file in files {
                    let size_str = format_size(file.size);
                    println!("{:<50} {:<12} {:<8} {}",
                        truncate_path(&file.path, 50),
                        size_str,
                        file.chunks.len(),
                        file.modified_at
                    );
                }
            }
        }
        Response::Error { message, .. } => {
            error!("Error: {}", message);
            anyhow::bail!("Command failed: {}", message);
        }
        _ => {
            anyhow::bail!("Unexpected response type");
        }
    }
}

FileCommands::Purge { path, yes } => {
    if !yes {
        print!("Are you sure you want to purge metadata for '{}'? This will NOT delete chunks. [y/N]: ", path);
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let response = send_request(
        cluster_addrs[0],
        Request::PurgeFileMetadata { path: path.clone() },
    ).await?;

    match response {
        Response::Ok { .. } => {
            println!("✓ Successfully purged metadata for: {}", path);
            println!();
            println!("Note: Chunks are still stored on disk.");
            println!("Run 'dfs-admin healing trigger' to clean up orphaned chunks.");
        }
        Response::Error { message, code } => {
            error!("Error: {}", message);
            if code == dfs_common::ErrorCode::NotFound {
                anyhow::bail!("File not found: {}", path);
            } else {
                anyhow::bail!("Command failed: {}", message);
            }
        }
        _ => {
            antml:parameter name="new_string">    antml::bail!("Unexpected response type");
        }
    }
}
```

Helper functions to add:

```rust
fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", size, UNITS[unit_index])
    }
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        let start = &path[..max_len/2 - 2];
        let end = &path[path.len() - (max_len/2 - 1)..];
        format!("{}...{}", start, end)
    }
}
```

## How to Use the New Commands

### List all files in the metadata database:
```bash
dfs-admin file list --cluster 10.25.1.58:8900
```

### Purge corrupt file metadata:
```bash
# With confirmation prompt
dfs-admin file purge /test.img --cluster 10.25.1.58:8900

# Skip confirmation
dfs-admin file purge /test.img --cluster 10.25.1.58:8900 --yes
```

### Purge multiple corrupt files:
```bash
# List files first to see duplicates
dfs-admin file list --cluster 10.25.1.58:8900 | grep "test.img"

# Purge the corrupt entries
dfs-admin file purge /test.img --cluster 10.25.1.58:8900 --yes
dfs-admin file purge /test2.img --cluster 10.25.1.58:8900 --yes

# Trigger healing to clean up orphaned chunks
dfs-admin healing trigger --cluster 10.25.1.58:8900
```

## Benefits
1. **Visibility** - See all files in the metadata database across the cluster
2. **Corruption Recovery** - Purge corrupt metadata entries without losing chunks
3. **Debugging** - Identify duplicate entries and inconsistencies
4. **Safe Operations** - Chunks preserved for potential recovery

## To Complete Implementation
1. Add server handlers in `dfs-server/src/server.rs`
2. Add admin commands in `dfs-admin/src/main.rs`
3. Build and deploy:
   ```bash
   cargo build --release
   # Deploy dfs-admin
   sudo cp target/release/dfs-admin /usr/local/bin/
   ```

## Current Status
- ✅ Protocol messages defined in dfs-common
- ⏳ Server handlers need to be added
- ⏳ Admin tool commands need to be added
- ⏳ Build and test

The protocol changes are complete and committed. The implementation just needs the handler code added to the server and admin tool.
