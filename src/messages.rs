//! The public messages: requests the game writes, facts the plugin writes, and their enums.

use std::net::SocketAddr;

use bevy_ecs::prelude::*;

// ---------------------------------------------------------------------------------------------
// Requests (the game writes these)
// ---------------------------------------------------------------------------------------------

/// Request: become the host. Opens a listen transport and accepts joiners through the handshake.
///
/// Closes whatever session this process had first. Answered with [`SessionStarted`] or
/// [`HostFailed`].
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct HostSession {
    /// Where to listen.
    pub transport: HostTransport,
    /// How many clients may be connected at once (the host itself not counted). Clamped to at
    /// least 1; the UDP transport also caps it at [`MAX_IP_CLIENTS`](crate::MAX_IP_CLIENTS).
    pub max_clients: usize,
}

impl HostSession {
    /// Host over UDP on `port` (`0` = let the OS pick one; read it back from
    /// [`SessionStarted::local_addr`]).
    pub fn ip(port: u16, max_clients: usize) -> Self {
        Self { transport: HostTransport::Ip { port }, max_clients }
    }

    /// Host over Steam P2P, reachable by the host's Steam friends ([`SteamAccess::FriendsOnly`]).
    pub fn steam(max_clients: usize) -> Self {
        Self { transport: HostTransport::Steam { access: SteamAccess::FriendsOnly }, max_clients }
    }
}

/// The transport a host listens on. One per host: a host is reachable over UDP **or** Steam,
/// never both at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostTransport {
    /// UDP (renet's netcode transport, feature `ip`), bound to `0.0.0.0:port`.
    Ip {
        /// The UDP port. `0` lets the operating system choose a free one.
        port: u16,
    },
    /// Steam P2P (renet_steam, feature `steam`). Needs a [`SteamNetClient`](crate::SteamNetClient)
    /// resource.
    Steam {
        /// Who may connect.
        access: SteamAccess,
    },
}

/// Who may connect to a Steam host (maps to renet_steam's `AccessPermission`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SteamAccess {
    /// The host's direct Steam friends. The default.
    #[default]
    FriendsOnly,
    /// Anyone who knows the host's SteamID64.
    Public,
    /// Nobody (useful to stop accepting new connections).
    Private,
    /// Members of this Steam lobby (a lobby id, e.g. from a lobby crate) only.
    InLobby(u64),
    /// Exactly these SteamID64s.
    InList(Vec<u64>),
}

/// Request: join a host. Closes whatever session this process had first.
///
/// Answered with exactly one [`JoinAccepted`] or [`JoinFailed`].
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct JoinSession {
    /// Where the host is.
    pub target: JoinTarget,
    /// Opaque bytes handed to the host's [`JoinValidator`](crate::JoinValidator) (a password, a
    /// character, a token; the game decides). Empty by default.
    pub payload: Vec<u8>,
}

impl JoinSession {
    /// Join a UDP host at `addr`.
    pub fn ip(addr: SocketAddr) -> Self {
        Self { target: JoinTarget::Ip(addr), payload: Vec::new() }
    }

    /// Join a Steam host by its SteamID64.
    pub fn steam(host: u64) -> Self {
        Self { target: JoinTarget::Steam(host), payload: Vec::new() }
    }

    /// The same request carrying `payload` for the host's validator.
    pub fn with_payload(mut self, payload: impl Into<Vec<u8>>) -> Self {
        self.payload = payload.into();
        self
    }
}

/// Where a [`JoinSession`] connects to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinTarget {
    /// A UDP host (feature `ip`).
    Ip(SocketAddr),
    /// A Steam host, by its SteamID64 (feature `steam`).
    Steam(u64),
}

/// Request: leave the current session (host or client). The close runs in `PostUpdate`, after
/// this frame's packets were sent, so whatever the game sent in the same frame still goes out.
/// Ends with [`SessionEnded`] (`Left`), or [`JoinFailed`] (`Cancelled`) for a join still pending.
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LeaveSession;

// ---------------------------------------------------------------------------------------------
// Facts (the plugin writes these)
// ---------------------------------------------------------------------------------------------

/// Which transport a session uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// UDP (renet netcode).
    Ip,
    /// Steam P2P (renet_steam).
    Steam,
}

/// Fact: this process is hosting and listening.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct SessionStarted {
    /// The transport it listens on.
    pub transport: TransportKind,
    /// UDP: the bound address (`0.0.0.0:<port>`; the real port when `0` was requested).
    pub local_addr: Option<SocketAddr>,
    /// Steam: the host's own SteamID64 (what joiners connect to).
    pub steam_id: Option<u64>,
}

/// Fact: a [`HostSession`] could not start. Nothing is listening.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct HostFailed {
    /// Why.
    pub reason: HostFailReason,
    /// A human-readable detail (ASCII, suitable for a log line or a toast).
    pub message: String,
}

/// Why a host did not start.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostFailReason {
    /// The transport is not compiled in (feature `ip` / `steam`), or Steam is not available
    /// (no [`SteamNetClient`](crate::SteamNetClient)).
    TransportUnavailable,
    /// The transport refused to start (port in use, Steam socket error, ...).
    CouldNotStart,
}

/// Fact: the host accepted this client. The session is live; game data (world info, ...) is the
/// game's own replicated message from here on.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct JoinAccepted {
    /// The transport it uses.
    pub transport: TransportKind,
    /// This client's id on the host (renet's client id; the SteamID64 over Steam).
    pub local_id: u64,
}

/// Fact: a [`JoinSession`] did not lead to a session. Everything it opened is closed again.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct JoinFailed {
    /// Why.
    pub reason: JoinFailReason,
    /// A human-readable detail (ASCII, suitable for a log line or a toast).
    pub message: String,
}

/// Why a join failed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum JoinFailReason {
    /// The transport dropped before it ever connected (nobody there, refused, unreachable).
    CouldNotReach,
    /// Nothing was decided within the join timeout.
    Timeout,
    /// The transport connected, then dropped before the host answered.
    LostConnection,
    /// The host runs another protocol version (or registers a different replicated protocol).
    VersionMismatch,
    /// The host's validator said no; the host's reason.
    Rejected(String),
    /// The transport is not compiled in, or Steam is not available.
    TransportUnavailable,
    /// The transport refused to start (socket error, bad address, ...).
    CouldNotStart,
    /// A [`LeaveSession`], a new request or the app exiting abandoned the join first.
    Cancelled,
}

/// Fact (host): a client passed the handshake and is now part of the session.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerConnected {
    /// The client's entity (replicon's `ConnectedClient`; carries [`SessionPeer`](crate::SessionPeer)).
    pub entity: Entity,
    /// The client's id (renet's client id; its SteamID64 over Steam).
    pub id: u64,
}

/// Fact (host): a client that had passed the handshake left or was disconnected.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerDisconnected {
    /// The client's entity (already despawned or being despawned).
    pub entity: Entity,
    /// The client's id.
    pub id: u64,
}

/// Fact: a session that had started ([`SessionStarted`] or [`JoinAccepted`]) is over.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct SessionEnded {
    /// Why.
    pub reason: SessionEndReason,
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionEndReason {
    /// A [`LeaveSession`].
    Left,
    /// A new [`HostSession`] / [`JoinSession`] replaced it.
    Replaced,
    /// Client: the connection to the host dropped (host gone, kicked, network).
    LostConnection,
    /// The app is exiting.
    AppExit,
}
