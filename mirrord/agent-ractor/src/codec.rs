//! Framing for mirrord-protocol connections.
//!
//! mirrord-protocol implements its codec against `actix-codec`, whose `Framed`
//! cannot be split into independent read/write halves without a lock. Actors need
//! exactly that split: the reading task owns the decode half while the client
//! actor owns the encode half. [`TokioCodec`] re-exposes any actix codec through
//! `tokio-util`'s identical `Decoder`/`Encoder` traits, so the two halves can live
//! on `OwnedReadHalf`/`OwnedWriteHalf` with no shared state at all.
//!
//! The client-side aliases exist for the verification and benchmark binaries,
//! which speak the protocol from the other end.

use bytes::BytesMut;
use mirrord_protocol::{ClientCodec, DaemonCodec};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

/// Adapter that turns an `actix-codec` codec into a `tokio-util` one.
#[derive(Default)]
pub struct TokioCodec<C>(C);

impl<C: actix_codec::Decoder> tokio_util::codec::Decoder for TokioCodec<C> {
    type Item = C::Item;
    type Error = C::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.0.decode(src)
    }
}

impl<C: actix_codec::Encoder<O>, O> tokio_util::codec::Encoder<O> for TokioCodec<C> {
    type Error = C::Error;

    fn encode(&mut self, item: O, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.0.encode(item, dst)
    }
}

/// Decoding half of a client connection, owned by the client's reader task.
pub type ClientRx = FramedRead<OwnedReadHalf, TokioCodec<DaemonCodec>>;
/// Encoding half of a client connection, owned by the client actor.
pub type ClientTx = FramedWrite<OwnedWriteHalf, TokioCodec<DaemonCodec>>;

/// Splits a client connection for the agent side:
/// [`ClientMessage`](mirrord_protocol::ClientMessage)s in,
/// [`DaemonMessage`](mirrord_protocol::DaemonMessage)s out.
pub fn split_client_connection(stream: tokio::net::TcpStream) -> (ClientRx, ClientTx) {
    let (read, write) = stream.into_split();
    (
        FramedRead::new(read, TokioCodec::default()),
        FramedWrite::new(write, TokioCodec::default()),
    )
}

/// Decoding half of an agent connection, for client binaries.
pub type AgentRx = FramedRead<OwnedReadHalf, TokioCodec<ClientCodec>>;
/// Encoding half of an agent connection, for client binaries.
pub type AgentTx = FramedWrite<OwnedWriteHalf, TokioCodec<ClientCodec>>;

/// Splits an agent connection for the client side:
/// [`DaemonMessage`](mirrord_protocol::DaemonMessage)s in,
/// [`ClientMessage`](mirrord_protocol::ClientMessage)s out.
pub fn split_agent_connection(stream: tokio::net::TcpStream) -> (AgentRx, AgentTx) {
    let (read, write) = stream.into_split();
    (
        FramedRead::new(read, TokioCodec::default()),
        FramedWrite::new(write, TokioCodec::default()),
    )
}
