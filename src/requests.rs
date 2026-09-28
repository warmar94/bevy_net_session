//! `Update`: turn the request messages into sessions.
//!
//! Requests are read once per frame in this order: every [`LeaveSession`], then every
//! [`HostSession`], then every [`JoinSession`]; the LAST one read is the one that stands (each
//! replaces the one before it).
//!
//! A request that starts a session while another one is open closes the old one at once and
//! starts the new one on the NEXT frame: replicon's client/server state follows the renet
//! resources in `PreUpdate`, and removing and re-adding them within one frame would leave it in
//! the old session's state.

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::{RenetClient, RenetServer};
use tracing::{debug, info, warn};

use crate::handshake::ClientJoin;
use crate::messages::*;
use crate::session::{NetSession, SessionRole, SessionState};
use crate::transport::{end_session, finish_closing, Close, PendingClose};
use crate::NetSessionConfig;

/// A start request waiting one frame for the old session to be gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartRequest {
    Host(HostSession),
    Join(JoinSession),
}

/// See [`StartRequest`].
#[derive(Resource, Default, Debug)]
pub(crate) struct DeferredStart(pub(crate) Option<StartRequest>);

/// The request messages.
#[derive(SystemParam)]
pub(crate) struct Requests<'w, 's> {
    leaves: MessageReader<'w, 's, LeaveSession>,
    hosts: MessageReader<'w, 's, HostSession>,
    joins: MessageReader<'w, 's, JoinSession>,
}

/// What the start of a session reports.
#[derive(SystemParam)]
pub(crate) struct StartFacts<'w> {
    started: MessageWriter<'w, SessionStarted>,
    host_failed: MessageWriter<'w, HostFailed>,
    join_failed: MessageWriter<'w, JoinFailed>,
}

/// Everything a start needs to open a transport.
#[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(dead_code))]
#[derive(SystemParam)]
pub(crate) struct Opener<'w> {
    #[cfg_attr(not(feature = "ip"), allow(dead_code))]
    config: Res<'w, NetSessionConfig>,
    channels: Res<'w, RepliconChannels>,
    #[cfg(feature = "steam")]
    steam: Option<Res<'w, crate::SteamNetClient>>,
}

