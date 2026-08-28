//! Plain TCP + UDP echo server for verifying and benchmarking the agents'
//! outgoing feature. Deliberately trivial and multi-threaded, so the server is
//! never the bottleneck of a benchmark.

#[cfg(target_os = "linux")]
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
};

#[cfg(target_os = "linux")]
const BUFFER_SIZE: usize = 256 * 1024;

#[cfg(target_os = "linux")]
async fn echo_tcp(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut buffer = vec![0u8; BUFFER_SIZE];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(..) => break,
            Ok(read) => {
                if stream.write_all(&buffer[..read]).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn echo_udp(socket: UdpSocket) {
    let mut buffer = vec![0u8; u16::MAX as usize];
    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((read, peer)) => {
                let _ = socket.send_to(&buffer[..read], peer).await;
            }
            Err(error) => {
                eprintln!("UDP echo error: {error}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> ExitCode {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|port| port.parse().ok())
        .unwrap_or(7777);

    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("failed to bind TCP {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let udp = match UdpSocket::bind(("0.0.0.0", port)).await {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("failed to bind UDP {port}: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("echo server listening on TCP+UDP port {port}");

    tokio::spawn(echo_udp(udp));

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(echo_tcp(stream));
            }
            Err(error) => {
                eprintln!("accept error: {error}");
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("linux only");
    std::process::exit(1);
}
