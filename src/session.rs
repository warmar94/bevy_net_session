//! The session state the plugin keeps: [`NetSession`] (read-only for the game) and the
//! [`SessionPeer`] marker on a host's accepted clients.

use std::net::SocketAddr;

use bevy_ecs::prelude::*;

use crate::messages::TransportKind;

/// This process's part in the network session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SessionRole {
    /// No session (single-player, or not connected yet).
    #[default]
    None,
    /// Hosting: a listen transport is open.
    Host,
    /// A client of a host (joining or joined).
    Client,
}

/// Where the session is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// No session.
    #[default]
    Idle,
    /// Host: listening for joiners.
    Listening,
    /// Client: the transport is up (or coming up) and the join handshake is in progress.
    Connecting,
    /// Client: the host accepted the join.
    Joined,
}

/// The current session. Written only by the plugin; read it, never write it.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct NetSession {
    pub(crate) role: SessionRole,
    pub(crate) state: SessionState,
    pub(crate) transport: Option<TransportKind>,
    pub(crate) local_id: Option<u64>,
    pub(crate) local_addr: Option<SocketAddr>,
    pub(crate) generation: u32,
}

impl NetSession {
    /// Host, client or none.
    pub fn role(&self) -> SessionRole {
        self.role
    }

    /// Idle, listening, connecting or joined.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// `true` while hosting.
    pub fn is_host(&self) -> bool {
        self.role == SessionRole::Host
    }

    /// `true` while a client (joining or joined).
    pub fn is_client(&self) -> bool {
        self.role == SessionRole::Client
    }

    /// `true` once a client's join was accepted.
    pub fn is_joined(&self) -> bool {
        self.state == SessionState::Joined
    }

    /// `true` when any session exists (hosting, joining or joined).
    pub fn is_active(&self) -> bool {
        self.role != SessionRole::None
    }

    /// The transport in use, if any.
    pub fn transport(&self) -> Option<TransportKind> {
        self.transport
    }

    /// This process's id in the session: a client's renet client id (its SteamID64 over Steam),
    /// a Steam host's SteamID64. `None` for a UDP host and when idle.
    pub fn local_id(&self) -> Option<u64> {
        self.local_id
    }

    /// A UDP host's bound address (the real port when `0` was requested).
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Bumped (wrapping) on every change of role or state. A cheap change check.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub(crate) fn set(&mut self, role: SessionRole, state: SessionState) {
        self.role = role;
        self.state = state;
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn reset(&mut self) {
        let generation = self.generation.wrapping_add(1);
        *self = Self { generation, ..Self::default() };
    }
}

/// Host side: marks a connected client (replicon's `ConnectedClient` entity) that passed the join
/// handshake. Inserted by the plugin together with [`PeerConnected`](crate::PeerConnected);
/// removing it (the entity despawning) writes [`PeerDisconnected`](crate::PeerDisconnected).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionPeer {
    /// The client's id (renet's client id; its SteamID64 over Steam).
    pub id: u64,
}
