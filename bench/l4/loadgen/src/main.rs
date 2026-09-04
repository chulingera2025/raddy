use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::task::JoinSet;

const READY: &[u8] = b"READY\n";
const MAX_TAIL_SAMPLES: usize = 16_384;
const LATENCY_BUCKETS_US: [u64; 31] = [
    1,
    2,
    3,
    4,
    5,
    6,
    8,
    10,
    12,
    15,
    20,
    25,
    30,
    40,
    50,
    60,
    70,
    80,
    90,
    100,
    125,
    150,
    200,
    250,
    500,
    1_000,
    2_000,
    5_000,
    10_000,
    100_000,
    u64::MAX,
];

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    TcpThroughput,
    TcpLatency,
    TcpConnections,
    TcpConnectRate,
    UdpThroughput,
    UdpLatency,
    UdpFlows,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::TcpThroughput => "tcp-throughput",
            Self::TcpLatency => "tcp-latency",
            Self::TcpConnections => "tcp-connections",
            Self::TcpConnectRate => "tcp-connect-rate",
            Self::UdpThroughput => "udp-throughput",
            Self::UdpLatency => "udp-latency",
            Self::UdpFlows => "udp-flows",
        }
    }

    fn holds_connections(self) -> bool {
        matches!(
            self,
            Self::TcpConnections | Self::TcpConnectRate | Self::UdpFlows
        )
    }
}

#[derive(Clone, Debug, Parser)]
#[command(about = "TCP and UDP load generator for the Raddex L4 benchmark")]
struct Args {
    #[arg(long, value_enum)]
    mode: Mode,
    #[arg(long)]
    host: String,
    #[arg(long)]
    port: u16,
    #[arg(long, default_value_t = 5)]
    duration_secs: u64,
    #[arg(long, default_value_t = 1)]
    connections: usize,
    #[arg(long, default_value_t = 64)]
    payload_bytes: usize,
    #[arg(long, default_value_t = 1)]
    window: usize,
    #[arg(long, default_value_t = 0)]
    connect_rate: usize,
    #[arg(long, default_value_t = 0)]
    packets_per_second: u64,
    #[arg(long, default_value_t = 5_000)]
    connect_timeout_ms: u64,
}

#[derive(Clone, Debug, Default)]
struct Histogram {
    buckets: [u64; LATENCY_BUCKETS_US.len()],
    total: u64,
    max_us: u64,
    tail_samples: Vec<u64>,
}

impl Histogram {
    fn observe(&mut self, duration: Duration) {
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let bucket = LATENCY_BUCKETS_US
            .iter()
            .position(|limit| micros <= *limit)
            .unwrap_or(LATENCY_BUCKETS_US.len() - 1);
        self.buckets[bucket] += 1;
        self.total += 1;
        self.max_us = self.max_us.max(micros);
        // Retain a bounded overflow sample so high-latency p99 values stay
        // useful without allowing a bad target to grow the loadgen heap.
        if bucket == LATENCY_BUCKETS_US.len() - 1 && self.tail_samples.len() < MAX_TAIL_SAMPLES {
            self.tail_samples.push(micros);
        }
    }

    fn merge(&mut self, other: &Self) {
        for (left, right) in self.buckets.iter_mut().zip(other.buckets) {
            *left += right;
        }
        self.total += other.total;
        self.max_us = self.max_us.max(other.max_us);
        let remaining = MAX_TAIL_SAMPLES.saturating_sub(self.tail_samples.len());
        self.tail_samples
            .extend(other.tail_samples.iter().take(remaining));
    }

    fn percentile(&self, percentile: f64) -> Option<u64> {
        if self.total == 0 {
            return None;
        }
        let rank = ((self.total as f64) * percentile).ceil().max(1.0) as u64;
        let mut seen = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= rank {
                if index != LATENCY_BUCKETS_US.len() - 1 {
                    return Some(LATENCY_BUCKETS_US[index]);
                }
                let tail_count = self.buckets[index];
                if tail_count <= self.tail_samples.len() as u64 {
                    let tail_rank = (rank - (seen - count)).max(1) as usize;
                    let mut tail = self.tail_samples.clone();
                    tail.sort_unstable();
                    return tail.get(tail_rank - 1).copied().or(Some(self.max_us));
                }
                return Some(self.max_us);
            }
        }
        Some(self.max_us)
    }
}

