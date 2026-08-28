//! UDP flavor of the outgoing feature. The agent binds a fresh socket per
//! "connection" and `connect`s it to the peer, so reads and writes map onto the
//! same chunk-based protocol as the stream transports.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};

use bytes::Bytes;
use mirrord_protocol::{
    DaemonMessage,
    outgoing::{DaemonConnect, DaemonConnectV2, DaemonRead, SocketAddress, udp::DaemonUdpOutgoing},
};
use tokio::net::UdpSocket;

use crate::outgoing::{DaemonEvent, EstablishedConn, OutgoingFlavor};

/// Big enough for any UDP datagram.
const RECV_BUFFER_SIZE: usize = u16::MAX as usize;

pub struct UdpFlavor;

pub struct UdpReader {
    socket: Arc<UdpSocket>,
    buffer: Box<[u8]>,
}

impl OutgoingFlavor for UdpFlavor {
    const NAME: &'static str = "udp-out";

    type Writer = Arc<UdpSocket>;
    type Reader = UdpReader;

    async fn connect(address: SocketAddress) -> io::Result<EstablishedConn<Self>> {
        let addr = match address {
            SocketAddress::Ip(addr) => addr,
            SocketAddress::Unix(..) => {
                return Err(io::Error::other(format!(
                    "unexpected UNIX address: {address}"
                )));
            }
        };

        let bind_addr = match addr.ip() {
            IpAddr::V4(..) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(..) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let socket = UdpSocket::bind(SocketAddr::new(bind_addr, 0)).await?;
        socket.connect(addr).await?;
        let local_address = socket.local_addr()?.into();
        let peer_address = socket.peer_addr()?.into();
        let socket = Arc::new(socket);

        Ok(EstablishedConn {
            reader: UdpReader {
                socket: socket.clone(),
                buffer: vec![0; RECV_BUFFER_SIZE].into_boxed_slice(),
            },
            writer: socket,
            local_address,
            peer_address,
        })
    }

    async fn write(writer: &mut Self::Writer, bytes: Bytes) -> io::Result<()> {
        let sent = writer.send(&bytes).await?;
        if sent < bytes.len() {
            Err(io::Error::other(
                "failed to send the whole datagram through the socket",
            ))
        } else {
            Ok(())
        }
    }

    /// UDP has no write-side shutdown; the writer is simply dropped by the caller.
    async fn shutdown(_writer: &mut Self::Writer) -> io::Result<()> {
        Ok(())
    }

    async fn read(reader: &mut Self::Reader) -> io::Result<Option<Bytes>> {
        let read = reader.socket.recv(&mut reader.buffer).await?;
        // An empty datagram is treated as stream end, matching mirrord-agent's
        // `UdpStream` behavior (a `write` of empty bytes on the other side is the
        // client's shutdown signal, so the semantics line up).
        Ok((read > 0).then(|| Bytes::copy_from_slice(&reader.buffer[..read])))
    }

    fn daemon_message(event: DaemonEvent) -> DaemonMessage {
        let message = match event {
            DaemonEvent::ConnectOk {
                uid: None,
                id,
                local_address,
                peer_address,
            } => DaemonUdpOutgoing::Connect(Ok(DaemonConnect {
                connection_id: id,
                remote_address: peer_address,
                local_address,
            })),
            DaemonEvent::ConnectOk {
                uid: Some(uid),
                id,
                local_address,
                peer_address,
            } => DaemonUdpOutgoing::ConnectV2(DaemonConnectV2 {
                uid,
                connect: Ok(DaemonConnect {
                    connection_id: id,
                    remote_address: peer_address,
                    local_address,
                }),
            }),
            DaemonEvent::ConnectErr { uid: None, error } => {
                DaemonUdpOutgoing::Connect(Err(error.into()))
            }
            DaemonEvent::ConnectErr {
                uid: Some(uid),
                error,
            } => DaemonUdpOutgoing::ConnectV2(DaemonConnectV2 {
                uid,
                connect: Err(error.into()),
            }),
            DaemonEvent::Read(id, bytes) => DaemonUdpOutgoing::Read(Ok(DaemonRead {
                connection_id: id,
                bytes: bytes.into(),
            })),
            DaemonEvent::Close(id) => DaemonUdpOutgoing::Close(id),
        };
        DaemonMessage::UdpOutgoing(message)
    }
}
