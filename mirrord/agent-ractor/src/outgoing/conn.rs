//! One actor per outgoing connection.
//!
//! The actor owns the whole connection lifecycle: it dials the peer, owns the
//! write half, and holds the reader task that relays peer data straight to the
//! client actor (peer data skips the router - it needs no routing decision, and
//! that keeps the hot path at a single mailbox hop).
//!
//! ## Close protocol
//!
//! Mirrors the semantics of mirrord-agent's `OutgoingRouter`:
//! * peer closed its write side -> `Read(empty)` to the client, connection stays half-open for
//!   client writes;
//! * client sent empty write -> shut down the write side, connection stays half-open for peer
//!   reads;
//! * both sides closed, or any IO error -> `Close(id)` to the client and the actor stops (errors
//!   are additionally surfaced as a client log message, because the protocol has no error variant
//!   for established connections).
//!
//! The router only observes this actor's termination (via supervision) to clean up
//! its routing table; all protocol messages are emitted from here, in order.

use std::{io, marker::PhantomData, time::Duration};

use bytes::Bytes;
use mirrord_protocol::{
    ConnectionId, DaemonMessage, LogMessage, outgoing::SocketAddress, uid::Uid,
};
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tokio::time::timeout;

use crate::{
    budget::{BudgetPermit, MemoryBudget},
    client::ClientMsg,
    outgoing::{DaemonEvent, EstablishedConn, OutgoingFlavor, router::RouterMsg},
    util::TaskGuard,
};

/// Timeout for dialing the peer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for writing a single chunk to the peer, protects the actor from a
/// non-responsive connection.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub enum ConnMsg {
    /// Relay a chunk from the client to the peer. The permit is released once the
    /// chunk has been written out.
    Write { bytes: Bytes, budget: BudgetPermit },
    /// The client shut down its write direction (sent an empty write).
    ShutdownWrite,
    /// The client closed the connection; tear down silently.
    Close,
    /// The reader task saw EOF from the peer.
    PeerClosed,
    /// The reader task saw an IO error.
    PeerFailed(io::Error),
}

pub struct ConnArgs<F: OutgoingFlavor> {
    pub id: ConnectionId,
    /// `Some` for `ConnectV2` requests, `None` for legacy ordered requests.
    pub request_uid: Option<Uid>,
    pub address: SocketAddress,
    pub router: ActorRef<RouterMsg>,
    pub client: ActorRef<ClientMsg>,
    pub to_client_budget: MemoryBudget,
    pub _flavor: PhantomData<fn() -> F>,
}

pub struct ConnState<F: OutgoingFlavor> {
    id: ConnectionId,
    request_uid: Option<Uid>,
    address: Option<SocketAddress>,
    router: ActorRef<RouterMsg>,
    client: ActorRef<ClientMsg>,
    to_client_budget: MemoryBudget,
    /// `None` until connected, and after the write side is shut down.
    writer: Option<F::Writer>,
    /// Aborts the reader task on drop. `None` until connected, and after the peer
    /// closed its side (or failed).
    reader: Option<TaskGuard>,
}

pub struct OutgoingConnActor<F>(PhantomData<fn() -> F>);

impl<F> Default for OutgoingConnActor<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<F: OutgoingFlavor> OutgoingConnActor<F> {
    /// Reads peer data and casts it straight to the client actor. Every chunk
    /// reserves memory budget first, so a fast peer is paused (plain socket
    /// backpressure) instead of ballooning the client actor's mailbox.
    async fn reader_task(
        mut reader: F::Reader,
        id: ConnectionId,
        conn: ActorRef<ConnMsg>,
        client: ActorRef<ClientMsg>,
        budget: MemoryBudget,
    ) {
        loop {
            match F::read(&mut reader).await {
                Ok(Some(bytes)) => {
                    let budget = budget.reserve(bytes.len()).await;
                    let message = ClientMsg::Send {
                        message: F::daemon_message(DaemonEvent::Read(id, bytes)),
                        budget: Some(budget),
                    };
                    if client.cast(message).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = conn.cast(ConnMsg::PeerClosed);
                    break;
                }
                Err(error) => {
                    let _ = conn.cast(ConnMsg::PeerFailed(error));
                    break;
                }
            }
        }
    }

    /// Common teardown for IO failures: silence the reader first so no more peer
    /// data can be queued behind the `Close`, then tell the client and stop.
    fn fail(myself: &ActorRef<ConnMsg>, state: &mut ConnState<F>, error: io::Error, context: &str) {
        tracing::debug!(
            id = state.id,
            flavor = F::NAME,
            %error,
            context,
            "Outgoing connection failed",
        );
        state.reader = None;
        state.writer = None;
        let log = DaemonMessage::LogMessage(LogMessage::warn(format!(
            "outgoing connection {} failed: {error} ({})",
            state.id,
            F::NAME,
        )));
        let _ = state.client.cast(ClientMsg::Send {
            message: log,
            budget: None,
        });
        Self::close_and_stop(myself, state);
    }

