//! One DNS actor per client.
//!
//! Lookups run concurrently in spawned tasks (each builds a fresh
//! [`Resolver`] from `/etc/resolv.conf` + `/etc/hosts`, because that
//! configuration may change under the agent's feet), but responses are released
//! to the client strictly in request order - the mirrord protocol has no request
//! IDs for DNS, so the client matches responses to requests by order alone.
//!
//! Ordering is kept with a simple sequence-numbered queue: results parked out of
//! order wait until everything before them has resolved.

use std::{collections::VecDeque, io, path::PathBuf, sync::Arc, time::Duration};

use hickory_resolver::{
    Hosts, Resolver,
    config::{LookupIpStrategy, ServerOrderingStrategy},
    lookup_ip::LookupIp,
    net::{DnsError, NetError, runtime::TokioRuntimeProvider},
    proto::rr::{IntoName, Name},
    system_conf::parse_resolv_conf,
};
use mirrord_protocol::{
    DaemonMessage, DnsLookupError, ResolveErrorKindInternal, ResponseError,
    dns::{AddressFamily, DnsLookup, GetAddrInfoRequestV2, GetAddrInfoResponse, LookupRecord},
};
use ractor::{Actor, ActorProcessingErr, ActorRef};
use tokio::{fs, task::JoinSet, time::timeout};

use crate::client::ClientMsg;

/// Overall timeout for one lookup task, so a parked response cannot block the
/// ordered queue (and with it every later DNS answer) forever.
const LOOKUP_TASK_TIMEOUT: Duration = Duration::from_secs(15);

pub enum DnsMsg {
    Lookup(GetAddrInfoRequestV2),
    Resolved {
        seq: u64,
        result: Result<DnsLookup, ResolveErrorKindInternal>,
    },
}

pub struct DnsArgs {
    pub client: ActorRef<ClientMsg>,
}

pub struct DnsState {
    client: ActorRef<ClientMsg>,
    next_seq: u64,
    /// Lookups that were requested but not yet answered to the client,
    /// in request order. The front is the next response owed to the client.
    pending: VecDeque<(u64, Option<Result<DnsLookup, ResolveErrorKindInternal>>)>,
    /// Keeps lookup tasks abortable as a group when the actor dies. Reaped
    /// opportunistically, results are delivered through the mailbox instead.
    tasks: JoinSet<()>,
}

pub struct DnsActor;

impl Actor for DnsActor {
    type Msg = DnsMsg;
    type State = DnsState;
    type Arguments = DnsArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        Ok(DnsState {
            client: args.client,
            next_seq: 0,
            pending: VecDeque::new(),
            tasks: JoinSet::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            DnsMsg::Lookup(request) => {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.pending.push_back((seq, None));
                tracing::debug!(seq, node = %request.node, "Starting a DNS lookup");

                let actor = myself.clone();
                state.tasks.spawn(async move {
                    let result = timeout(LOOKUP_TASK_TIMEOUT, do_lookup(request))
                        .await
                        .unwrap_or(Err(ResolveErrorKindInternal::Timeout));
                    let _ = actor.cast(DnsMsg::Resolved { seq, result });
                });
            }

            DnsMsg::Resolved { seq, result } => {
                let slot = state
                    .pending
                    .iter_mut()
                    .find(|(pending_seq, ..)| *pending_seq == seq);
                match slot {
                    Some((_, response)) => *response = Some(result),
                    None => tracing::error!(seq, "Got a DNS result with no pending request"),
                }

                while state
                    .pending
                    .front()
                    .is_some_and(|(_, result)| result.is_some())
                {
                    let (seq, result) = state.pending.pop_front().expect("front was checked");
                    let result = result.expect("front was checked");
                    tracing::debug!(seq, ok = result.is_ok(), "Answering a DNS lookup");
                    let response = GetAddrInfoResponse(
                        result.map_err(|kind| ResponseError::DnsLookup(DnsLookupError { kind })),
                    );
                    state.client.cast(ClientMsg::Send {
                        message: DaemonMessage::GetAddrInfoResponse(response),
                        budget: None,
                    })?;
                }

                while state.tasks.try_join_next().is_some() {}
            }
        }

        Ok(())
    }
}

