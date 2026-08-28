//! Functional verification harness for the DNS + outgoing features of a mirrord
//! agent. Speaks raw mirrord-protocol to the agent under test and asserts the
//! responses, covering:
//!
//! * protocol version negotiation, ping/pong;
//! * DNS lookups, including response ordering under concurrency;
//! * TCP outgoing: legacy + V2 connects, echo roundtrips, write-side shutdown, close semantics,
//!   legacy connect FIFO ordering (success and failure);
//! * UDP outgoing: V2 connect and echo roundtrip;
//! * graceful handling of unsupported features (expects `Close`).
//!
//! Works against both mirrord-agent (targetless) and mirrord-agent-ractor, so it
//! doubles as a behavioral diff between the two.

#[cfg(target_os = "linux")]
mod run {
    use std::{net::SocketAddr, process::ExitCode, time::Duration};

    use futures::{SinkExt, StreamExt};
    use mirrord_agent_ractor::codec::{AgentRx, AgentTx, split_agent_connection};
    use mirrord_protocol::{
        ClientMessage, ConnectionId, DaemonMessage, FileRequest,
        dns::{AddressFamily, GetAddrInfoRequestV2, SockType},
        file::OpenFileRequest,
        outgoing::{
            LayerClose, LayerConnect, LayerConnectV2, LayerWrite, SocketAddress,
            tcp::{DaemonTcpOutgoing, LayerTcpOutgoing},
            udp::{DaemonUdpOutgoing, LayerUdpOutgoing},
        },
        uid::Uid,
    };
    use tokio::{net::TcpStream, time::timeout};

    const RECV_TIMEOUT: Duration = Duration::from_secs(10);

    struct Conn {
        rx: AgentRx,
        tx: AgentTx,
    }

    impl Conn {
        async fn connect(addr: SocketAddr) -> Self {
            let stream = TcpStream::connect(addr).await.expect("agent unreachable");
            stream.set_nodelay(true).unwrap();
            let (rx, tx) = split_agent_connection(stream);
            Self { rx, tx }
        }

        async fn send(&mut self, message: ClientMessage) {
            self.tx.send(message).await.expect("send to agent failed");
        }

        async fn recv(&mut self) -> DaemonMessage {
            timeout(RECV_TIMEOUT, self.rx.next())
                .await
                .expect("timed out waiting for agent message")
                .expect("agent disconnected")
                .expect("agent connection error")
        }
    }

    fn dns_request(node: &str) -> ClientMessage {
        ClientMessage::GetAddrInfoRequestV2(GetAddrInfoRequestV2 {
            node: node.to_owned(),
            service_port: 0,
            family: AddressFamily::Ipv4Only,
            socktype: SockType::Any,
            flags: 0,
            protocol: 0,
        })
    }

    macro_rules! check {
        ($name:expr, $cond:expr, $($ctx:tt)*) => {
            if $cond {
                println!("PASS {}", $name);
            } else {
                println!("FAIL {}: {}", $name, format_args!($($ctx)*));
                return Err(());
            }
        };
    }

    async fn negotiation(conn: &mut Conn) -> Result<(), ()> {
        conn.send(ClientMessage::SwitchProtocolVersion(
            mirrord_protocol::VERSION.clone(),
        ))
        .await;
        let response = conn.recv().await;
        check!(
            "protocol version negotiation",
            matches!(response, DaemonMessage::SwitchProtocolVersionResponse(..)),
            "unexpected response: {response:?}"
        );

        conn.send(ClientMessage::Ping).await;
        let response = conn.recv().await;
        check!(
            "ping/pong",
            matches!(response, DaemonMessage::Pong),
            "unexpected response: {response:?}"
        );
        Ok(())
    }