#[derive(Debug, Default)]
struct Stats {
    sent_bytes: u64,
    received_bytes: u64,
    sent_packets: u64,
    received_packets: u64,
    completed_operations: u64,
    successful_connections: u64,
    failed_connections: u64,
    errors: u64,
    latency: Histogram,
    connect_latency: Histogram,
    establishment_seconds: f64,
}

impl Stats {
    fn merge(&mut self, other: Self) {
        self.sent_bytes += other.sent_bytes;
        self.received_bytes += other.received_bytes;
        self.sent_packets += other.sent_packets;
        self.received_packets += other.received_packets;
        self.completed_operations += other.completed_operations;
        self.successful_connections += other.successful_connections;
        self.failed_connections += other.failed_connections;
        self.errors += other.errors;
        self.latency.merge(&other.latency);
        self.connect_latency.merge(&other.connect_latency);
    }
}

#[derive(Debug, Serialize)]
struct Output {
    schema_version: u8,
    mode: String,
    elapsed_seconds: f64,
    establishment_seconds: f64,
    requested_connections: usize,
    successful_connections: u64,
    failed_connections: u64,
    completed_operations: u64,
    sent_bytes: u64,
    received_bytes: u64,
    sent_packets: u64,
    received_packets: u64,
    errors: u64,
    success_rate: f64,
    error_rate: f64,
    packet_loss_pct: f64,
    offered_packets_per_second: u64,
    throughput_mbps: f64,
    packets_per_second: f64,
    connection_rate_per_second: f64,
    p50_latency_us: Option<u64>,
    p95_latency_us: Option<u64>,
    p99_latency_us: Option<u64>,
    max_latency_us: Option<u64>,
    p50_connect_latency_us: Option<u64>,
    p95_connect_latency_us: Option<u64>,
    p99_connect_latency_us: Option<u64>,
    max_connect_latency_us: Option<u64>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args = Args::parse();
    validate(&args)?;
    let started = Instant::now();
    let stats = match args.mode {
        Mode::TcpThroughput | Mode::TcpLatency => run_tcp_echo(&args).await?,
        Mode::TcpConnections | Mode::TcpConnectRate => run_tcp_connections(&args).await?,
        Mode::UdpThroughput => run_udp_throughput(&args).await?,
        Mode::UdpLatency => run_udp_latency(&args).await?,
        Mode::UdpFlows => run_udp_flows(&args).await?,
    };
    let elapsed = if matches!(
        args.mode,
        Mode::TcpThroughput | Mode::TcpLatency | Mode::UdpThroughput | Mode::UdpLatency
    ) {
        args.duration_secs as f64
    } else {
        started.elapsed().as_secs_f64()
    }
    .max(0.001);
    let output = make_output(&args, stats, elapsed);
    if output.successful_connections == 0
        && output.completed_operations == 0
        && output.received_packets == 0
    {
        return Err(format!("{} produced no successful work", args.mode.name()));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&output)
            .map_err(|error| format!("failed to encode result: {error}"))?
    );
    Ok(())
}

fn validate(args: &Args) -> Result<(), String> {
    if args.host.trim().is_empty() {
        return Err("--host must not be empty".into());
    }
    if args.duration_secs == 0 {
        return Err("--duration-secs must be positive".into());
    }
    if args.connections == 0 {
        return Err("--connections must be positive".into());
    }
    if args.payload_bytes == 0 {
        return Err("--payload-bytes must be positive".into());
    }
    if args.window == 0 {
        return Err("--window must be positive".into());
    }
    if args.connect_timeout_ms == 0 {
        return Err("--connect-timeout-ms must be positive".into());
    }
    if args
        .payload_bytes
        .checked_mul(args.window)
        .is_none_or(|size| size > 8 * 1024 * 1024)
    {
        return Err("--payload-bytes multiplied by --window must not exceed 8 MiB".into());
    }
    if matches!(
        args.mode,
        Mode::UdpThroughput | Mode::UdpLatency | Mode::UdpFlows
    ) && args.payload_bytes > 65_507
    {
        return Err("UDP payload must not exceed 65507 bytes".into());
    }
    Ok(())
}

