//! One actor per connected client (an intproxy or the operator).
//!
//! The client actor owns the *write* half of the client connection and the
//! session-wide state (negotiated protocol version, log readiness). Everything
//! any part of the agent wants to tell the client goes through its mailbox as a
//! [`ClientMsg::Send`], which serializes writes without any channel juggling.
//!
//! The *read* half lives in a plain tokio task ([`reader_task`]) that decodes
//! [`ClientMessage`]s and routes each one directly to the actor that handles it
//! (DNS actor, an outgoing router, or the client actor itself for session-level
//! messages). Routing in the reader keeps the hot data path short: a client write
//! travels reader -> router -> connection actor, without bouncing through this
//! actor's mailbox.
//!
//! The feature actors are spawned linked to this actor, so the whole session is
//! torn down as one subtree when the client disconnects.

use std::{collections::HashMap, io};

use futures::{SinkExt, StreamExt};
use mirrord_protocol::{ClientMessage, DaemonMessage};
use ractor::{Actor, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio::net::TcpStream;

use crate::{
    budget::{self, BudgetPermit, MemoryBudget},
    codec::{ClientRx, ClientTx, split_client_connection},
    dns::{DnsActor, DnsArgs, DnsMsg},
    outgoing::{
        LayerEvent,
        router::{OutgoingRouterActor, RouterArgs, RouterMsg},
        tcp::TcpFlavor,
        udp::UdpFlavor,
    },
    util::{ClientId, TaskGuard},
};

/// Process group of all client actors, joined for observability.
pub const CLIENTS_GROUP: &str = "clients";

// `Send` dominates the enum size because of the inlined `DaemonMessage`; boxing it
// would put an allocation on every relayed data chunk, which is the hot path.
#[allow(clippy::large_enum_variant)]
pub enum ClientMsg {
    /// Send a message to the client. The budget permit (for peer data chunks) is
    /// released after the message has been written and flushed.
    Send {
        message: DaemonMessage,
        budget: Option<BudgetPermit>,
    },
    Ping,
    SwitchProtocolVersion(semver::Version),
    ReadyForLogs,
    GetEnvVars,
    /// The client sent a message for a feature outside this spike's scope
    /// (files, incoming traffic, vpn, ...).
    NotSupported {
        feature: &'static str,
    },
    /// The client sent [`ClientMessage::Close`].
    CloseRequested,
    /// The reader task finished: clean disconnect (`None`) or connection error.
    ReaderFinished(Option<io::Error>),
}

pub struct ClientArgs {
    pub id: ClientId,
    pub stream: TcpStream,
}

pub struct ClientState {
    id: ClientId,
    tx: ClientTx,
    /// Whether the client has sent [`ClientMessage::ReadyForLogs`].
    ready_for_logs: bool,
    /// Client's version of [`mirrord_protocol`], from `SwitchProtocolVersion`.
    protocol_version: Option<semver::Version>,
    _reader: TaskGuard,
}

pub struct ClientActor;

impl Actor for ClientActor {
    type Msg = ClientMsg;
    type State = ClientState;
    type Arguments = ClientArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let ClientArgs { id, stream } = args;

        let (rx, tx) = split_client_connection(stream);

        let to_client_budget = MemoryBudget::new(budget::TO_CLIENT_LIMIT);
        let from_client_budget = MemoryBudget::new(budget::FROM_CLIENT_LIMIT);

        let (dns, _) = Actor::spawn_linked(
            Some(format!("client-{id}.dns")),
            DnsActor,
            DnsArgs {
                client: myself.clone(),
            },
            myself.get_cell(),
        )
        .await?;

        let router_args = || RouterArgs {
            client_id: id,
            client: myself.clone(),
            to_client_budget: to_client_budget.clone(),
        };
        let (tcp_outgoing, _) = Actor::spawn_linked(
            Some(format!("client-{id}.tcp-out")),
            OutgoingRouterActor::<TcpFlavor>::default(),
            router_args(),
            myself.get_cell(),
        )
        .await?;
        let (udp_outgoing, _) = Actor::spawn_linked(
            Some(format!("client-{id}.udp-out")),
            OutgoingRouterActor::<UdpFlavor>::default(),
            router_args(),
            myself.get_cell(),
        )
        .await?;

        let reader = tokio::spawn(reader_task(
            rx,
            SessionRefs {
                client: myself.clone(),
                dns,
                tcp_outgoing,
                udp_outgoing,
                from_client_budget,
            },
        ));

        ractor::pg::join(CLIENTS_GROUP.to_owned(), vec![myself.get_cell()]);

        tracing::info!(client_id = id, "Client session started");

        Ok(ClientState {
            id,
            tx,
            ready_for_logs: false,
            protocol_version: None,
            _reader: TaskGuard::new(reader),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ClientMsg::Send { message, budget } => {
                if matches!(&message, DaemonMessage::LogMessage(..)) && !state.ready_for_logs {
                    return Ok(());
                }
                state.tx.send(message).await?;
                drop(budget);
            }

            ClientMsg::Ping => {
                state.tx.send(DaemonMessage::Pong).await?;
            }

            ClientMsg::SwitchProtocolVersion(client_version) => {
                let settled = (&*mirrord_protocol::VERSION).min(&client_version).clone();
                state.protocol_version = Some(client_version);
                state
                    .tx
                    .send(DaemonMessage::SwitchProtocolVersionResponse(settled))
                    .await?;
            }

            ClientMsg::ReadyForLogs => {
                state.ready_for_logs = true;
            }

            ClientMsg::GetEnvVars => {
                // Targetless agents have no meaningful target environment;
                // an empty set keeps env-requesting clients working.
                state
                    .tx
                    .send(DaemonMessage::GetEnvVarsResponse(Ok(HashMap::new().into())))
                    .await?;
            }

            ClientMsg::NotSupported { feature } => {
                tracing::warn!(
                    client_id = state.id,
                    feature,
                    "Client requested a feature that is not supported by this agent",
                );
                state
                    .tx
                    .send(DaemonMessage::Close(format!(
                        "{feature} is not supported by mirrord-agent-ractor",
                    )))
                    .await?;
            }

            ClientMsg::CloseRequested => {
                myself.stop(Some("client sent Close".to_owned()));
            }

            ClientMsg::ReaderFinished(None) => {
                tracing::info!(client_id = state.id, "Client disconnected");
                myself.stop(Some("client disconnected".to_owned()));
            }

            ClientMsg::ReaderFinished(Some(error)) => {
                return Err(format!("client connection failed: {error}").into());
            }
        }

        Ok(())
    }

    /// The feature actors live exactly as long as the session, so any child ending
    /// on its own means the session is broken: tell the client and fail, tearing
    /// down the rest of the subtree.
    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match event {
            SupervisionEvent::ActorFailed(cell, error) => {
                let name = cell.get_name().unwrap_or_default();
                let _ = state
                    .tx
                    .send(DaemonMessage::Close(format!(
                        "agent failed serving this session: {error}"
                    )))
                    .await;
                Err(format!("feature actor {name} failed: {error}").into())
            }

            SupervisionEvent::ActorTerminated(cell, _, reason) => {
                let name = cell.get_name().unwrap_or_default();
                Err(
                    format!("feature actor {name} stopped unexpectedly (reason: {reason:?})")
                        .into(),
                )
            }

            _ => Ok(()),
        }
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        tracing::info!(client_id = state.id, "Client session finished");
        Ok(())
    }
}

