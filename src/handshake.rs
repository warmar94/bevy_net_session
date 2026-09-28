//! The join handshake: the two wire events, the game's validator hook, the host's answer and the
//! client's reaction.
//!
//! 1. A client whose transport is connected sends [`SessionJoinRequest`] (protocol version + the
//!    game's opaque payload) every [`JOIN_RESEND_SECS`] until it is answered.
//! 2. The host (only while hosting) checks the version FIRST, then asks the game's
//!    [`JoinValidator`], and answers [`SessionJoinReply`]. A refused client is disconnected after
//!    this frame's sends ([`DisconnectRequest`]), so the reason reaches it.
//! 3. The client turns the answer into [`JoinAccepted`] or [`JoinFailed`].
//!
//! Under replicon's `AuthMethod::ProtocolCheck` (its default) the host answers only a client that
//! replicon has authorized (its protocol hash matched); an earlier request is ignored and the
//! client's resend comes back once it is. A mismatched hash never gets that far: replicon answers
//! it with `ProtocolMismatch`, which the client reports as [`JoinFailReason::VersionMismatch`].

use bevy_ecs::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon::shared::backend::connected_client::NetworkId;
use bevy_replicon_renet::RenetClient;
use bevy_state::state::State;
use bevy_time::{Real, Time};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::messages::{JoinAccepted, JoinFailReason, PeerConnected, PeerDisconnected, SessionEndReason};
use crate::session::{NetSession, SessionPeer, SessionRole, SessionState};
use crate::transport::{end_session, fail_join, Close};
use crate::NetSessionConfig;

/// How often a joining client repeats its join request until the host answers (seconds).
pub(crate) const JOIN_RESEND_SECS: f32 = 0.5;

/// Client -> host: "let me in". Resent until answered.
#[derive(Event, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionJoinRequest {
    pub(crate) version: u64,
    pub(crate) payload: Vec<u8>,
}

/// Host -> one client: the answer. Independent (sent at once, not held until the client is
/// replication-authorized): it carries no entity.
#[derive(Event, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionJoinReply {
    Accepted,
    VersionMismatch { host: u64 },
    Rejected { reason: String },
}

// ---------------------------------------------------------------------------------------------
// The validator hook
// ---------------------------------------------------------------------------------------------

/// What the host's [`JoinValidator`] is asked about.
#[derive(Clone, Copy, Debug)]
pub struct JoinRequestInfo<'a> {
    /// The client's entity (replicon's `ConnectedClient`).
    pub client: Entity,
    /// The client's id (renet's client id; its SteamID64 over Steam).
    pub id: u64,
    /// The client's protocol version (already checked equal to the host's).
    pub version: u64,
    /// The client's [`JoinSession::payload`](crate::JoinSession::payload).
    pub payload: &'a [u8],
}

/// The game's say on who may join. Runs on the host, after the protocol version matched.
/// `Err(reason)` refuses the client; the reason reaches it as
/// [`JoinFailReason::Rejected`](crate::JoinFailReason::Rejected). Keep reasons short and ASCII.
///
/// Implemented for every `Fn(&JoinRequestInfo) -> Result<(), String> + Send + Sync + 'static`.
pub trait JoinValidator: Send + Sync + 'static {
    /// Accept (`Ok`) or refuse (`Err(reason)`) one join request.
    fn validate(&self, request: &JoinRequestInfo<'_>) -> Result<(), String>;
}

