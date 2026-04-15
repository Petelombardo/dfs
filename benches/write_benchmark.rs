// Write performance benchmark with detailed instrumentation
// Measures: chunking, serialization, network I/O, disk I/O, metadata updates

use dfs_client::DfsClient;
use dfs_common::{compute_chunk_hash, ChunkId, Message, MessageEnvelope, Request, RequestId};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug)]
struct WriteMetrics {
    data_size: usize,
    chunking_time: std::time::Duration,
    serialize_time: std::time::Duration,
    network_send_time: std::time::Duration,
    network_recv_time: std::time::Duration,
    deserialize_time: std::time::Duration,
    total_time: std::time::Duration,
    throughput_mbps: f64,
}

impl WriteMetrics {
    fn print_report(&self) {
        println!("\n=== Write Performance Report ===");
        println!("Data size:        {} bytes ({:.2} MB)", self.data_size, self.data_size as f64 / 1024.0 / 1024.0);
        println!("Total time:       {:?}", self.total_time);
        println!("Throughput:       {:.2} MB/s", self.throughput_mbps);
        println!("\nBreakdown:");
        println!("  Chunking:       {:?} ({:.1}%)", self.chunking_time, self.percent(self.chunking_time));
        println!("  Serialization:  {:?} ({:.1}%)", self.serialize_time, self.percent(self.serialize_time));
        println!("  Network send:   {:?} ({:.1}%)", self.network_send_time, self.percent(self.network_send_time));
        println!("  Network recv:   {:?} ({:.1}%)", self.network_recv_time, self.percent(self.network_recv_time));
        println!("  Deserialization:{:?} ({:.1}%)", self.deserialize_time, self.percent(self.deserialize_time));

        // Calculate implied server time (time not accounted for by client)
        let accounted = self.chunking_time + self.serialize_time + self.network_send_time +
                       self.network_recv_time + self.deserialize_time;
        let server_time = self.total_time.saturating_sub(accounted);
        println!("  Server time:    {:?} ({:.1}%)", server_time,
                 (server_time.as_secs_f64() / self.total_time.as_secs_f64()) * 100.0);
        println!("==============================\n");
    }

    fn percent(&self, duration: std::time::Duration) -> f64 {
        (duration.as_secs_f64() / self.total_time.as_secs_f64()) * 100.0
    }
}

async fn benchmark_write_local_only(server_addr: SocketAddr, data: Vec<u8>) -> anyhow::Result<WriteMetrics> {
    let total_start = Instant::now();
    let data_size = data.len();

    // Stage 1: Chunking (Blake3 hashing)
    let chunking_start = Instant::now();
    let chunk_size = 4 * 1024 * 1024; // 4MB
    let mut chunks = Vec::new();
    for chunk_data in data.chunks(chunk_size) {
        let hash = compute_chunk_hash(chunk_data);
        let chunk_id = ChunkId::from_hash(hash);
        chunks.push((chunk_id, chunk_data.to_vec()));
    }
    let chunking_time = chunking_start.elapsed();

    println!("Chunked {} bytes into {} chunks in {:?}", data_size, chunks.len(), chunking_time);

    // Stage 2: Serialization
    let serialize_start = Instant::now();
    let request = Request::WriteFileLocalOnly { data, file_offset: 0 };
    let request_id = RequestId::new(1);
    let envelope = MessageEnvelope::new(request_id, Message::Request(request));
    let encoded = envelope.to_bytes()?;
    let serialize_time = serialize_start.elapsed();

    println!("Serialized {} bytes in {:?}", encoded.len(), serialize_time);

    // Stage 3: Network I/O
    let mut stream = TcpStream::connect(server_addr).await?;

    // Send
    let network_send_start = Instant::now();
    let len = encoded.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&encoded).await?;
    stream.flush().await?;
    let network_send_time = network_send_start.elapsed();

    println!("Sent {} bytes in {:?}", encoded.len(), network_send_time);

    // Receive
    let network_recv_start = Instant::now();
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let response_len = u32::from_be_bytes(len_buf) as usize;

    let mut response_buf = vec![0u8; response_len];
    stream.read_exact(&mut response_buf).await?;
    let network_recv_time = network_recv_start.elapsed();

    println!("Received {} bytes in {:?}", response_len, network_recv_time);

    // Stage 4: Deserialization
    let deserialize_start = Instant::now();
    let _response_envelope = MessageEnvelope::from_bytes(&response_buf)?;
    let deserialize_time = deserialize_start.elapsed();

    let total_time = total_start.elapsed();
    let throughput_mbps = (data_size as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();

    Ok(WriteMetrics {
        data_size,
        chunking_time,
        serialize_time,
        network_send_time,
        network_recv_time,
        deserialize_time,
        total_time,
        throughput_mbps,
    })
}

