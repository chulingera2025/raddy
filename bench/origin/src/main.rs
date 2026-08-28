use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const LISTEN_ADDR: &str = "0.0.0.0:18080";
const HEALTHCHECK_ADDR: &str = "127.0.0.1:18080";
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;

struct Bodies {
    small: Arc<Vec<u8>>,
    four_kib: Arc<Vec<u8>>,
    one_mib: Arc<Vec<u8>>,
    health: Arc<Vec<u8>>,
}

#[tokio::main]
async fn main() {
    if env::args().any(|arg| arg == "--healthcheck") {
        std::process::exit(if healthcheck().await { 0 } else { 1 });
    }

    let listener = TcpListener::bind(LISTEN_ADDR)
        .await
        .expect("failed to bind origin listener");
    let bodies = Arc::new(Bodies {
        small: Arc::new(vec![b's'; 128]),
        four_kib: Arc::new(vec![b'4'; 4096]),
        one_mib: Arc::new(vec![b'1'; 1024 * 1024]),
        health: Arc::new(b"ok\n".to_vec()),
    });

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let bodies = Arc::clone(&bodies);
                tokio::spawn(handle_connection(stream, bodies));
            }
            Err(error) => {
                eprintln!("origin accept failed: {error}");
            }
        }
    }
}

async fn healthcheck() -> bool {
    let Some(mut stream) =
        tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(HEALTHCHECK_ADDR))
            .await
            .ok()
            .and_then(Result::ok)
    else {
        return false;
    };

    if tokio::time::timeout(
        Duration::from_secs(1),
        stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: origin\r\nConnection: close\r\n\r\n"),
    )
    .await
    .is_err()
    {
        return false;
    }

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut response))
        .await
        .ok()
        .and_then(Result::ok)
        .is_some()
        && response.starts_with(b"HTTP/1.1 200")
}

async fn handle_connection(mut stream: TcpStream, bodies: Arc<Bodies>) {
    let _ = stream.set_nodelay(true);
    let mut input = Vec::with_capacity(8192);

    loop {
        input.clear();
        if !read_request_header(&mut stream, &mut input).await {
            return;
        }

        let request = String::from_utf8_lossy(&input);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .split('?')
            .next()
            .unwrap_or("/");
        let body = body_for_path(path, &bodies);
        let close = request
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("connection: close"));
        let connection = if close { "close" } else { "keep-alive" };
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n",
            body.len()
        );

        if stream.write_all(head.as_bytes()).await.is_err() || stream.write_all(body).await.is_err()
        {
            return;
        }
        if close {
            return;
        }
    }
}

async fn read_request_header(stream: &mut TcpStream, input: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(read) = stream.read(&mut chunk).await else {
            return false;
        };
        if read == 0 {
            return false;
        }
        input.extend_from_slice(&chunk[..read]);
        if input.windows(4).any(|window| window == b"\r\n\r\n") {
            return true;
        }
        if input.len() > MAX_REQUEST_HEADER_BYTES {
            return false;
        }
    }
}

fn body_for_path<'a>(path: &str, bodies: &'a Bodies) -> &'a [u8] {
    match path {
        "/healthz" => &bodies.health,
        "/4k" => &bodies.four_kib,
        "/1m" => &bodies.one_mib,
        _ => &bodies.small,
    }
}
