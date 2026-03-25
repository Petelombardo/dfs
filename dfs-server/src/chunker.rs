use anyhow::{Context, Result};
use dfs_common::{compute_chunk_hash, ChunkId};
use std::io::Read;

/// File chunker - splits files into fixed-size chunks
/// Optimized for SBC environments (streaming, low memory)
pub struct Chunker {
    chunk_size: usize,
}

impl Chunker {
    /// Create a new chunker with specified chunk size
    pub fn new(chunk_size: usize) -> Self {
        Self { chunk_size }
    }

    /// Split data into chunks and return chunk IDs and data
    /// Uses streaming to avoid loading entire file into memory
    pub fn chunk_data(&self, data: &[u8]) -> Vec<(ChunkId, Vec<u8>)> {
        let mut chunks = Vec::new();

        for chunk_data in data.chunks(self.chunk_size) {
            let hash = compute_chunk_hash(chunk_data);
            let chunk_id = ChunkId::from_hash(hash);
            chunks.push((chunk_id, chunk_data.to_vec()));
        }

        chunks
    }

    /// Stream chunks from a reader (memory-efficient for large files)
    pub fn chunk_reader<R: Read>(&self, mut reader: R) -> Result<Vec<(ChunkId, Vec<u8>)>> {
        let mut chunks = Vec::new();
        let mut buffer = vec![0u8; self.chunk_size];

        loop {
            let bytes_read = reader
                .read(&mut buffer)
                .context("Failed to read from stream")?;

            if bytes_read == 0 {
                break;
            }

            let chunk_data = &buffer[..bytes_read];
            let hash = compute_chunk_hash(chunk_data);
            let chunk_id = ChunkId::from_hash(hash);
            chunks.push((chunk_id, chunk_data.to_vec()));
        }

        Ok(chunks)
    }

    /// Reassemble chunks back into original data
    pub fn reassemble_chunks(&self, chunks: Vec<Vec<u8>>) -> Vec<u8> {
        let total_size: usize = chunks.iter().map(|c| c.len()).sum();
        let mut data = Vec::with_capacity(total_size);

        for chunk in chunks {
            data.extend_from_slice(&chunk);
        }

        data
    }

    /// Calculate number of chunks for a given file size
    pub fn calculate_chunk_count(&self, file_size: u64) -> usize {
        let chunk_count = (file_size as f64 / self.chunk_size as f64).ceil() as usize;
        chunk_count.max(1) // At least 1 chunk even for empty files
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_chunk_small_data() {
        let chunker = Chunker::new(1024); // 1KB chunks
        let data = b"Hello, World!";

        let chunks = chunker.chunk_data(data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].1, data);
    }

    #[test]
    fn test_chunk_large_data() {
        let chunker = Chunker::new(10); // 10 byte chunks for testing
        let data = b"This is a test string that is longer than 10 bytes";

        let chunks = chunker.chunk_data(data);
        assert!(chunks.len() > 1);

        // Verify reassembly
        let chunk_data: Vec<Vec<u8>> = chunks.into_iter().map(|(_, data)| data).collect();
        let reassembled = chunker.reassemble_chunks(chunk_data);
        assert_eq!(reassembled, data);
    }

    #[test]
    fn test_chunk_reader() {
        let chunker = Chunker::new(100);
        let data = vec![42u8; 250]; // 250 bytes
        let cursor = Cursor::new(data.clone());

        let chunks = chunker.chunk_reader(cursor).unwrap();
        assert_eq!(chunks.len(), 3); // 100 + 100 + 50

        // Verify data integrity
        let chunk_data: Vec<Vec<u8>> = chunks.into_iter().map(|(_, data)| data).collect();
        let reassembled = chunker.reassemble_chunks(chunk_data);
        assert_eq!(reassembled, data);
    }

    #[test]
    fn test_chunk_exact_multiple() {
        let chunker = Chunker::new(10);
        let data = vec![1u8; 30]; // Exactly 3 chunks

        let chunks = chunker.chunk_data(&data);
        assert_eq!(chunks.len(), 3);
        for (_, chunk_data) in chunks {
            assert_eq!(chunk_data.len(), 10);
        }
    }

    #[test]
    fn test_calculate_chunk_count() {
        let chunker = Chunker::new(4 * 1024 * 1024); // 4MB chunks

        assert_eq!(chunker.calculate_chunk_count(0), 1);
        assert_eq!(chunker.calculate_chunk_count(1024), 1);
        assert_eq!(chunker.calculate_chunk_count(4 * 1024 * 1024), 1);
        assert_eq!(chunker.calculate_chunk_count(4 * 1024 * 1024 + 1), 2);
        assert_eq!(chunker.calculate_chunk_count(8 * 1024 * 1024), 2);
        assert_eq!(chunker.calculate_chunk_count(10 * 1024 * 1024), 3);
    }

    #[test]
    fn test_chunk_empty_data() {
        let chunker = Chunker::new(1024);
        let data = b"";

        let chunks = chunker.chunk_data(data);
        assert_eq!(chunks.len(), 0); // No chunks for empty data
    }

    #[test]
    fn test_chunk_deterministic() {
        let chunker = Chunker::new(100);
        let data = b"Deterministic test data";

        let chunks1 = chunker.chunk_data(data);
        let chunks2 = chunker.chunk_data(data);

        // Same data should produce same chunk IDs
        assert_eq!(chunks1.len(), chunks2.len());
        for (c1, c2) in chunks1.iter().zip(chunks2.iter()) {
            assert_eq!(c1.0, c2.0); // Same ChunkId
        }
    }
}
