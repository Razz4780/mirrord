//! Internal proxy is accepting connection from local layers and forward it to agent
//! while having 1:1 relationship - each layer connection is another agent connection.
//!
//! This might be changed later on.
//!
//! The main advantage of this design is that we remove kube logic from the layer itself,
//! thus eliminating bugs that happen due to mix of remote env vars in our code
//! (previously was solved using envguard which wasn't good enough)
//!
//! The proxy will either directly connect to an existing agent (currently only used for tests),
//! or let the [`OperatorApi`](mirrord_operator::client::OperatorApi) handle the connection.

use std::{
    env,
    io::Write,
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use mirrord_analytics::{AnalyticsReporter, CollectAnalytics, Reporter};
use mirrord_config::LayerConfig;
use mirrord_intproxy::{
    agent_conn::{AgentConnectInfo, AgentConnection},
    IntProxy,
};
use nix::{
    libc,
    sys::resource::{setrlimit, Resource},
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::{
    connection::AGENT_CONNECT_INFO_ENV_KEY,
    error::{CliError, InternalProxySetupError, Result},
};

unsafe fn redirect_fd_to_dev_null(fd: libc::c_int) {
    let devnull_fd = libc::open(b"/dev/null\0" as *const [u8; 10] as _, libc::O_RDWR);
    libc::dup2(devnull_fd, fd);
    libc::close(devnull_fd);
}

unsafe fn detach_io() -> Result<()> {
    // Create a new session for the proxy process, detaching from the original terminal.
    // This makes the process not to receive signals from the "mirrord" process or it's parent
    // terminal fixes some side effects such as https://github.com/metalbear-co/mirrord/issues/1232
    nix::unistd::setsid().map_err(InternalProxySetupError::SetSidError)?;

    // flush before redirection
    {
        // best effort
        let _ = std::io::stdout().lock().flush();
    }
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        redirect_fd_to_dev_null(fd);
    }
    Ok(())
}

/// Print the port for the caller (mirrord cli execution flow) so it can pass it
/// back to the layer instances via env var.
fn print_port(listener: &TcpListener) -> Result<()> {
    let port = listener
        .local_addr()
        .map_err(InternalProxySetupError::LocalPortError)?
        .port();
    println!("{port}\n");
    Ok(())
}

/// Creates a listening socket using socket2
/// to control the backlog and manage scenarios where
/// the proxy is under heavy load.
/// <https://github.com/metalbear-co/mirrord/issues/1716#issuecomment-1663736500>
/// in macOS backlog is documented to be hardcoded limited to 128.
fn create_listen_socket() -> Result<TcpListener, InternalProxySetupError> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .map_err(InternalProxySetupError::ListenError)?;

    socket
        .bind(&socket2::SockAddr::from(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            0,
        )))
        .map_err(InternalProxySetupError::ListenError)?;
    socket
        .listen(1024)
        .map_err(InternalProxySetupError::ListenError)?;

    socket
        .set_nonblocking(true)
        .map_err(InternalProxySetupError::ListenError)?;

    // socket2 -> std -> tokio
    TcpListener::from_std(socket.into()).map_err(InternalProxySetupError::ListenError)
}

fn get_agent_connect_info() -> Result<Option<AgentConnectInfo>> {
    let Ok(var) = env::var(AGENT_CONNECT_INFO_ENV_KEY) else {
        return Ok(None);
    };

    serde_json::from_str(&var).map_err(|e| CliError::ConnectInfoLoadFailed(var, e))
}

/// Main entry point for the internal proxy.
/// It listens for inbound layer connect and forwards to agent.
pub(crate) async fn proxy(watch: drain::Watch) -> Result<()> {
    let config = LayerConfig::from_env()?;

    if let Some(ref log_destination) = config.internal_proxy.log_destination {
        let output_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_destination)
            .map_err(CliError::OpenIntProxyLogFile)?;
        let tracing_registry = tracing_subscriber::fmt()
            .with_writer(output_file)
            .with_ansi(false);
        if let Some(ref log_level) = config.internal_proxy.log_level {
            tracing_registry
                .with_env_filter(EnvFilter::builder().parse_lossy(log_level))
                .init();
        } else {
            tracing_registry.init();
        }
    }

    // According to https://wilsonmar.github.io/maximum-limits/ this is the limit on macOS
    // so we assume Linux can be higher and set to that.
    if let Err(err) = setrlimit(Resource::RLIMIT_NOFILE, 12288, 12288) {
        tracing::warn!(?err, "Failed to set the file descriptor limit");
    }

    let agent_connect_info = get_agent_connect_info()?;

    let mut analytics = AnalyticsReporter::new(config.telemetry, watch);
    (&config).collect_analytics(analytics.get_mut());

    // Let it assign port for us then print it for the user.
    let listener = create_listen_socket()?;

    let connection = AgentConnection::new(&config, agent_connect_info, &mut analytics).await?;

    print_port(&listener)?;

    unsafe {
        detach_io()?;
    }

    let first_connection_timeout = Duration::from_secs(config.internal_proxy.start_idle_timeout);
    let consecutive_connection_timeout = Duration::from_secs(config.internal_proxy.idle_timeout);

    IntProxy::new_with_connection(connection, listener)
        .run(first_connection_timeout, consecutive_connection_timeout)
        .await?;

    Ok(())
}