async fn connect_ready(
    host: String,
    port: u16,
    timeout_ms: u64,
) -> Result<(TcpStream, Duration), String> {
    let started = Instant::now();
    let mut stream = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| "TCP connect timed out".to_string())?
    .map_err(|error| format!("TCP connect failed: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("failed to set TCP_NODELAY: {error}"))?;
    let mut ready = [0_u8; READY.len()];
    tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        stream.read_exact(&mut ready),
    )
    .await
    .map_err(|_| "TCP ready handshake timed out".to_string())?
    .map_err(|error| format!("TCP ready handshake failed: {error}"))?;
    if ready != READY {
        return Err("TCP origin returned an invalid ready handshake".into());
    }
    Ok((stream, started.elapsed()))
}

async fn tcp_echo_worker(
    mut stream: TcpStream,
    connect_latency: Duration,
    args: Arc<Args>,
    deadline: Instant,
) -> Stats {
    let mut stats = Stats {
        successful_connections: 1,
        ..Stats::default()
    };
    stats.connect_latency.observe(connect_latency);
    let payload = vec![b'x'; args.payload_bytes];
    let batch = payload.repeat(args.window);
    let mut response = vec![0_u8; batch.len()];
    let operation_timeout = Duration::from_millis(args.connect_timeout_ms);
    while Instant::now() < deadline {
        let started = Instant::now();
        match tokio::time::timeout(operation_timeout, stream.write_all(&batch)).await {
            Ok(Ok(())) => {
                stats.sent_bytes += batch.len() as u64;
                stats.sent_packets += args.window as u64;
            }
            Ok(Err(_)) | Err(_) => {
                stats.errors += args.window as u64;
                break;
            }
        }
        match tokio::time::timeout(operation_timeout, stream.read_exact(&mut response)).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                stats.errors += args.window as u64;
                stats.latency.observe(started.elapsed());
                break;
            }
        }
        stats.received_bytes += batch.len() as u64;
        stats.received_packets += args.window as u64;
        stats.completed_operations += args.window as u64;
        stats.latency.observe(started.elapsed());
    }
    stats
}

async fn run_tcp_echo(args: &Args) -> Result<Stats, String> {
    let establishment_started = Instant::now();
    let args = Arc::new(args.clone());
    let mut connectors = JoinSet::new();
    for _ in 0..args.connections {
        connectors.spawn(connect_ready(
            args.host.clone(),
            args.port,
            args.connect_timeout_ms,
        ));
    }
    let mut stats = Stats::default();
    let mut connected = Vec::new();
    while let Some(result) = connectors.join_next().await {
        match result {
            Ok(Ok((stream, latency))) => connected.push((stream, latency)),
            Ok(Err(_)) => {
                stats.failed_connections += 1;
                stats.errors += 1;
            }
            Err(_) => {
                stats.failed_connections += 1;
                stats.errors += 1;
            }
        }
    }
    stats.establishment_seconds = establishment_started.elapsed().as_secs_f64().max(0.001);
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let mut workers = JoinSet::new();
    for (stream, latency) in connected {
        workers.spawn(tcp_echo_worker(
            stream,
            latency,
            Arc::clone(&args),
            deadline,
        ));
    }
    while let Some(result) = workers.join_next().await {
        match result {
            Ok(worker) => stats.merge(worker),
            Err(_) => stats.errors += 1,
        }
    }
    Ok(stats)
}

