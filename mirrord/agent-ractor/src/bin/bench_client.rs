//! Benchmark client: spams outgoing TCP data through a mirrord agent to an echo
//! server, as fast as the agent lets it, and reports throughput.
//!
//! The client opens N outgoing connections through the agent, then keeps every
//! connection saturated with fixed-size write chunks until the requested volume
//! has been pushed, while concurrently draining the echoed reads. Flow control is
//! left entirely to the agent (its per-client memory budgets translate into TCP
//! backpressure on this client's socket), which is exactly how a real intproxy
//! session behaves.
//!
//! CPU usage of the agent is measured externally (see `bench/run_bench.sh`);
//! this binary prints a machine-readable RESULT line with the volume and timing.

#[cfg(target_os = "linux")]
mod run {
    use std::{collections::HashMap, net::SocketAddr, process::ExitCode, time::Instant};

    use bytes::Bytes;
    use futures::{SinkExt, StreamExt};
    use mirrord_agent_ractor::codec::split_agent_connection;
    use mirrord_protocol::{
        ClientMessage, ConnectionId, DaemonMessage,
        outgoing::{
            LayerConnectV2, LayerWrite, SocketAddress,
            tcp::{DaemonTcpOutgoing, LayerTcpOutgoing},
        },
        uid::Uid,
    };
    use tokio::net::TcpStream;

    #[derive(Debug)]
    struct Args {
        agent: SocketAddr,
        target: SocketAddr,
        total_mib: usize,
        chunk_kib: usize,
        conns: usize,
    }

    fn parse_args() -> Args {
        let mut args = std::env::args().skip(1);
        let mut parsed = Args {
            agent: "127.0.0.1:61337".parse().unwrap(),
            target: "127.0.0.1:7777".parse().unwrap(),
            total_mib: 1024,
            chunk_kib: 64,
            conns: 4,
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().expect("missing value for argument");
            match arg.as_str() {
                "--agent" => parsed.agent = value().parse().expect("bad agent address"),
                "--target" => parsed.target = value().parse().expect("bad target address"),
                "--total-mib" => parsed.total_mib = value().parse().expect("bad total"),
                "--chunk-kib" => parsed.chunk_kib = value().parse().expect("bad chunk size"),
                "--conns" => parsed.conns = value().parse().expect("bad connection count"),
                other => panic!("unknown argument: {other}"),
            }
        }
        parsed
    }

    pub async fn main() -> ExitCode {
        let args = parse_args();
        eprintln!("bench_client: {args:?}");

        let stream = TcpStream::connect(args.agent)
            .await
            .expect("agent unreachable");
        stream.set_nodelay(true).unwrap();
        let (mut rx, mut tx) = split_agent_connection(stream);

        tx.send(ClientMessage::SwitchProtocolVersion(
            mirrord_protocol::VERSION.clone(),
        ))
        .await
        .unwrap();
        match rx.next().await {
            Some(Ok(DaemonMessage::SwitchProtocolVersionResponse(version))) => {
                eprintln!("negotiated protocol {version}");
            }
            other => panic!("bad negotiation response: {other:?}"),
        }

        // Open all benchmark connections through the agent up front.
        let mut conns: Vec<ConnectionId> = Vec::with_capacity(args.conns);
        for _ in 0..args.conns {
            let uid = Uid::new_v4();
            tx.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::ConnectV2(
                LayerConnectV2 {
                    uid,
                    remote_address: SocketAddress::Ip(args.target),
                },
            )))
            .await
            .unwrap();
            match rx.next().await {
                Some(Ok(DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::ConnectV2(connect))))
                    if connect.uid == uid =>
                {
                    conns.push(connect.connect.expect("connect failed").connection_id);
                }
                other => panic!("bad connect response: {other:?}"),
            }
        }
        eprintln!("opened {} connections through the agent", conns.len());

        let chunk = Bytes::from(vec![0xABu8; args.chunk_kib * 1024]);
        let total_bytes = args.total_mib * 1024 * 1024;
        let per_conn_chunks = total_bytes / args.conns / chunk.len();
        let per_conn_bytes = per_conn_chunks * chunk.len();

        let started = Instant::now();

        // Writer: round-robin chunks over all connections, then shut each
        // connection's write side down so the echo server closes it back.
        let conns_for_writer = conns.clone();
        let chunk_for_writer = chunk.clone();
        let writer = async move {
            for _ in 0..per_conn_chunks {
                for id in &conns_for_writer {
                    tx.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Write(
                        LayerWrite {
                            connection_id: *id,
                            bytes: chunk_for_writer.clone().into(),
                        },
                    )))
                    .await
                    .unwrap();
                }
            }
            for id in &conns_for_writer {
                tx.send(ClientMessage::TcpOutgoing(LayerTcpOutgoing::Write(
                    LayerWrite {
                        connection_id: *id,
                        bytes: Default::default(),
                    },
                )))
                .await
                .unwrap();
            }
            tx
        };

        // Reader: drain echoes until every connection got everything back and
        // was closed by the agent.
        let expected: HashMap<ConnectionId, usize> =
            conns.iter().map(|id| (*id, per_conn_bytes)).collect();
        let reader = async move {
            let mut received: HashMap<ConnectionId, usize> =
                expected.keys().map(|id| (*id, 0)).collect();
            let mut closed = 0;
            while closed < expected.len() {
                match rx.next().await {
                    Some(Ok(DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Read(Ok(read))))) => {
                        *received.get_mut(&read.connection_id).expect("unknown conn") +=
                            read.bytes.len();
                    }
                    Some(Ok(DaemonMessage::TcpOutgoing(DaemonTcpOutgoing::Close(id)))) => {
                        assert_eq!(
                            received.get(&id),
                            expected.get(&id),
                            "connection {id} closed before echoing everything",
                        );
                        closed += 1;
                    }
                    Some(Ok(DaemonMessage::LogMessage(log))) => {
                        eprintln!("agent log: {log:?}");
                    }
                    other => panic!("unexpected agent message: {other:?}"),
                }
            }
            received.values().sum::<usize>()
        };

        let (mut tx, received_bytes) = tokio::join!(writer, reader);
        let elapsed = started.elapsed();

        let _ = tx.send(ClientMessage::Close).await;

        let sent_mib = (per_conn_bytes * conns.len()) as f64 / 1024.0 / 1024.0;
        let received_mib = received_bytes as f64 / 1024.0 / 1024.0;
        let throughput = sent_mib / elapsed.as_secs_f64();
        println!(
            "RESULT sent_mib={sent_mib:.1} received_mib={received_mib:.1} wall_s={:.3} throughput_mib_s={throughput:.1} conns={} chunk_kib={}",
            elapsed.as_secs_f64(),
            conns.len(),
            args.chunk_kib,
        );
        ExitCode::SUCCESS
    }
}

#[cfg(target_os = "linux")]
#[tokio::main(worker_threads = 4, flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    run::main().await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("linux only");
    std::process::exit(1);
}
