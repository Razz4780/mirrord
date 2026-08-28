//! Root of the supervision tree: accepts client connections and spawns one
//! [`ClientActor`] subtree per client.
//!
//! Also the observability anchor of the agent: because every actor is registered
//! by name and joined to a process group, the root can periodically report a live
//! snapshot of the whole tree (clients, outgoing connections per flavor) without
//! asking anyone.

use std::{collections::HashMap, io, net::SocketAddr, time::Duration};

use ractor::{Actor, ActorId, ActorProcessingErr, ActorRef, SupervisionEvent};
use tokio::net::{TcpListener, TcpStream};

use crate::{
    client::{CLIENTS_GROUP, ClientActor, ClientArgs},
    outgoing::{router::OutgoingRouterActor, tcp::TcpFlavor, udp::UdpFlavor},
    util::{ClientId, TaskGuard},
};

/// How often the root actor logs the live actor tree snapshot.
const STATUS_REPORT_PERIOD: Duration = Duration::from_secs(60);

pub enum AgentMsg {
    ClientConnected(TcpStream, SocketAddr),
    AcceptFailed(io::Error),
    /// Fires once, `communication_timeout` after startup.
    FirstClientTimeout,
    ReportStatus,
}

pub struct AgentArgs {
    pub listener: TcpListener,
    pub first_client_timeout: Duration,
}

pub struct AgentState {
    next_client_id: ClientId,
    served_any_client: bool,
    /// Live client sessions, for logging on exit events.
    clients: HashMap<ActorId, ClientId>,
    _accept_task: TaskGuard,
}

pub struct AgentActor;

async fn accept_task(listener: TcpListener, agent: ActorRef<AgentMsg>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                if agent.cast(AgentMsg::ClientConnected(stream, peer)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = agent.cast(AgentMsg::AcceptFailed(error));
                break;
            }
        }
    }
}

impl Actor for AgentActor {
    type Msg = AgentMsg;
    type State = AgentState;
    type Arguments = AgentArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let accept = tokio::spawn(accept_task(args.listener, myself.clone()));
        myself.send_after(args.first_client_timeout, || AgentMsg::FirstClientTimeout);
        myself.send_interval(STATUS_REPORT_PERIOD, || AgentMsg::ReportStatus);

        Ok(AgentState {
            next_client_id: 0,
            served_any_client: false,
            clients: HashMap::new(),
            _accept_task: TaskGuard::new(accept),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            AgentMsg::ClientConnected(stream, peer) => {
                let id = state.next_client_id;
                state.next_client_id += 1;
                state.served_any_client = true;
                tracing::info!(client_id = id, %peer, "Accepted a client connection");

                // mirrord protocol messages are small and latency sensitive,
                // buffering them with Nagle's algorithm only slows the session down.
                if let Err(error) = stream.set_nodelay(true) {
                    tracing::warn!(client_id = id, %error, "Failed to set TCP_NODELAY on a client connection");
                }

                let (client_ref, _) = Actor::spawn_linked(
                    Some(format!("client-{id}")),
                    ClientActor,
                    ClientArgs { id, stream },
                    myself.get_cell(),
                )
                .await?;
                state.clients.insert(client_ref.get_id(), id);
            }

            AgentMsg::AcceptFailed(error) => {
                return Err(format!("client listener failed: {error}").into());
            }

            AgentMsg::FirstClientTimeout => {
                if !state.served_any_client {
                    tracing::error!("No client connected in time, exiting");
                    myself.stop(Some("first client timeout".to_owned()));
                }
            }

            AgentMsg::ReportStatus => {
                tracing::info!(
                    clients = ractor::pg::get_local_members(&CLIENTS_GROUP.to_owned()).len(),
                    tcp_outgoing_connections = ractor::pg::get_local_members(
                        &OutgoingRouterActor::<TcpFlavor>::conns_group()
                    )
                    .len(),
                    udp_outgoing_connections = ractor::pg::get_local_members(
                        &OutgoingRouterActor::<UdpFlavor>::conns_group()
                    )
                    .len(),
                    "Agent status",
                );
            }
        }

        Ok(())
    }

    /// Client sessions come and go; the agent lives on. This intentionally
    /// diverges from the original agent's exit-when-idle behavior to keep
    /// benchmark deployments simple.
    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match event {
            SupervisionEvent::ActorTerminated(cell, _, reason) => {
                if let Some(id) = state.clients.remove(&cell.get_id()) {
                    tracing::info!(client_id = id, ?reason, "Client session ended");
                }
            }

            SupervisionEvent::ActorFailed(cell, error) => {
                if let Some(id) = state.clients.remove(&cell.get_id()) {
                    tracing::error!(client_id = id, %error, "Client session failed");
                }
            }

            _ => {}
        }

        Ok(())
    }
}