async fn run_tcp_connections(args: &Args) -> Result<Stats, String> {
    let started = Instant::now();
    let batch_size = if args.connect_rate == 0 {
        args.connections
    } else {
        (args.connect_rate / 10).max(1)
    };
    let mut streams = Vec::with_capacity(args.connections);
    let mut stats = Stats::default();
    let mut completed = 0;
    while completed < args.connections {
        let count = (args.connections - completed).min(batch_size);
        let mut batch = JoinSet::new();
        for _ in 0..count {
            batch.spawn(connect_ready(
                args.host.clone(),
                args.port,
                args.connect_timeout_ms,
            ));
        }
        while let Some(result) = batch.join_next().await {
            match result {
                Ok(Ok((stream, latency))) => {
                    streams.push(stream);
                    stats.successful_connections += 1;
                    stats.connect_latency.observe(latency);
                }
                Ok(Err(_)) => {
                    stats.failed_connections += 1;
                    stats.errors += 1;
                }
                Err(_) => stats.errors += 1,
            }
        }
        completed += count;
        if args.connect_rate > 0 && completed < args.connections {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    stats.establishment_seconds = started.elapsed().as_secs_f64().max(0.001);
    tokio::time::sleep(Duration::from_secs(args.duration_secs)).await;
    drop(streams);
    Ok(stats)
}

async fn connected_udp_socket(host: &str, port: u16) -> Result<Arc<UdpSocket>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|error| format!("UDP bind failed: {error}"))?;
    socket
        .connect((host, port))
        .await
        .map_err(|error| format!("UDP connect failed: {error}"))?;
    Ok(Arc::new(socket))
}

async fn run_udp_throughput(args: &Args) -> Result<Stats, String> {
    let socket = connected_udp_socket(&args.host, args.port).await?;
    let payload = Arc::new(vec![b'u'; args.payload_bytes]);
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let drain_deadline = deadline + Duration::from_millis(250);
    let sender_socket = Arc::clone(&socket);
    let sender_payload = Arc::clone(&payload);
    let sender = async move {
        let mut stats = Stats::default();
        let burst_size = if args.packets_per_second > 0 {
            (args.packets_per_second / 100).clamp(1, 256)
        } else {
            1
        };
        let interval = (!sender_payload.is_empty() && args.packets_per_second > 0)
            .then(|| Duration::from_secs_f64(burst_size as f64 / args.packets_per_second as f64));
        let mut next_send = Instant::now();
        'send: while Instant::now() < deadline {
            if interval.is_some() {
                let now = Instant::now();
                if now < next_send {
                    tokio::time::sleep(next_send - now).await;
                }
            }
            for _ in 0..burst_size {
                if Instant::now() >= deadline {
                    break;
                }
                stats.sent_packets += 1;
                match tokio::time::timeout(
                    Duration::from_millis(args.connect_timeout_ms),
                    sender_socket.send(&sender_payload),
                )
                .await
                {
                    Ok(Ok(sent)) => {
                        stats.sent_bytes += sent as u64;
                    }
                    Ok(Err(_)) | Err(_) => {
                        stats.errors += 1;
                        break 'send;
                    }
                }
            }
            if let Some(interval) = interval {
                next_send += interval;
                if next_send < Instant::now() {
                    next_send = Instant::now();
                }
            }
        }
        stats
    };
    let receiver_socket = Arc::clone(&socket);
    let receiver = async move {
        let mut stats = Stats::default();
        let mut buffer = vec![0_u8; args.payload_bytes.max(65_507)];
        while Instant::now() < drain_deadline {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            let timeout = remaining.min(Duration::from_millis(50));
            match tokio::time::timeout(timeout, receiver_socket.recv(&mut buffer)).await {
                Ok(Ok(read)) => {
                    stats.received_bytes += read as u64;
                    stats.received_packets += 1;
                }
                Ok(Err(_)) => {
                    stats.errors += 1;
                    break;
                }
                Err(_) if Instant::now() >= deadline => break,
                Err(_) => {}
            }
        }
        stats
    };
    let (mut sent, received) = tokio::join!(sender, receiver);
    sent.merge(received);
    sent.completed_operations = sent.received_packets;
    sent.successful_connections = 1;
    sent.establishment_seconds = 0.001;
    Ok(sent)
}

