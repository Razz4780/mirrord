//! One router actor per client per outgoing flavor.
//!
//! Owns the `ConnectionId -> connection actor` table and nothing else: client
//! requests are forwarded to the right connection actor, and the table is kept in
//! sync purely through supervision events (every connection actor is spawned
//! linked to its router, so its termination - clean stop, failure, or the kill
//! cascade - always comes back here).
//!
//! Also enforces the ordering contract of the legacy `Connect` request (responses
//! must arrive in request order), by dialing legacy requests one at a time.
//! `ConnectV2` requests carry a UID, so they are dialed concurrently.

use std::{
    collections::{HashMap, VecDeque},
    marker::PhantomData,
};

use bytes::Bytes;
use mirrord_protocol::{
    ConnectionId, DaemonMessage, LogMessage, outgoing::SocketAddress, uid::Uid,
};
use ractor::{Actor, ActorId, ActorProcessingErr, ActorRef, SupervisionEvent};

use crate::{
    budget::{BudgetPermit, MemoryBudget},
    client::ClientMsg,
    outgoing::{
        DaemonEvent, OutgoingFlavor,
        conn::{ConnArgs, ConnMsg, OutgoingConnActor},
    },
    util::ClientId,
};

pub enum RouterMsg {
    /// Client wants a new outgoing connection.
    Connect {
        /// `None` marks a legacy request, which is subject to FIFO ordering.
        request_uid: Option<Uid>,
        address: SocketAddress,
    },
    /// Client data for an established connection.
    Write {
        id: ConnectionId,
        bytes: Bytes,
        budget: BudgetPermit,
    },
    /// Client shut down its write direction for this connection.
    ShutdownWrite { id: ConnectionId },
    /// Client closed this connection.
    Close { id: ConnectionId },
    /// A connection actor finished its dial attempt (the client was already
    /// notified by the connection actor itself).
    ConnectFinished {
        id: ConnectionId,
        request_uid: Option<Uid>,
        connected: bool,
    },
}

pub struct RouterArgs {
    pub client_id: ClientId,
    pub client: ActorRef<ClientMsg>,
    pub to_client_budget: MemoryBudget,
}

pub struct RouterState<F: OutgoingFlavor> {
    client_id: ClientId,
    client: ActorRef<ClientMsg>,
    to_client_budget: MemoryBudget,
    next_conn_id: ConnectionId,
    conns: HashMap<ConnectionId, ActorRef<ConnMsg>>,
    /// Maps supervision events (which carry only actor identity) back to
    /// connection IDs.
    conn_ids: HashMap<ActorId, ConnectionId>,
    /// Legacy connect requests waiting for their turn.
    queued_legacy: VecDeque<SocketAddress>,
    /// Whether a legacy dial is currently in flight. At most one runs at a time,
    /// which is what keeps legacy responses in request order.
    legacy_in_flight: bool,
    _flavor: PhantomData<fn() -> F>,
}

pub struct OutgoingRouterActor<F>(PhantomData<fn() -> F>);

impl<F> Default for OutgoingRouterActor<F> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<F: OutgoingFlavor> OutgoingRouterActor<F> {
    /// Process group that all connection actors of this flavor join,
    /// e.g. `tcp-out-conns`. Lets the root actor report live connection counts.
    pub fn conns_group() -> String {
        format!("{}-conns", F::NAME)
    }

    async fn spawn_conn(
        myself: &ActorRef<RouterMsg>,
        state: &mut RouterState<F>,
        request_uid: Option<Uid>,
        address: SocketAddress,
    ) -> Result<(), ActorProcessingErr> {
        let id = state.next_conn_id;
        state.next_conn_id = state
            .next_conn_id
            .checked_add(1)
            .ok_or("exhausted u64 connection IDs")?;

        let name = format!("client-{}.{}.conn-{id}", state.client_id, F::NAME);
        let (conn_ref, _join) = Actor::spawn_linked(
            Some(name),
            OutgoingConnActor::<F>::default(),
            ConnArgs {
                id,
                request_uid,
                address,
                router: myself.clone(),
                client: state.client.clone(),
                to_client_budget: state.to_client_budget.clone(),
                _flavor: PhantomData,
            },
            myself.get_cell(),
        )
        .await?;

        ractor::pg::join(Self::conns_group(), vec![conn_ref.get_cell()]);
        state.conn_ids.insert(conn_ref.get_id(), id);
        state.conns.insert(id, conn_ref);

        Ok(())
    }
}