async fn benchmark_dual_replica(servers: [SocketAddr; 2], data: Vec<u8>) -> anyhow::Result<WriteMetrics> {
    println!("\n=== Dual-Replica Write Benchmark ===");
    println!("Servers: {:?}", servers);
    println!("Data size: {} bytes ({:.2} MB)", data.len(), data.len() as f64 / 1024.0 / 1024.0);

    let total_start = Instant::now();
    let data_size = data.len();

    // Write to both servers in parallel
    let data1 = data.clone();
    let data2 = data.clone();

    let (result1, result2) = tokio::join!(
        benchmark_write_local_only(servers[0], data1),
        benchmark_write_local_only(servers[1], data2)
    );

    let metrics1 = result1?;
    let metrics2 = result2?;

    let total_time = total_start.elapsed();
    let throughput_mbps = (data_size as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64();

    println!("\n=== Replica 1 ({}) ===", servers[0]);
    metrics1.print_report();

    println!("=== Replica 2 ({}) ===", servers[1]);
    metrics2.print_report();

    // Return combined metrics (max of both replicas)
    Ok(WriteMetrics {
        data_size,
        chunking_time: metrics1.chunking_time.max(metrics2.chunking_time),
        serialize_time: metrics1.serialize_time.max(metrics2.serialize_time),
        network_send_time: metrics1.network_send_time.max(metrics2.network_send_time),
        network_recv_time: metrics1.network_recv_time.max(metrics2.network_recv_time),
        deserialize_time: metrics1.deserialize_time.max(metrics2.deserialize_time),
        total_time,
        throughput_mbps,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("=== DFS Write Performance Profiling ===\n");

    // Parse command line arguments
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!("Usage: {} <server1:port> <server2:port> [size_mb]", args[0]);
        eprintln!("Example: {} 127.0.0.1:8001 127.0.0.1:8002 10", args[0]);
        std::process::exit(1);
    }

    let server1: SocketAddr = args[1].parse()?;
    let server2: SocketAddr = args[2].parse()?;
    let size_mb: usize = if args.len() > 3 { args[3].parse()? } else { 10 };

    println!("Configuration:");
    println!("  Server 1: {}", server1);
    println!("  Server 2: {}", server2);
    println!("  Test size: {} MB\n", size_mb);

    // Generate test data
    let data_size = size_mb * 1024 * 1024;
    println!("Generating {} bytes of random data...", data_size);
    let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();

    // Run dual-replica benchmark
    let metrics = benchmark_dual_replica([server1, server2], data).await?;

    println!("\n=== Overall Dual-Replica Metrics ===");
    metrics.print_report();

    // Network latency analysis
    println!("=== Network Analysis ===");
    let rtt = metrics.network_send_time + metrics.network_recv_time;
    println!("Round-trip time: {:?}", rtt);
    println!("Effective bandwidth: {:.2} MB/s (data size / network time)",
             data_size as f64 / 1024.0 / 1024.0 / (metrics.network_send_time.as_secs_f64()));

    // Identify bottleneck
    println!("\n=== Bottleneck Analysis ===");
    let stages = vec![
        ("Chunking", metrics.chunking_time),
        ("Serialization", metrics.serialize_time),
        ("Network send", metrics.network_send_time),
        ("Network recv", metrics.network_recv_time),
        ("Deserialization", metrics.deserialize_time),
    ];

    let max_stage = stages.iter().max_by_key(|(_, d)| d).unwrap();
    println!("Primary bottleneck: {} ({:?}, {:.1}%)",
             max_stage.0, max_stage.1, metrics.percent(*max_stage.1));

    Ok(())
}