async fn run_udp_latency(args: &Args) -> Result<Stats, String> {
    let socket = connected_udp_socket(&args.host, args.port).await?;
    let payload = vec![b'l'; args.payload_bytes];
    let deadline = Instant::now() + Duration::from_secs(args.duration_secs);
    let mut stats = Stats {
        successful_connections: 1,
        establishment_seconds: 0.001,
        ..Stats::default()
    };
    let mut buffer = vec![0_u8; 65_507];
    while Instant::now() < deadline {
        let started = Instant::now();
        stats.sent_packets += 1;
        match tokio::time::timeout(
            Duration::from_millis(args.connect_timeout_ms),
            socket.send(&payload),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => {
                stats.errors += 1;
                stats.latency.observe(started.elapsed());
                break;
            }
        }
        stats.sent_bytes += payload.len() as u64;
        match tokio::time::timeout(
            Duration::from_millis(args.connect_timeout_ms),
            socket.recv(&mut buffer),
        )
        .await
        {
            Ok(Ok(read)) => {
                stats.received_bytes += read as u64;
                stats.received_packets += 1;
                stats.completed_operations += 1;
                stats.latency.observe(started.elapsed());
            }
            Ok(Err(_)) | Err(_) => {
                stats.errors += 1;
                stats.latency.observe(started.elapsed());
            }
        }
    }
    Ok(stats)
}

async fn connect_udp_flow(
    host: String,
    port: u16,
    payload_bytes: usize,
    timeout_ms: u64,
) -> Result<(UdpSocket, Duration), String> {
    let started = Instant::now();
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|error| format!("UDP flow bind failed: {error}"))?;
    socket
        .connect((host.as_str(), port))
        .await
        .map_err(|error| format!("UDP flow connect failed: {error}"))?;
    let payload = vec![b'f'; payload_bytes];
    tokio::time::timeout(Duration::from_millis(timeout_ms), socket.send(&payload))
        .await
        .map_err(|_| "UDP flow probe timed out while sending".to_string())?
        .map_err(|error| format!("UDP flow probe failed: {error}"))?;
    let mut response = vec![0_u8; 65_507];
    tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        socket.recv(&mut response),
    )
    .await
    .map_err(|_| "UDP flow probe timed out".to_string())?
    .map_err(|error| format!("UDP flow probe receive failed: {error}"))?;
    Ok((socket, started.elapsed()))
}

