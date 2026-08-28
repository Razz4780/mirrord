//! Outgoing traffic feature: the agent makes connections on the client's behalf
//! and relays data in both directions.
//!
//! The mirrord protocol has one message family per transport (TCP/UNIX-stream vs
//! UDP) with identical shapes. [`OutgoingFlavor`] captures the per-transport
//! differences (how to connect, read, write, and how to wrap events back into
//! [`DaemonMessage`]s), so the router and connection actors are written once and
//! instantiated per flavor.

use std::{fmt, future::Future, io};

use bytes::Bytes;
use mirrord_protocol::{
    ConnectionId, DaemonMessage,
    outgoing::{SocketAddress, tcp::LayerTcpOutgoing, udp::LayerUdpOutgoing},
    uid::Uid,
};

pub mod conn;
pub mod router;
pub mod tcp;
pub mod udp;

/// Per-transport behavior of the outgoing feature.
///
/// Implementors are zero-sized markers; connection IO state lives in
/// [`conn::OutgoingConnActor`]'s state.
pub trait OutgoingFlavor: 'static + Sized + Send + Sync {
    /// Used in actor names (`client-{N}.{NAME}`), process group names and logs.
    const NAME: &'static str;

    /// Write half of an established connection.
    type Writer: Send + 'static;
    /// Read half of an established connection.
    type Reader: Send + 'static;

    fn connect(
        address: SocketAddress,
    ) -> impl Future<Output = io::Result<EstablishedConn<Self>>> + Send;

    fn write(
        writer: &mut Self::Writer,
        bytes: Bytes,
    ) -> impl Future<Output = io::Result<()>> + Send;

    /// Signals end-of-writes to the peer, where the transport supports it.
    fn shutdown(writer: &mut Self::Writer) -> impl Future<Output = io::Result<()>> + Send;

    /// Reads the next data chunk from the peer. `None` means the peer closed its
    /// write side.
    fn read(reader: &mut Self::Reader) -> impl Future<Output = io::Result<Option<Bytes>>> + Send;

    /// Wraps a transport-agnostic event into the flavor's [`DaemonMessage`] family.
    fn daemon_message(event: DaemonEvent) -> DaemonMessage;
}

/// Client requests of the outgoing feature, decoupled from the per-flavor
/// protocol message families.
pub enum LayerEvent {
    Connect {
        /// `None` for the legacy `Connect` request, `Some` for `ConnectV2`.
        request_uid: Option<Uid>,
        address: SocketAddress,
    },
    /// Data for an established connection. Empty bytes are the client's
    /// write-side shutdown signal, per the mirrord protocol.
    Write {
        id: ConnectionId,
        bytes: Bytes,
    },
    Close {
        id: ConnectionId,
    },
}

impl From<LayerTcpOutgoing> for LayerEvent {
    fn from(message: LayerTcpOutgoing) -> Self {
        match message {
            LayerTcpOutgoing::Connect(connect) => Self::Connect {
                request_uid: None,
                address: connect.remote_address,
            },
            LayerTcpOutgoing::ConnectV2(connect) => Self::Connect {
                request_uid: Some(connect.uid),
                address: connect.remote_address,
            },
            LayerTcpOutgoing::Write(write) => Self::Write {
                id: write.connection_id,
                bytes: write.bytes.0,
            },
            LayerTcpOutgoing::Close(close) => Self::Close {
                id: close.connection_id,
            },
        }
    }
}

impl From<LayerUdpOutgoing> for LayerEvent {
    fn from(message: LayerUdpOutgoing) -> Self {
        match message {
            LayerUdpOutgoing::Connect(connect) => Self::Connect {
                request_uid: None,
                address: connect.remote_address,
            },
            LayerUdpOutgoing::ConnectV2(connect) => Self::Connect {
                request_uid: Some(connect.uid),
                address: connect.remote_address,
            },
            LayerUdpOutgoing::Write(write) => Self::Write {
                id: write.connection_id,
                bytes: write.bytes.0,
            },
            LayerUdpOutgoing::Close(close) => Self::Close {
                id: close.connection_id,
            },
        }
    }
}

/// A freshly established outgoing connection.
pub struct EstablishedConn<F: OutgoingFlavor> {
    pub reader: F::Reader,
    pub writer: F::Writer,
    pub local_address: SocketAddress,
    pub peer_address: SocketAddress,
}

/// Transport-agnostic events produced by the outgoing actors, one-to-one with the
/// per-flavor `Daemon*Outgoing` protocol messages.
pub enum DaemonEvent {
    ConnectOk {
        /// `None` for the legacy `Connect` request, `Some` for `ConnectV2`.
        uid: Option<Uid>,
        id: ConnectionId,
        local_address: SocketAddress,
        peer_address: SocketAddress,
    },
    ConnectErr {
        uid: Option<Uid>,
        error: io::Error,
    },
    /// Data read from the peer. Empty bytes mean the peer shut down its write side.
    Read(ConnectionId, Bytes),
    /// The connection is gone and the client should forget its ID.
    Close(ConnectionId),
}

impl fmt::Debug for DaemonEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectOk { uid, id, .. } => f
                .debug_struct("ConnectOk")
                .field("uid", uid)
                .field("id", id)
                .finish_non_exhaustive(),
            Self::ConnectErr { uid, error } => f
                .debug_struct("ConnectErr")
                .field("uid", uid)
                .field("error", error)
                .finish(),
            Self::Read(id, bytes) => write!(f, "Read({id}, {} bytes)", bytes.len()),
            Self::Close(id) => write!(f, "Close({id})"),
        }
    }
}