impl<F> JoinValidator for F
where
    F: Fn(&JoinRequestInfo<'_>) -> Result<(), String> + Send + Sync + 'static,
{
    fn validate(&self, request: &JoinRequestInfo<'_>) -> Result<(), String> {
        self(request)
    }
}

/// The installed [`JoinValidator`]. Without this resource every client with the right protocol
/// version is accepted. Insert, replace or remove it at any time.
#[derive(Resource)]
pub struct JoinValidatorRes(pub Box<dyn JoinValidator>);

impl JoinValidatorRes {
    /// Wrap a validator (a closure or any [`JoinValidator`]).
    pub fn new(validator: impl JoinValidator) -> Self {
        Self(Box::new(validator))
    }
}

// ---------------------------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------------------------

/// A client's join in progress. Local, reset with every new join.
#[derive(Resource, Default, Debug)]
pub(crate) struct ClientJoin {
    /// Wall-clock seconds since the join started.
    pub(crate) waited: f32,
    /// The transport has been connected at least once.
    pub(crate) was_connected: bool,
    /// Seconds until the next request is sent.
    pub(crate) resend_in: f32,
    /// The payload to present.
    pub(crate) payload: Vec<u8>,
}

/// `Update`: while joining with a connected transport, send the join request (and repeat it).
pub(crate) fn send_join_request(
    session: Res<NetSession>,
    config: Res<NetSessionConfig>,
    client_state: Option<Res<State<ClientState>>>,
    time: Res<Time<Real>>,
    mut join: ResMut<ClientJoin>,
    mut commands: Commands,
) {
    if session.state != SessionState::Connecting {
        return;
    }
    // Only on a live connection: replicon delivers a client event LOCALLY while disconnected.
    if client_state.as_deref().map(State::get) != Some(&ClientState::Connected) {
        return;
    }
    join.resend_in -= time.delta_secs();
    if join.resend_in > 0.0 {
        return;
    }
    join.resend_in = JOIN_RESEND_SECS;
    commands.client_trigger(SessionJoinRequest { version: config.protocol_version, payload: join.payload.clone() });
}

/// `Update`: give up on a join going nowhere, and notice a joined client losing its host.
///
/// Joining: a transport that reports disconnected ends the join at once
/// ([`JoinFailReason::LostConnection`] if it had connected, [`JoinFailReason::CouldNotReach`] if
/// it never did), and so does the join timeout ([`JoinFailReason::Timeout`]). Joined: a
/// disconnected transport ends the session ([`SessionEndReason::LostConnection`]).
/// Wall-clock time, so a long frame cannot stretch the timeout.
pub(crate) fn join_watchdog(
    session: Res<NetSession>,
    config: Res<NetSessionConfig>,
    time: Res<Time<Real>>,
    client: Option<Res<RenetClient>>,
    mut join: ResMut<ClientJoin>,
    mut commands: Commands,
) {
    if session.role != SessionRole::Client {
        return;
    }
    let dropped = client.as_deref().is_none_or(|c| c.is_disconnected());
    match session.state {
        SessionState::Connecting => {
            join.waited += time.delta_secs();
            join.was_connected |= client.as_deref().is_some_and(|c| c.is_connected());
            let timeout = config.join_timeout.as_secs_f32();
            let failure = if dropped && join.was_connected {
                Some((JoinFailReason::LostConnection, "lost the connection to the host before it answered".to_string()))
            } else if dropped {
                Some((JoinFailReason::CouldNotReach, "could not reach the host".to_string()))
            } else if join.waited >= timeout {
                Some((JoinFailReason::Timeout, format!("the host did not answer within {timeout:.1} s")))
            } else {
                None
            };
            if let Some((reason, message)) = failure {
                commands.queue(move |world: &mut World| fail_join(world, reason, message));
            }
        }
        SessionState::Joined if dropped => {
            commands.queue(|world: &mut World| end_session(world, Close::Now, SessionEndReason::LostConnection));
        }
        _ => {}
    }
}

/// The host answered.
pub(crate) fn on_join_reply(
    reply: On<SessionJoinReply>,
    mut session: ResMut<NetSession>,
    mut accepted: MessageWriter<JoinAccepted>,
    config: Res<NetSessionConfig>,
    mut commands: Commands,
) {
    // Only a joining client acts on it (never a host, never a client already in).
    if session.role != SessionRole::Client || session.state != SessionState::Connecting {
        return;
    }
    match reply.event().clone() {
        SessionJoinReply::Accepted => {
            session.set(SessionRole::Client, SessionState::Joined);
            let transport = session.transport.unwrap_or(crate::TransportKind::Ip);
            let local_id = session.local_id.unwrap_or(0);
            info!("net session: joined (id {local_id})");
            accepted.write(JoinAccepted { transport, local_id });
        }
        SessionJoinReply::VersionMismatch { host } => {
            let mine = config.protocol_version;
            let message = format!("version mismatch: the host runs protocol {host}, this build {mine}");
            commands.queue(move |world: &mut World| fail_join(world, JoinFailReason::VersionMismatch, message));
        }
        SessionJoinReply::Rejected { reason } => {
            let message = format!("the host refused the join: {reason}");
            commands.queue(move |world: &mut World| fail_join(world, JoinFailReason::Rejected(reason), message));
        }
    }
}

/// Replicon's protocol check refused this client: another build (a different protocol version,
/// which is part of the hash, or different replicated registrations). The event is unreliable and
/// races the disconnect, so a lost one is reported as the disconnect instead.
pub(crate) fn on_protocol_mismatch(_mismatch: On<ProtocolMismatch>, session: Res<NetSession>, mut commands: Commands) {
    if session.role != SessionRole::Client || session.state != SessionState::Connecting {
        return;
    }
    commands.queue(|world: &mut World| {
        fail_join(
            world,
            JoinFailReason::VersionMismatch,
            "version mismatch: the host runs a different protocol (version or replicated registrations)".to_string(),
        )
    });
}

// ---------------------------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------------------------

/// The host's view of a joining client.
type JoinerQuery<'w, 's> = Query<'w, 's, (Option<&'static NetworkId>, Has<AuthorizedClient>, Has<SessionPeer>)>;

/// A client asked to join: version first, then the game's validator, then accept.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_join_request(
    request: On<FromClient<SessionJoinRequest>>,
    session: Res<NetSession>,
    config: Res<NetSessionConfig>,
    validator: Option<Res<JoinValidatorRes>>,
    auth: Option<Res<AuthMethod>>,
    clients: JoinerQuery,
    mut disconnects: MessageWriter<DisconnectRequest>,
    mut connected: MessageWriter<PeerConnected>,
    mut commands: Commands,
) {
    // Only a host answers. A request delivered locally (no transport) is never an invitation.
    if session.role != SessionRole::Host {
        return;
    }
    let client_id = request.client_id;
    let Some(entity) = client_id.entity() else {
        return;
    };
    let Ok((network_id, authorized, already)) = clients.get(entity) else {
        return;
    };
    if already {
        // A resend that crossed the answer.
        return;
    }
    if auth.as_deref() == Some(&AuthMethod::ProtocolCheck) && !authorized {
        debug!("net session: join request from {entity} before replicon authorized it; waiting for the resend");
        return;
    }
    let id = network_id.map_or_else(|| entity.to_bits(), NetworkId::get);
    let message = &request.message;

    let refuse = |reply: SessionJoinReply, commands: &mut Commands, disconnects: &mut MessageWriter<DisconnectRequest>| {
        commands.server_trigger(ToClients { targets: SendTargets::Single(client_id), message: reply });
        // After this frame's sends, so the answer reaches the client first.
        disconnects.write(DisconnectRequest { client: entity });
    };

    if message.version != config.protocol_version {
        info!("net session: refused client {id}: protocol version {} (host {})", message.version, config.protocol_version);
        refuse(SessionJoinReply::VersionMismatch { host: config.protocol_version }, &mut commands, &mut disconnects);
        return;
    }
    if let Some(validator) = validator.as_deref() {
        let info = JoinRequestInfo { client: entity, id, version: message.version, payload: &message.payload };
        if let Err(reason) = validator.0.validate(&info) {
            info!("net session: refused client {id}: {reason}");
            refuse(SessionJoinReply::Rejected { reason }, &mut commands, &mut disconnects);
            return;
        }
    }
    commands.entity(entity).insert(SessionPeer { id });
    commands.server_trigger(ToClients { targets: SendTargets::Single(client_id), message: SessionJoinReply::Accepted });
    info!("net session: client {id} joined");
    connected.write(PeerConnected { entity, id });
}

/// A client that had joined is gone (its entity despawned, or the host closed).
pub(crate) fn on_peer_removed(removed: On<Remove, SessionPeer>, peers: Query<&SessionPeer>, mut disconnected: MessageWriter<PeerDisconnected>) {
    let entity = removed.entity;
    if let Ok(peer) = peers.get(entity) {
        info!("net session: client {} left", peer.id);
        disconnected.write(PeerDisconnected { entity, id: peer.id });
    }
}
