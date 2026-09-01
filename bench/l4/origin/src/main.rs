use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

const TCP_ADDR: &str = "0.0.0.0:19000";
const UDP_ADDR: &str = "0.0.0.0:19001";
const READY: &[u8] = b"READY\n";

#[tokio::main]
async fn main() {
    if env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(if healthcheck().await { 0 } else { 1 });
    }

    if let Err(error) = tokio::try_join!(run_tcp(), run_udp()) {
        eprintln!("L4 origin stopped: {error}");
        std::process::exit(1);
    }
}

async fn run_tcp() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(TCP_ADDR).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = handle_tcp(stream).await {
                eprintln!("TCP origin connection failed: {error}");
            }
        });
    }
}

async fn handle_tcp(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    stream.set_nodelay(true)?;
    stream.write_all(READY).await?;
    let mut buffer = [0_u8; 4 * 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        stream.write_all(&buffer[..read]).await?;
    }
}

async fn run_udp() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let socket = UdpSocket::bind(UDP_ADDR).await?;
    let mut buffer = vec![0_u8; 65_535];
    loop {
        let (read, peer) = socket.recv_from(&mut buffer).await?;
        socket.send_to(&buffer[..read], peer).await?;
    }
}

async fn healthcheck() -> bool {
    let tcp_ok = async {
        let mut stream = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(TCP_ADDR))
            .await
            .ok()?
            .ok()?;
        let mut ready = [0_u8; READY.len()];
        tokio::time::timeout(Duration::from_secs(1), stream.read_exact(&mut ready))
            .await
            .ok()?
            .ok()?;
        (ready == READY).then_some(())
    }
    .await
    .is_some();

    let udp_ok = async {
        let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
        let peer: SocketAddr = UDP_ADDR.parse().ok()?;
        socket.send_to(b"health", peer).await.ok()?;
        let mut response = [0_u8; 16];
        let (read, _) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut response))
                .await
                .ok()?
                .ok()?;
        (read == 6 && &response[..read] == b"health").then_some(())
    }
    .await
    .is_some();

    tcp_ok && udp_ok
}