/// Everything the reader task needs to route decoded client messages.
struct SessionRefs {
    client: ActorRef<ClientMsg>,
    dns: ActorRef<DnsMsg>,
    tcp_outgoing: ActorRef<RouterMsg>,
    udp_outgoing: ActorRef<RouterMsg>,
    from_client_budget: MemoryBudget,
}

/// Decodes client messages and routes each to its handling actor.
///
/// Outgoing write chunks reserve memory budget *before* being cast, so a client
/// that outpaces its peers is paused right here - the socket read stops, and TCP
/// backpressure does the rest.
async fn reader_task(mut rx: ClientRx, refs: SessionRefs) {
    let error = loop {
        match rx.next().await {
            None => break None,
            Some(Err(error)) => break Some(error),
            Some(Ok(message)) => {
                if dispatch(message, &refs).await.is_err() {
                    // The target actor is gone, so the session is tearing down.
                    return;
                }
            }
        }
    };
    let _ = refs.client.cast(ClientMsg::ReaderFinished(error));
}

/// Error type for "the destination actor is dead"; details are irrelevant because
/// the session is going down anyway.
struct SessionClosed;

impl<T> From<ractor::MessagingErr<T>> for SessionClosed {
    fn from(_: ractor::MessagingErr<T>) -> Self {
        Self
    }
}