/// The last request of the frame.
enum Incoming {
    Leave,
    Start(StartRequest),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_requests(
    mut requests: Requests,
    mut session: ResMut<NetSession>,
    mut deferred: ResMut<DeferredStart>,
    mut pending: ResMut<PendingClose>,
    mut join: ResMut<ClientJoin>,
    opener: Opener,
    mut facts: StartFacts,
    server: Option<Res<RenetServer>>,
    client: Option<Res<RenetClient>>,
    mut commands: Commands,
) {
    let mut last = None;
    let mut count = 0usize;
    for _ in requests.leaves.read() {
        last = Some(Incoming::Leave);
        count += 1;
    }
    for host in requests.hosts.read() {
        last = Some(Incoming::Start(StartRequest::Host(host.clone())));
        count += 1;
    }
    for join in requests.joins.read() {
        last = Some(Incoming::Start(StartRequest::Join(join.clone())));
        count += 1;
    }
    if count > 1 {
        debug!("net session: {count} requests in one frame; the last one stands");
    }

    let busy = session.is_active() || server.is_some() || client.is_some();
    let start = match last {
        Some(Incoming::Leave) => {
            deferred.0 = None;
            if busy {
                // Closed in PostUpdate, after this frame's packets went out.
                pending.0 = Some(SessionEndReason::Left);
            } else {
                debug!("net session: leave requested with no session");
            }
            return;
        }
        Some(Incoming::Start(request)) => {
            pending.0 = None;
            if busy {
                commands.queue(|world: &mut World| end_session(world, Close::Now, SessionEndReason::Replaced));
                deferred.0 = Some(request);
                return;
            }
            request
        }
        None => match deferred.0.take() {
            Some(request) if busy => {
                // Still closing (should not happen: the close ran last frame). Try again next frame.
                deferred.0 = Some(request);
                return;
            }
            Some(request) => request,
            None => return,
        },
    };

    // Never share the process with a link still in its close grace.
    commands.queue(|world: &mut World| {
        finish_closing(world);
    });
    match start {
        StartRequest::Host(request) => start_host(&request, &mut session, &opener, &mut facts, &mut commands),
        StartRequest::Join(request) => start_join(request, &mut session, &mut join, &opener, &mut facts, &mut commands),
    }
}

fn start_host(request: &HostSession, session: &mut NetSession, opener: &Opener, facts: &mut StartFacts, commands: &mut Commands) {
    let result = match &request.transport {
        HostTransport::Ip { port } => open_ip_host(commands, opener, *port, request.max_clients),
        HostTransport::Steam { access } => open_steam_host(commands, opener, access, request.max_clients),
    };
    match result {
        Ok(started) => {
            session.set(SessionRole::Host, SessionState::Listening);
            session.transport = Some(started.transport);
            session.local_id = started.steam_id;
            session.local_addr = started.local_addr;
            match (started.local_addr, started.steam_id) {
                (Some(addr), _) => info!("net session: hosting on UDP {addr} (max {} clients)", request.max_clients),
                (_, Some(id)) => info!("net session: hosting on Steam as {id} (max {} clients)", request.max_clients),
                _ => info!("net session: hosting"),
            }
            facts.started.write(SessionStarted { transport: started.transport, local_addr: started.local_addr, steam_id: started.steam_id });
        }
        Err((reason, message)) => {
            warn!("net session: could not host ({reason:?}): {message}");
            facts.host_failed.write(HostFailed { reason, message });
        }
    }
}

fn start_join(request: JoinSession, session: &mut NetSession, join: &mut ClientJoin, opener: &Opener, facts: &mut StartFacts, commands: &mut Commands) {
    let result = match request.target {
        JoinTarget::Ip(addr) => open_ip_client(commands, opener, addr).map(|id| (TransportKind::Ip, id)),
        JoinTarget::Steam(host) => open_steam_client(commands, opener, host).map(|id| (TransportKind::Steam, id)),
    };
    match result {
        Ok((transport, local_id)) => {
            session.set(SessionRole::Client, SessionState::Connecting);
            session.transport = Some(transport);
            session.local_id = Some(local_id);
            session.local_addr = None;
            *join = ClientJoin { payload: request.payload, ..Default::default() };
            match request.target {
                JoinTarget::Ip(addr) => info!("net session: joining {addr} as {local_id}"),
                JoinTarget::Steam(host) => info!("net session: joining Steam host {host} as {local_id}"),
            }
        }
        Err((reason, message)) => {
            warn!("net session: could not join ({reason:?}): {message}");
            facts.join_failed.write(JoinFailed { reason, message });
        }
    }
}

type HostResult = Result<crate::transport::HostStarted, (HostFailReason, String)>;

#[cfg(feature = "ip")]
fn open_ip_host(commands: &mut Commands, opener: &Opener, port: u16, max_clients: usize) -> HostResult {
    crate::transport::start_ip_server(commands, &opener.channels, &opener.config, port, max_clients)
        .map(|addr| crate::transport::HostStarted { transport: TransportKind::Ip, local_addr: Some(addr), steam_id: None })
        .map_err(|e| (HostFailReason::CouldNotStart, e))
}

#[cfg(not(feature = "ip"))]
fn open_ip_host(_: &mut Commands, _: &Opener, _: u16, _: usize) -> HostResult {
    Err((HostFailReason::TransportUnavailable, "UDP hosting needs the `ip` feature".to_string()))
}

#[cfg(feature = "steam")]
fn open_steam_host(commands: &mut Commands, opener: &Opener, access: &SteamAccess, max_clients: usize) -> HostResult {
    let Some(steam) = opener.steam.as_deref() else {
        return Err((HostFailReason::TransportUnavailable, "Steam is not available (no SteamNetClient)".to_string()));
    };
    crate::transport::start_steam_server(commands, &opener.channels, &steam.0, access, max_clients)
        .map(|id| crate::transport::HostStarted { transport: TransportKind::Steam, local_addr: None, steam_id: Some(id) })
        .map_err(|e| (HostFailReason::CouldNotStart, e))
}

#[cfg(not(feature = "steam"))]
fn open_steam_host(_: &mut Commands, _: &Opener, _: &SteamAccess, _: usize) -> HostResult {
    Err((HostFailReason::TransportUnavailable, "Steam hosting needs the `steam` feature".to_string()))
}

type JoinResult = Result<u64, (JoinFailReason, String)>;

#[cfg(feature = "ip")]
fn open_ip_client(commands: &mut Commands, opener: &Opener, addr: std::net::SocketAddr) -> JoinResult {
    crate::transport::start_ip_client(commands, &opener.channels, &opener.config, addr).map_err(|e| (JoinFailReason::CouldNotStart, e))
}

#[cfg(not(feature = "ip"))]
fn open_ip_client(_: &mut Commands, _: &Opener, _: std::net::SocketAddr) -> JoinResult {
    Err((JoinFailReason::TransportUnavailable, "UDP joining needs the `ip` feature".to_string()))
}

#[cfg(feature = "steam")]
fn open_steam_client(commands: &mut Commands, opener: &Opener, host: u64) -> JoinResult {
    let Some(steam) = opener.steam.as_deref() else {
        return Err((JoinFailReason::TransportUnavailable, "Steam is not available (no SteamNetClient)".to_string()));
    };
    crate::transport::start_steam_client(commands, &opener.channels, &steam.0, host).map_err(|e| (JoinFailReason::CouldNotStart, e))
}

#[cfg(not(feature = "steam"))]
fn open_steam_client(_: &mut Commands, _: &Opener, _: u64) -> JoinResult {
    Err((JoinFailReason::TransportUnavailable, "Steam joining needs the `steam` feature".to_string()))
}