    /// Three lookups fired back to back; responses must come back in request
    /// order. `ractor-test-one`/`ractor-test-two` are /etc/hosts entries with
    /// distinct IPs, created by the caller of this harness.
    async fn dns(conn: &mut Conn, ordered_names: bool) -> Result<(), ()> {
        conn.send(dns_request("localhost")).await;
        let response = conn.recv().await;
        let ips: Vec<std::net::IpAddr> = match &response {
            DaemonMessage::GetAddrInfoResponse(response) => match &response.0 {
                Ok(lookup) => lookup.0.iter().map(|record| record.ip).collect(),
                Err(error) => {
                    println!("FAIL dns localhost: lookup error {error:?}");
                    return Err(());
                }
            },
            other => {
                println!("FAIL dns localhost: unexpected response {other:?}");
                return Err(());
            }
        };
        check!(
            "dns localhost",
            ips.iter().any(std::net::IpAddr::is_loopback),
            "no loopback in {ips:?}"
        );

        if !ordered_names {
            return Ok(());
        }

        conn.send(dns_request("ractor-test-one")).await;
        conn.send(dns_request("ractor-test-two")).await;
        conn.send(dns_request("ractor-test-one")).await;
        let mut got = Vec::new();
        for _ in 0..3 {
            match conn.recv().await {
                DaemonMessage::GetAddrInfoResponse(response) => match response.0 {
                    Ok(lookup) => got.push(lookup.0.first().map(|record| record.ip)),
                    Err(error) => {
                        println!("FAIL dns ordering: lookup error {error:?}");
                        return Err(());
                    }
                },
                other => {
                    println!("FAIL dns ordering: unexpected response {other:?}");
                    return Err(());
                }
            }
        }
        let expected =
            ["127.0.0.11", "127.0.0.12", "127.0.0.11"].map(|ip| Some(ip.parse().unwrap()));
        check!(
            "dns response ordering",
            got == expected,
            "got {got:?}, expected {expected:?}"
        );
        Ok(())
    }