async fn run_udp_flows(args: &Args) -> Result<Stats, String> {
    let started = Instant::now();
    let batch_size = if args.connect_rate == 0 {
        args.connections
    } else {
        (args.connect_rate / 10).max(1)
    };
    let mut sockets = Vec::with_capacity(args.connections);
    let mut stats = Stats::default();
    let mut completed = 0;
    while completed < args.connections {
        let count = (args.connections - completed).min(batch_size);
        let mut batch = JoinSet::new();
        for _ in 0..count {
            batch.spawn(connect_udp_flow(
                args.host.clone(),
                args.port,
                args.payload_bytes,
                args.connect_timeout_ms,
            ));
        }
        while let Some(result) = batch.join_next().await {
            match result {
                Ok(Ok((socket, latency))) => {
                    sockets.push(socket);
                    stats.successful_connections += 1;
                    stats.connect_latency.observe(latency);
                }
                Ok(Err(_)) => {
                    stats.failed_connections += 1;
                    stats.errors += 1;
                }
                Err(_) => stats.errors += 1,
            }
        }
        completed += count;
        if args.connect_rate > 0 && completed < args.connections {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    stats.establishment_seconds = started.elapsed().as_secs_f64().max(0.001);
    tokio::time::sleep(Duration::from_secs(args.duration_secs)).await;
    drop(sockets);
    Ok(stats)
}

fn make_output(args: &Args, stats: Stats, elapsed_seconds: f64) -> Output {
    let operation_success_rate = if args.mode.holds_connections() {
        if args.connections > 0 {
            stats.successful_connections as f64 / args.connections as f64
        } else {
            0.0
        }
    } else if matches!(args.mode, Mode::UdpThroughput | Mode::UdpLatency) {
        if stats.sent_packets > 0 {
            stats.received_packets as f64 / stats.sent_packets as f64
        } else {
            0.0
        }
    } else {
        let attempted_operations = stats.completed_operations.saturating_add(stats.errors);
        if attempted_operations > 0 {
            stats.completed_operations as f64 / attempted_operations as f64
        } else {
            0.0
        }
    };
    let success_rate = operation_success_rate.clamp(0.0, 1.0);
    let establishment_seconds = stats.establishment_seconds.max(0.001);
    Output {
        schema_version: 1,
        mode: args.mode.name().to_string(),
        elapsed_seconds,
        establishment_seconds,
        requested_connections: args.connections,
        successful_connections: stats.successful_connections,
        failed_connections: stats.failed_connections,
        completed_operations: stats.completed_operations,
        sent_bytes: stats.sent_bytes,
        received_bytes: stats.received_bytes,
        sent_packets: stats.sent_packets,
        received_packets: stats.received_packets,
        errors: stats.errors,
        success_rate,
        error_rate: 1.0 - success_rate,
        packet_loss_pct: if !matches!(args.mode, Mode::UdpThroughput | Mode::UdpLatency) {
            0.0
        } else {
            if stats.sent_packets == 0 {
                0.0
            } else {
                (1.0 - success_rate) * 100.0
            }
        },
        offered_packets_per_second: args.packets_per_second,
        throughput_mbps: stats.received_bytes as f64 * 8.0 / elapsed_seconds / 1_000_000.0,
        packets_per_second: stats.received_packets as f64 / elapsed_seconds,
        connection_rate_per_second: stats.successful_connections as f64 / establishment_seconds,
        p50_latency_us: stats.latency.percentile(0.50),
        p95_latency_us: stats.latency.percentile(0.95),
        p99_latency_us: stats.latency.percentile(0.99),
        max_latency_us: (stats.latency.total > 0).then_some(stats.latency.max_us),
        p50_connect_latency_us: stats.connect_latency.percentile(0.50),
        p95_connect_latency_us: stats.connect_latency.percentile(0.95),
        p99_connect_latency_us: stats.connect_latency.percentile(0.99),
        max_connect_latency_us: (stats.connect_latency.total > 0)
            .then_some(stats.connect_latency.max_us),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_reports_monotonic_percentile_buckets() {
        let mut histogram = Histogram::default();
        histogram.observe(Duration::from_micros(7));
        histogram.observe(Duration::from_micros(1_500));
        assert_eq!(histogram.percentile(0.50), Some(8));
        assert_eq!(histogram.percentile(0.99), Some(2_000));
        assert_eq!(histogram.max_us, 1_500);
    }

    #[test]
    fn validation_rejects_an_oversized_udp_payload() {
        let args = Args {
            mode: Mode::UdpLatency,
            host: "127.0.0.1".into(),
            port: 18001,
            duration_secs: 1,
            connections: 1,
            payload_bytes: 65_508,
            window: 1,
            connect_rate: 0,
            packets_per_second: 0,
            connect_timeout_ms: 1_000,
        };
        assert!(validate(&args).is_err());
    }

    #[test]
    fn histogram_reports_actual_percentile_for_values_above_the_last_bucket() {
        let mut histogram = Histogram::default();
        for micros in 150_000..150_100 {
            histogram.observe(Duration::from_micros(micros));
        }

        assert_eq!(histogram.percentile(0.99), Some(150_098));
    }

    #[test]
    fn output_includes_operation_errors_in_the_success_rate() {
        let args = Args {
            mode: Mode::TcpLatency,
            host: "127.0.0.1".into(),
            port: 18000,
            duration_secs: 1,
            connections: 1,
            payload_bytes: 64,
            window: 1,
            connect_rate: 0,
            packets_per_second: 0,
            connect_timeout_ms: 1_000,
        };
        let stats = Stats {
            completed_operations: 8,
            successful_connections: 1,
            errors: 2,
            ..Stats::default()
        };

        let output = make_output(&args, stats, 1.0);

        assert!((output.success_rate - 0.8).abs() < f64::EPSILON);
        assert!((output.error_rate - 0.2).abs() < f64::EPSILON);
        assert_eq!(output.packet_loss_pct, 0.0);
    }
}