/// Reads `/etc/resolv.conf` and `/etc/hosts`, then resolves `request.node`.
///
/// Adapted from mirrord-agent's `DnsWorker::do_lookup`, minus target-container
/// path resolution (this agent is targetless, `/etc` is its own).
async fn do_lookup(request: GetAddrInfoRequestV2) -> Result<DnsLookup, ResolveErrorKindInternal> {
    let resolver = build_resolver(request.family)
        .await
        .inspect_err(|error| tracing::error!(%error, "Failed to build a DNS resolver"))
        .map_err(|error| lookup_error_to_protocol(&error))?;

    let result = if request.node.to_ip().is_some() {
        // `IntoName::to_ip` of `Name` always returns `None`, so IP addresses must
        // not be converted to `Name` first.
        resolver.lookup_ip(request.node).await
    } else {
        // Relaxed parsing, because hickory is too eager when validating hostnames
        // (it rejects underscores, for example).
        match Name::from_str_relaxed(&request.node) {
            Ok(name) => resolver.lookup_ip(name).await,
            Err(error) => Err(NetError::Msg(format!(
                "node name rejected by hickory: {error:?}"
            ))),
        }
    };

    result
        .inspect(|lookup| tracing::trace!(?lookup, "DNS lookup finished"))
        .inspect_err(|error| tracing::debug!(%error, "DNS lookup failed"))
        .map(convert_lookup)
        .map_err(|error| net_error_to_protocol(&error))
}

async fn build_resolver(
    family: AddressFamily,
) -> Result<Resolver<TokioRuntimeProvider>, LookupError> {
    let etc_path = PathBuf::from("/etc");
    let resolv_conf = fs::read(etc_path.join("resolv.conf")).await?;
    let hosts_conf = fs::read(etc_path.join("hosts")).await?;

    let (config, mut options) = parse_resolv_conf(resolv_conf)?;
    options.server_ordering_strategy = ServerOrderingStrategy::UserProvidedOrder;
    options.ip_strategy = family_to_strategy(family);

    let mut resolver = Resolver::builder_with_config(config, TokioRuntimeProvider::default())
        .with_options(options)
        .build()?;

    let mut hosts = Hosts::default();
    hosts.read_hosts_conf(hosts_conf.as_slice())?;
    resolver.set_hosts(Arc::new(hosts));

    Ok(resolver)
}

#[derive(thiserror::Error, Debug)]
enum LookupError {
    #[error("failed to read configuration from /etc: {0}")]
    ReadConfiguration(#[from] io::Error),
    #[error("resolve error: {0}")]
    Resolve(#[from] NetError),
}

fn lookup_error_to_protocol(error: &LookupError) -> ResolveErrorKindInternal {
    match error {
        LookupError::ReadConfiguration(error) => io_error_to_protocol(error.kind()),
        LookupError::Resolve(error) => net_error_to_protocol(error),
    }
}

fn io_error_to_protocol(kind: io::ErrorKind) -> ResolveErrorKindInternal {
    match kind {
        io::ErrorKind::TimedOut => ResolveErrorKindInternal::Timeout,
        io::ErrorKind::NotFound => ResolveErrorKindInternal::NotFound,
        io::ErrorKind::PermissionDenied => ResolveErrorKindInternal::PermissionDenied,
        other => ResolveErrorKindInternal::Message(format!("io error: {other}")),
    }
}

fn net_error_to_protocol(error: &NetError) -> ResolveErrorKindInternal {
    match error {
        NetError::Message(message) => ResolveErrorKindInternal::Message((*message).to_owned()),
        NetError::Msg(message) => ResolveErrorKindInternal::Message(message.clone()),
        NetError::NoConnections => ResolveErrorKindInternal::NoConnections,
        NetError::Dns(DnsError::NoRecordsFound(no_records)) => {
            ResolveErrorKindInternal::NoRecordsFound(no_records.response_code.into())
        }
        NetError::Dns(DnsError::ResponseCode(code)) => {
            ResolveErrorKindInternal::Message(format!("DNS server error response: {code}"))
        }
        NetError::Proto(proto_error) => {
            ResolveErrorKindInternal::Message(format!("proto error: {proto_error}"))
        }
        NetError::Timeout => ResolveErrorKindInternal::Timeout,
        NetError::Io(error) => io_error_to_protocol(error.kind()),
        other => {
            tracing::warn!(error = ?other, "Detected an unhandled NetError variant, this is a bug");
            ResolveErrorKindInternal::Unknown
        }
    }
}

fn convert_lookup(lookup: LookupIp) -> DnsLookup {
    let records = lookup
        .as_lookup()
        .answers()
        .iter()
        .filter_map(|record| {
            let ip = record.data.ip_addr()?;
            Some(LookupRecord {
                name: record.name.to_string(),
                ip,
            })
        })
        .collect();
    DnsLookup(records)
}

fn family_to_strategy(family: AddressFamily) -> LookupIpStrategy {
    match family {
        AddressFamily::Ipv4Only => LookupIpStrategy::Ipv4Only,
        AddressFamily::Ipv6Only => LookupIpStrategy::Ipv6Only,
        AddressFamily::Both => LookupIpStrategy::Ipv4AndIpv6,
        AddressFamily::Any => LookupIpStrategy::Ipv4thenIpv6,
        AddressFamily::UnknownAddressFamilyFromNewerClient => {
            tracing::error!("Unknown address family in addrinfo request, using IPv4 and IPv6");
            LookupIpStrategy::Ipv4AndIpv6
        }
    }
}