async fn dispatch(message: ClientMessage, refs: &SessionRefs) -> Result<(), SessionClosed> {
    match message {
        ClientMessage::TcpOutgoing(message) => {
            dispatch_outgoing(&refs.tcp_outgoing, message.into(), refs).await
        }
        ClientMessage::UdpOutgoing(message) => {
            dispatch_outgoing(&refs.udp_outgoing, message.into(), refs).await
        }

        ClientMessage::GetAddrInfoRequest(request) => {
            refs.dns.cast(DnsMsg::Lookup(request.into()))?;
            Ok(())
        }
        ClientMessage::GetAddrInfoRequestV2(request) => {
            refs.dns.cast(DnsMsg::Lookup(request))?;
            Ok(())
        }

        ClientMessage::Ping => Ok(refs.client.cast(ClientMsg::Ping)?),
        ClientMessage::SwitchProtocolVersion(version) => Ok(refs
            .client
            .cast(ClientMsg::SwitchProtocolVersion(version))?),
        ClientMessage::ReadyForLogs => Ok(refs.client.cast(ClientMsg::ReadyForLogs)?),
        ClientMessage::GetEnvVarsRequest(..) => Ok(refs.client.cast(ClientMsg::GetEnvVars)?),
        ClientMessage::Close => Ok(refs.client.cast(ClientMsg::CloseRequested)?),

        // Handled exclusively by the operator, see mirrord-agent for details.
        ClientMessage::OperatorPong(..) => Ok(()),
        // Operator-managed share link keys; meaningless without the incoming
        // traffic features.
        ClientMessage::ShareLink(..) => Ok(()),

        ClientMessage::FileRequest(..) => not_supported(refs, "file operations"),
        ClientMessage::Tcp(..) => not_supported(refs, "incoming traffic mirroring"),
        ClientMessage::TcpSteal(..) => not_supported(refs, "incoming traffic stealing"),
        ClientMessage::SeqpacketOutgoing(..) => not_supported(refs, "seqpacket outgoing traffic"),
        ClientMessage::ReverseDnsLookup(..) => not_supported(refs, "reverse DNS lookup"),
        ClientMessage::Vpn(..) => not_supported(refs, "vpn"),
        ClientMessage::PauseTargetRequest(..) => not_supported(refs, "target pause"),
    }
}

fn not_supported(refs: &SessionRefs, feature: &'static str) -> Result<(), SessionClosed> {
    Ok(refs.client.cast(ClientMsg::NotSupported { feature })?)
}

/// Routes one outgoing feature request to the flavor's router.
async fn dispatch_outgoing(
    router: &ActorRef<RouterMsg>,
    event: LayerEvent,
    refs: &SessionRefs,
) -> Result<(), SessionClosed> {
    match event {
        LayerEvent::Connect {
            request_uid,
            address,
        } => Ok(router.cast(RouterMsg::Connect {
            request_uid,
            address,
        })?),
        LayerEvent::Write { id, bytes } if bytes.is_empty() => {
            Ok(router.cast(RouterMsg::ShutdownWrite { id })?)
        }
        LayerEvent::Write { id, bytes } => {
            let budget = refs.from_client_budget.reserve(bytes.len()).await;
            Ok(router.cast(RouterMsg::Write { id, bytes, budget })?)
        }
        LayerEvent::Close { id } => Ok(router.cast(RouterMsg::Close { id })?),
    }
}
