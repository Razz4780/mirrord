//! mirrord-agent-ractor entrypoint. See the crate docs (`lib.rs`) for the design.

#[cfg(target_os = "linux")]
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    process::ExitCode,
    time::Duration,
};

#[cfg(target_os = "linux")]
use clap::{Parser, Subcommand};
#[cfg(target_os = "linux")]
use mirrord_agent_ractor::agent::{AgentActor, AgentArgs};
#[cfg(target_os = "linux")]
use socket2::SockRef;
#[cfg(target_os = "linux")]
use tokio::net::{TcpListener, TcpSocket};
#[cfg(target_os = "linux")]
use tracing_subscriber::prelude::*;

#[cfg(target_os = "linux")]
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Kept for command line compatibility with mirrord-agent.
    /// Only the targetless mode is supported.
    #[clap(subcommand)]
    mode: Option<Mode>,

    /// Port to use for communication with the clients.
    #[arg(short = 'l', long, default_value_t = 61337)]
    communicate_port: u16,

    /// How long to wait for the first client, in seconds.
    #[arg(short = 't', long, default_value_t = 30)]
    communication_timeout: u16,
}

#[cfg(target_os = "linux")]
#[derive(Clone, Debug, Default, Subcommand)]
enum Mode {
    #[default]
    Targetless,
}

#[cfg(target_os = "linux")]
/// Binds the client listener the same way mirrord-agent does: prefer a dual-stack
/// IPv6 socket so both IPv4 and IPv6 clients can connect, fall back to plain IPv4
/// when the IPv6 stack is unusable.
fn bind_listener(port: u16) -> std::io::Result<TcpListener> {
    let dual_stack = || -> std::io::Result<TcpListener> {
        let socket = TcpSocket::new_v6()?;
        socket.set_reuseaddr(true)?;
        SockRef::from(&socket).set_only_v6(false)?;
        socket.bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port))?;
        socket.listen(1024)
    };

    dual_stack().or_else(|error| {
        tracing::warn!(%error, "Failed to set up an IPv6 client listener, falling back to IPv4");
        let socket = TcpSocket::new_v4()?;
        socket.set_reuseaddr(true)?;
        socket.bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port))?;
        socket.listen(1024)
    })
}

#[cfg(target_os = "linux")]
fn init_tracing() {
    let json = std::env::var("MIRRORD_AGENT_JSON_LOG")
        .ok()
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or_default();

    let registry =
        tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::from_default_env());
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .compact()
                    .with_line_number(true),
            )
            .init();
    }
}

/// The mirrord-agent data path is effectively single threaded (a current-thread
/// main runtime running the client loop and all per-connection IO tasks), so this
/// agent uses a current-thread runtime as well to keep CPU comparisons honest.
#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    init_tracing();
    mirrord_agent_ractor::cpu_sample::spawn_if_configured();

    let args = Args::parse();
    tracing::info!(?args, "Starting mirrord-agent-ractor");

    let listener = match bind_listener(args.communicate_port) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "Failed to bind the client listener");
            return ExitCode::FAILURE;
        }
    };

    let agent_args = AgentArgs {
        listener,
        first_client_timeout: Duration::from_secs(args.communication_timeout.into()),
    };
    let (_agent_ref, agent_handle) =
        match ractor::Actor::spawn(Some("agent".to_owned()), AgentActor, agent_args).await {
            Ok(spawned) => spawned,
            Err(error) => {
                tracing::error!(%error, "Failed to spawn the root agent actor");
                return ExitCode::FAILURE;
            }
        };

    // WARNING: `wait_for_agent_startup` in `mirrord/kube/src/api/container.rs` expects a line
    // containing "agent ready" to be printed, keep it compatible with mirrord-agent.
    println!("agent ready - version {}", env!("CARGO_PKG_VERSION"));

    if let Err(error) = agent_handle.await {
        tracing::error!(%error, "Root agent actor task failed");
        return ExitCode::FAILURE;
    }

    tracing::info!("Agent has finished");
    ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("mirrord-agent-ractor is only supported on Linux");
    std::process::exit(1);
}