    async fn tcp_echo(conn: &mut Conn, echo: SocketAddr) -> Result<(), ()> {
        // Legacy connect: response carries no UID.
        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Connect(
            LayerConnect {
                remote_address: SocketAddress::Ip(echo),
            },
        )))
        .await;
        let id = match conn.recv().await {
            DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Connect(Ok(connect))) => {
                connect.connection_id
            }
            other => {
                println!("FAIL tcp legacy connect: unexpected response {other:?}");
                return Err(());
            }
        };
        println!("PASS tcp legacy connect (id {id})");

        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Write(
            LayerWrite {
                connection_id: id,
                bytes: b"hello, echo".as_slice().into(),
            },
        )))
        .await;
        let response = conn.recv().await;
        check!(
            "tcp echo roundtrip",
            matches!(
                &response,
                DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Read(Ok(read)))
                    if read.connection_id == id && read.bytes.as_ref() == b"hello, echo"
            ),
            "unexpected response: {response:?}"
        );

        // Empty write shuts our write side down; the echo server responds by
        // closing, which must come back as `Close` for the fully-closed connection.
        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Write(
            LayerWrite {
                connection_id: id,
                bytes: Default::default(),
            },
        )))
        .await;
        let response = conn.recv().await;
        check!(
            "tcp shutdown -> close",
            matches!(
                response,
                DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Close(closed)) if closed == id
            ),
            "unexpected response: {response:?}"
        );
        Ok(())
    }

    async fn tcp_v2_and_client_close(conn: &mut Conn, echo: SocketAddr) -> Result<(), ()> {
        let uid = Uid::new_v4();
        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::ConnectV2(
            LayerConnectV2 {
                uid,
                remote_address: SocketAddress::Ip(echo),
            },
        )))
        .await;
        let id = match conn.recv().await {
            DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::ConnectV2(connect))
                if connect.uid == uid =>
            {
                match connect.connect {
                    Ok(connect) => connect.connection_id,
                    Err(error) => {
                        println!("FAIL tcp v2 connect: {error}");
                        return Err(());
                    }
                }
            }
            other => {
                println!("FAIL tcp v2 connect: unexpected response {other:?}");
                return Err(());
            }
        };
        println!("PASS tcp v2 connect (id {id})");

        // Client-initiated close is silent; the agent must still be responsive,
        // and writes to the closed ID must be dropped without breaking the session.
        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Close(
            LayerClose { connection_id: id },
        )))
        .await;
        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Write(
            LayerWrite {
                connection_id: id,
                bytes: b"into the void".as_slice().into(),
            },
        )))
        .await;
        conn.send(ClientMessage::Ping).await;
        let response = conn.recv().await;
        check!(
            "tcp client close is silent",
            matches!(response, DaemonMessage::Pong),
            "unexpected response: {response:?}"
        );
        Ok(())
    }

    async fn tcp_legacy_ordering(conn: &mut Conn, echo: SocketAddr) -> Result<(), ()> {
        // Two legacy connects back to back: one that succeeds and one that gets
        // refused. Responses carry no IDs, so their order is the only way the
        // client can match them - Ok must come first.
        let refused = SocketAddr::new(echo.ip(), 1);
        for address in [echo, refused] {
            conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Connect(
                LayerConnect {
                    remote_address: SocketAddress::Ip(address),
                },
            )))
            .await;
        }

        let first = conn.recv().await;
        let first_id = match &first {
            DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Connect(Ok(connect))) => {
                connect.connection_id
            }
            other => {
                println!("FAIL tcp legacy ordering: first response {other:?}");
                return Err(());
            }
        };
        let second = conn.recv().await;
        check!(
            "tcp legacy connect ordering",
            matches!(
                &second,
                DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Connect(Err(..)))
            ),
            "second response: {second:?}"
        );

        conn.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Close(
            LayerClose {
                connection_id: first_id,
            },
        )))
        .await;
        Ok(())
    }

    async fn udp_echo_check(conn: &mut Conn, echo: SocketAddr) -> Result<(), ()> {
        let uid = Uid::new_v4();
        conn.send(ClientMessage::UdpOutgoing(LayerUdpOutgoing::ConnectV2(
            LayerConnectV2 {
                uid,
                remote_address: SocketAddress::Ip(echo),
            },
        )))
        .await;
        let id: ConnectionId = match conn.recv().await {
            DaemonMessage::UdpOutgoing(DaemonUdpOutgoing::ConnectV2(connect))
                if connect.uid == uid =>
            {
                match connect.connect {
                    Ok(connect) => connect.connection_id,
                    Err(error) => {
                        println!("FAIL udp v2 connect: {error}");
                        return Err(());
                    }
                }
            }
            other => {
                println!("FAIL udp v2 connect: unexpected response {other:?}");
                return Err(());
            }
        };
        println!("PASS udp v2 connect (id {id})");

        conn.send(ClientMessage::UdpOutgoing(LayerUdpOutgoing::Write(
            LayerWrite {
                connection_id: id,
                bytes: b"udp says hi".as_slice().into(),
            },
        )))
        .await;
        let response = conn.recv().await;
        check!(
            "udp echo roundtrip",
            matches!(
                &response,
                DaemonMessage::UdpOutgoing(DaemonUdpOutgoing::Read(Ok(read)))
                    if read.connection_id == id && read.bytes.as_ref() == b"udp says hi"
            ),
            "unexpected response: {response:?}"
        );

        conn.send(ClientMessage::UdpOutgoing(LayerUdpOutgoing::Close(
            LayerClose { connection_id: id },
        )))
        .await;
        Ok(())
    }

    /// Feature outside the ported scope must be answered with `Close`, on a fresh
    /// session so the main session stays clean.
    async fn unsupported_feature(agent: SocketAddr) -> Result<(), ()> {
        let mut conn = Conn::connect(agent).await;
        conn.send(ClientMessage::FileRequest(FileRequest::Open(
            OpenFileRequest {
                path: "/etc/hostname".into(),
                open_options: Default::default(),
            },
        )))
        .await;
        let response = conn.recv().await;
        check!(
            "unsupported feature -> Close",
            matches!(response, DaemonMessage::Close(..)),
            "unexpected response: {response:?}"
        );
        Ok(())
    }

    pub async fn main() -> ExitCode {
        let mut args = std::env::args().skip(1);
        let agent: SocketAddr = args
            .next()
            .expect("usage: verify_client <agent> <tcp-echo> <udp-echo> [--unordered-dns]")
            .parse()
            .expect("bad agent address");
        let tcp_echo: SocketAddr = args.next().expect("missing tcp echo").parse().unwrap();
        let udp_echo: SocketAddr = args.next().expect("missing udp echo").parse().unwrap();
        let flags: Vec<String> = args.collect();
        let ordered_names = !flags.iter().any(|flag| flag == "--unordered-dns");
        // A full mirrord-agent supports file ops, so the unsupported-feature
        // check only applies to the ractor spike.
        let check_unsupported = !flags.iter().any(|flag| flag == "--full-agent");

        let mut conn = Conn::connect(agent).await;

        let results = async {
            negotiation(&mut conn).await?;
            dns(&mut conn, ordered_names).await?;
            tcp_echo_all(&mut conn, tcp_echo).await?;
            udp_echo_check(&mut conn, udp_echo).await?;
            conn.send(ClientMessage::Close).await;
            if check_unsupported {
                unsupported_feature(agent).await?;
            }
            Ok::<(), ()>(())
        }
        .await;

        match results {
            Ok(()) => {
                println!("ALL CHECKS PASSED");
                ExitCode::SUCCESS
            }
            Err(()) => ExitCode::FAILURE,
        }
    }

    async fn tcp_echo_all(conn: &mut Conn, echo: SocketAddr) -> Result<(), ()> {
        tcp_echo(conn, echo).await?;
        tcp_v2_and_client_close(conn, echo).await?;
        tcp_legacy_ordering(conn, echo).await
    }
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    run::main().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("linux only");
    std::process::exit(1);
}