    fn close_and_stop(myself: &ActorRef<ConnMsg>, state: &ConnState<F>) {
        let _ = state.client.cast(ClientMsg::Send {
            message: F::daemon_message(DaemonEvent::Close(state.id)),
            budget: None,
        });
        myself.stop(Some("connection closed".to_owned()));
    }
}

impl<F: OutgoingFlavor> Actor for OutgoingConnActor<F> {
    type Msg = ConnMsg;
    type State = ConnState<F>;
    type Arguments = ConnArgs<F>;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(ConnState {
            id: args.id,
            request_uid: args.request_uid,
            address: Some(args.address),
            router: args.router,
            client: args.client,
            to_client_budget: args.to_client_budget,
            writer: None,
            reader: None,
        })
    }

    /// Dials the peer. This runs on the actor's own message loop, so a slow
    /// connect never blocks the router (client messages for this connection cannot
    /// arrive before the client learns the connection ID from the connect
    /// response, so nothing meaningful queues up behind it).
    ///
    /// The connect response is cast to the client from *here*, before the reader
    /// task starts, so it is guaranteed to reach the client before any
    /// `Read`/`Close` for the same connection. The router only gets a bookkeeping
    /// notification (it drives the legacy request queue with it).
    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let address = state
            .address
            .take()
            .expect("the peer address is only consumed here");
        tracing::debug!(
            id = state.id,
            flavor = F::NAME,
            address = %address,
            "Dialing an outgoing connection",
        );

        let connected = timeout(CONNECT_TIMEOUT, F::connect(address))
            .await
            .unwrap_or_else(|_elapsed| Err(io::ErrorKind::TimedOut.into()));

        match connected {
            Ok(EstablishedConn {
                reader,
                writer,
                local_address,
                peer_address,
            }) => {
                state.client.cast(ClientMsg::Send {
                    message: F::daemon_message(DaemonEvent::ConnectOk {
                        uid: state.request_uid,
                        id: state.id,
                        local_address,
                        peer_address,
                    }),
                    budget: None,
                })?;
                state.router.cast(RouterMsg::ConnectFinished {
                    id: state.id,
                    request_uid: state.request_uid,
                    connected: true,
                })?;
                let reader_task = tokio::spawn(Self::reader_task(
                    reader,
                    state.id,
                    myself.clone(),
                    state.client.clone(),
                    state.to_client_budget.clone(),
                ));
                state.reader = Some(TaskGuard::new(reader_task));
                state.writer = Some(writer);
            }
            Err(error) => {
                tracing::debug!(
                    id = state.id,
                    flavor = F::NAME,
                    %error,
                    "Failed to make an outgoing connection",
                );
                state.client.cast(ClientMsg::Send {
                    message: F::daemon_message(DaemonEvent::ConnectErr {
                        uid: state.request_uid,
                        error,
                    }),
                    budget: None,
                })?;
                state.router.cast(RouterMsg::ConnectFinished {
                    id: state.id,
                    request_uid: state.request_uid,
                    connected: false,
                })?;
                myself.stop(Some("connect failed".to_owned()));
            }
        }

        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ConnMsg::Write { bytes, budget } => {
                // A write can legitimately race with write-side teardown; the
                // chunk is dropped like mirrord-agent drops writes to removed sinks.
                let Some(writer) = state.writer.as_mut() else {
                    return Ok(());
                };
                let result = timeout(WRITE_TIMEOUT, F::write(writer, bytes))
                    .await
                    .unwrap_or_else(|_elapsed| Err(io::ErrorKind::TimedOut.into()));
                if let Err(error) = result {
                    Self::fail(&myself, state, error, "writing to the peer");
                }
                drop(budget);
            }

            ConnMsg::ShutdownWrite => {
                let Some(mut writer) = state.writer.take() else {
                    return Ok(());
                };
                match F::shutdown(&mut writer).await {
                    Ok(()) if state.reader.is_none() => Self::close_and_stop(&myself, state),
                    Ok(()) => {}
                    Err(error) => Self::fail(&myself, state, error, "shutting down the write side"),
                }
            }

            ConnMsg::Close => {
                myself.stop(Some("closed by the client".to_owned()));
            }

            ConnMsg::PeerClosed => {
                state.reader = None;
                if state.writer.is_some() {
                    state.client.cast(ClientMsg::Send {
                        message: F::daemon_message(DaemonEvent::Read(state.id, Bytes::new())),
                        budget: None,
                    })?;
                } else {
                    Self::close_and_stop(&myself, state);
                }
            }

            ConnMsg::PeerFailed(error) => {
                Self::fail(&myself, state, error, "reading from the peer");
            }
        }

        Ok(())
    }
}
