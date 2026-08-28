//! Framing for the client connection.
//!
//! mirrord-protocol implements its codec against `actix-codec`, whose `Framed`
//! cannot be split into independent read/write halves without a lock. Actors need
//! exactly that split: the reading task owns the decode half while the client
//! actor owns the encode half. This adapter re-exposes the protocol codec through
//! `tokio-util`'s identical `Decoder`/`Encoder` traits, so the two halves can live
//! on `OwnedReadHalf`/`OwnedWriteHalf` with no shared state at all.

use std::io;

use bytes::BytesMut;
use mirrord_protocol::{ClientMessage, DaemonCodec, DaemonMessage};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite};

#[derive(Default)]
pub struct DaemonSideCodec(DaemonCodec);

impl tokio_util::codec::Decoder for DaemonSideCodec {
    type Item = ClientMessage;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        actix_codec::Decoder::decode(&mut self.0, src)
    }
}

impl tokio_util::codec::Encoder<DaemonMessage> for DaemonSideCodec {
    type Error = io::Error;

    fn encode(&mut self, item: DaemonMessage, dst: &mut BytesMut) -> io::Result<()> {
        actix_codec::Encoder::encode(&mut self.0, item, dst)
    }
}

/// Decoding half of a client connection, owned by the client's reader task.
pub type ClientRx = FramedRead<OwnedReadHalf, DaemonSideCodec>;
/// Encoding half of a client connection, owned by the client actor.
pub type ClientTx = FramedWrite<OwnedWriteHalf, DaemonSideCodec>;

pub fn split_client_connection(stream: tokio::net::TcpStream) -> (ClientRx, ClientTx) {
    let (read, write) = stream.into_split();
    (
        FramedRead::new(read, DaemonSideCodec::default()),
        FramedWrite::new(write, DaemonSideCodec::default()),
    )
}