impl<F: OutgoingFlavor> Actor for OutgoingRouterActor<F> {
    type Msg = RouterMsg;
    type State = RouterState<F>;
    type Arguments = RouterArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(RouterState {
            client_id: args.client_id,
            client: args.client,
            to_client_budget: args.to_client_budget,
            next_conn_id: 0,
            conns: HashMap::new(),
            conn_ids: HashMap::new(),
            queued_legacy: VecDeque::new(),
            legacy_in_flight: false,
            _flavor: PhantomData,
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            RouterMsg::Connect {
                request_uid: request_uid @ Some(..),
                address,
            } => {
                Self::spawn_conn(&myself, state, request_uid, address).await?;
            }

            RouterMsg::Connect {
                request_uid: None,
                address,
            } => {
                if state.legacy_in_flight {
                    state.queued_legacy.push_back(address);
                } else {
                    state.legacy_in_flight = true;
                    Self::spawn_conn(&myself, state, None, address).await?;
                }
            }

            RouterMsg::ConnectFinished {
                id,
                request_uid,
                connected,
            } => {
                tracing::trace!(
                    id,
                    flavor = F::NAME,
                    ?request_uid,
                    connected,
                    "Outgoing dial attempt finished",
                );
                if request_uid.is_none() {
                    state.legacy_in_flight = false;
                    if let Some(address) = state.queued_legacy.pop_front() {
                        state.legacy_in_flight = true;
                        Self::spawn_conn(&myself, state, None, address).await?;
                    }
                }
            }

            // Data/close for an unknown ID is dropped: teardown legitimately races
            // with in-flight client messages.
            RouterMsg::Write { id, bytes, budget } => {
                if let Some(conn) = state.conns.get(&id) {
                    let _ = conn.cast(ConnMsg::Write { bytes, budget });
                }
            }

            RouterMsg::ShutdownWrite { id } => {
                if let Some(conn) = state.conns.get(&id) {
                    let _ = conn.cast(ConnMsg::ShutdownWrite);
                }
            }

            RouterMsg::Close { id } => {
                if let Some(conn) = state.conns.get(&id) {
                    let _ = conn.cast(ConnMsg::Close);
                }
            }
        }

        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match event {
            SupervisionEvent::ActorTerminated(cell, _, reason) => {
                if let Some(id) = state.conn_ids.remove(&cell.get_id()) {
                    state.conns.remove(&id);
                    tracing::trace!(
                        id,
                        flavor = F::NAME,
                        ?reason,
                        "Outgoing connection actor is gone"
                    );
                }
            }

            // The connection actor emits all its protocol messages itself, so a
            // failure here means it died without saying goodbye (panic / internal
            // error) and the client must be told to forget the connection.
            SupervisionEvent::ActorFailed(cell, error) => {
                if let Some(id) = state.conn_ids.remove(&cell.get_id()) {
                    state.conns.remove(&id);
                    tracing::warn!(id, flavor = F::NAME, %error, "Outgoing connection actor failed");
                    let log = DaemonMessage::LogMessage(LogMessage::warn(format!(
                        "outgoing connection {id} failed: {error} ({})",
                        F::NAME,
                    )));
                    let _ = state.client.cast(ClientMsg::Send {
                        message: log,
                        budget: None,
                    });
                    let _ = state.client.cast(ClientMsg::Send {
                        message: F::daemon_message(DaemonEvent::Close(id)),
                        budget: None,
                    });
                }
            }

            _ => {}
        }

        Ok(())
    }
}
