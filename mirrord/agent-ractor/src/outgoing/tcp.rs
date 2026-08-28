//! TCP and UNIX stream flavor of the outgoing feature. Both transports share one
//! protocol message family (`LayerTcpOutgoing`/`DaemonTcpOutgoing`), the address
//! decides which one is dialed.

use std::{ffi::OsStr, io, os::unix::ffi::OsStrExt};

use bytes::{Bytes, BytesMut};
use mirrord_protocol::{
    DaemonMessage,
    outgoing::{
        DaemonConnect, DaemonConnectV2, DaemonRead, SocketAddress, UnixAddr, tcp::DaemonTcpOutgoing,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UnixStream, tcp, unix},
};

use crate::outgoing::{DaemonEvent, EstablishedConn, OutgoingFlavor};

/// Matches the read buffer size mirrord-agent gives `ReaderStream`.
const READ_BUFFER_CAPACITY: usize = 64 * 1024;

pub struct TcpFlavor;

pub enum Writer {
    Tcp(tcp::OwnedWriteHalf),
    Unix(unix::OwnedWriteHalf),
}

pub enum Reader {
    Tcp(tcp::OwnedReadHalf),
    Unix(unix::OwnedReadHalf),
}

fn convert_unix_addr(addr: unix::SocketAddr) -> UnixAddr {
    if let Some(path) = addr.as_pathname() {
        UnixAddr::Pathname(path.to_path_buf())
    } else if let Some(name) = addr.as_abstract_name() {
        UnixAddr::Abstract(name.to_vec())
    } else {
        UnixAddr::Unnamed
    }
}

impl OutgoingFlavor for TcpFlavor {
    const NAME: &'static str = "tcp-out";

    type Writer = Writer;
    type Reader = Reader;

    async fn connect(address: SocketAddress) -> io::Result<EstablishedConn<Self>> {
        match address {
            SocketAddress::Ip(addr) => {
                let stream = TcpStream::connect(addr).await?;
                // Writes on this socket are chunks relayed from the local application,
                // which already went through its own socket. Nagle would only add latency.
                if let Err(error) = stream.set_nodelay(true) {
                    tracing::warn!(
                        %error,
                        peer_addr = %addr,
                        "Failed to set TCP_NODELAY on an outgoing TCP connection socket",
                    );
                }
                let local_address = SocketAddress::Ip(stream.local_addr()?);
                let peer_address = SocketAddress::Ip(stream.peer_addr()?);
                let (read, write) = stream.into_split();
                Ok(EstablishedConn {
                    reader: Reader::Tcp(read),
                    writer: Writer::Tcp(write),
                    local_address,
                    peer_address,
                })
            }

            SocketAddress::Unix(unix_addr) => {
                let stream = match unix_addr {
                    UnixAddr::Pathname(path) => UnixStream::connect(path).await?,
                    UnixAddr::Abstract(mut name) => {
                        // Abstract names are "paths" that start with a NUL byte.
                        name.insert(0, 0);
                        UnixStream::connect(OsStr::from_bytes(&name)).await?
                    }
                    UnixAddr::Unnamed => {
                        return Err(io::Error::other("unexpected unnamed UNIX address"));
                    }
                };
                let local_address = SocketAddress::Unix(convert_unix_addr(stream.local_addr()?));
                let peer_address = SocketAddress::Unix(convert_unix_addr(stream.peer_addr()?));
                let (read, write) = stream.into_split();
                Ok(EstablishedConn {
                    reader: Reader::Unix(read),
                    writer: Writer::Unix(write),
                    local_address,
                    peer_address,
                })
            }
        }
    }

    async fn write(writer: &mut Self::Writer, bytes: Bytes) -> io::Result<()> {
        match writer {
            Writer::Tcp(write) => write.write_all(&bytes).await,
            Writer::Unix(write) => write.write_all(&bytes).await,
        }
    }

    async fn shutdown(writer: &mut Self::Writer) -> io::Result<()> {
        match writer {
            Writer::Tcp(write) => write.shutdown().await,
            Writer::Unix(write) => write.shutdown().await,
        }
    }

    async fn read(reader: &mut Self::Reader) -> io::Result<Option<Bytes>> {
        let mut buffer = BytesMut::with_capacity(READ_BUFFER_CAPACITY);
        let read = match reader {
            Reader::Tcp(read) => read.read_buf(&mut buffer).await?,
            Reader::Unix(read) => read.read_buf(&mut buffer).await?,
        };
        Ok((read > 0).then(|| buffer.freeze()))
    }

    fn daemon_message(event: DaemonEvent) -> DaemonMessage {
        let message = match event {
            DaemonEvent::ConnectOk {
                uid: None,
                id,
                local_address,
                peer_address,
            } => DaemonTcpOutgoing::Connect(Ok(DaemonConnect {
                connection_id: id,
                remote_address: peer_address,
                local_address,
            })),
            DaemonEvent::ConnectOk {
                uid: Some(uid),
                id,
                local_address,
                peer_address,
            } => DaemonTcpOutgoing::ConnectV2(DaemonConnectV2 {
                uid,
                connect: Ok(DaemonConnect {
                    connection_id: id,
                    remote_address: peer_address,
                    local_address,
                }),
            }),
            DaemonEvent::ConnectErr { uid: None, error } => {
                DaemonTcpOutgoing::Connect(Err(error.into()))
            }
            DaemonEvent::ConnectErr {
                uid: Some(uid),
                error,
            } => DaemonTcpOutgoing::ConnectV2(DaemonConnectV2 {
                uid,
                connect: Err(error.into()),
            }),
            DaemonEvent::Read(id, bytes) => DaemonTcpOutgoing::Read(Ok(DaemonRead {
                connection_id: id,
                bytes: bytes.into(),
            })),
            DaemonEvent::Close(id) => DaemonTcpOutgoing::Close(id),
        };
        DaemonMessage::TcpOutgoing(message)
    }
}
