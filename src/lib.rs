//! Game-agnostic multiplayer sessions for Bevy on `bevy_replicon` + renet.
//!
//! The game sends requests ([`HostSession`], [`JoinSession`], [`LeaveSession`]) and reacts to
//! facts ([`SessionStarted`], [`HostFailed`], [`JoinAccepted`], [`JoinFailed`],
//! [`PeerConnected`], [`PeerDisconnected`], [`SessionEnded`]). Two transports, each a cargo
//! feature: `ip` (UDP, renet netcode, on by default) and `steam` (Steam P2P, renet_steam).
//!
//! Every join goes through a handshake: the client presents the app's protocol version and an
//! opaque payload; the host checks the version, then asks the game's [`JoinValidator`]. The
//! protocol version is also part of replicon's protocol hash, so a client on another build is
//! refused on every transport. What a session MEANS (which world, which character) is the game's
//! own replicated data, sent after [`JoinAccepted`] / [`PeerConnected`].
//!
//! Schedules: [`NetSessionSystems::Requests`] then [`NetSessionSystems::Watch`] in `Update`;
//! [`NetSessionSystems::Close`] in `PostUpdate` after renet's `RenetSend` (a leave is executed
//! only after the frame's last packets went out); [`NetSessionSystems::ExitClose`] in `Last` on
//! the frame an `AppExit` is written.
#![warn(missing_docs)]

mod handshake;
mod messages;
mod requests;
mod session;
#[cfg(feature = "steam")]
mod steam;
#[cfg(test)]
mod tests;
mod transport;

/// Every Rust example in the README compiles (checked by `cargo test --all-features`).
#[cfg(all(doctest, feature = "ip", feature = "steam"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

use std::time::Duration;

/// The replicon this crate is built on (use it to be sure your versions match).
pub use bevy_replicon;
/// The replicon renet backend this crate is built on.
pub use bevy_replicon_renet;
pub use handshake::{JoinRequestInfo, JoinValidator, JoinValidatorRes};
pub use messages::{
    HostFailReason, HostFailed, HostSession, HostTransport, JoinAccepted, JoinFailReason, JoinFailed, JoinSession, JoinTarget, LeaveSession, PeerConnected,
    PeerDisconnected, SessionEndReason, SessionEnded, SessionStarted, SteamAccess, TransportKind,
};
pub use session::{NetSession, SessionPeer, SessionRole, SessionState};
#[cfg(feature = "steam")]
pub use steam::SteamNetClient;

use bevy_app::{App, AppExit, Last, Plugin, PostUpdate, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::common_conditions::on_message;
use bevy_replicon::prelude::*;
use bevy_replicon::shared::RepliconSharedPlugin;
use bevy_replicon_renet::{RenetSend, RepliconRenetClientPlugin, RepliconRenetPlugins, RepliconRenetServerPlugin};
use tracing::warn;

/// The UDP transport's hard cap on connected clients (renet netcode panics above it; requests
/// are clamped).
pub const MAX_IP_CLIENTS: usize = 1024;

/// The default netcode protocol id: a constant of this crate, so any two builds using it can
/// reach each other and a version difference surfaces as [`JoinFailReason::VersionMismatch`]
/// instead of a silent UDP timeout.
pub const DEFAULT_NETCODE_PROTOCOL_ID: u64 = 0x6E65_7473_6573_7331;

// ---------------------------------------------------------------------------------------------
// Plugin + config
// ---------------------------------------------------------------------------------------------

/// The plugin. Add it once, on every peer, at the same point in your plugin order (see
/// [`protocol_hash`]).
///
/// It adds `RepliconPlugins` and `RepliconRenetPlugins` when they are not added yet; to configure
/// them, add them yourself BEFORE this plugin. Like replicon, it needs Bevy's `StatesPlugin`
/// (part of `DefaultPlugins`; add it to `MinimalPlugins` yourself).
#[derive(Clone, Debug)]
pub struct NetSessionPlugin {
    /// The app's wire-protocol version. Bump it on every change of what crosses the wire. It is
    /// checked in the join handshake and added to replicon's protocol hash. Default `0`.
    pub protocol_version: u64,
    /// How long a join may take before it fails with [`JoinFailReason::Timeout`]. Each join has
    /// its own clock (wall-clock time). Default 15 s.
    pub join_timeout: Duration,
    /// How long a connection keeps being pumped after a [`LeaveSession`] (a connected client,
    /// and a UDP host with clients), so the last messages of the session are delivered, and
    /// resent once if lost, before the disconnect. The session itself ends at once. Capped at
    /// 5 s; zero closes immediately. Default 350 ms (one renet reliable resend plus a margin).
    pub client_close_grace: Duration,
    /// netcode's protocol id for the UDP transport. Two peers with different ids never connect
    /// (the join ends as [`JoinFailReason::CouldNotReach`] or `Timeout`). Default
    /// [`DEFAULT_NETCODE_PROTOCOL_ID`].
    pub netcode_protocol_id: u64,
}

impl Default for NetSessionPlugin {
    fn default() -> Self {
        Self {
            protocol_version: 0,
            join_timeout: Duration::from_secs(15),
            client_close_grace: Duration::from_millis(350),
            netcode_protocol_id: DEFAULT_NETCODE_PROTOCOL_ID,
        }
    }
}

/// The plugin's configuration as a resource (copied from [`NetSessionPlugin`]; read-only).
#[derive(Resource, Clone, Debug)]
pub struct NetSessionConfig {
    /// See [`NetSessionPlugin::protocol_version`].
    pub protocol_version: u64,
    /// See [`NetSessionPlugin::join_timeout`].
    pub join_timeout: Duration,
    /// See [`NetSessionPlugin::client_close_grace`].
    pub client_close_grace: Duration,
    /// See [`NetSessionPlugin::netcode_protocol_id`].
    pub netcode_protocol_id: u64,
}

/// The plugin's system sets.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NetSessionSystems {
    /// `Update`: handle [`HostSession`] / [`JoinSession`] / [`LeaveSession`] and open transports.
    Requests,
    /// `Update`, after `Requests`: send (and repeat) the join request, run the join timeout,
    /// notice a joined client losing its host.
    Watch,
    /// `PostUpdate`, after renet's `RenetSend`: run the close a [`LeaveSession`] asked for, pump
    /// a Steam client link in its close grace.
    Close,
    /// `Last`, only on a frame with an `AppExit`: close every connection.
    ExitClose,
}

impl Plugin for NetSessionPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<RepliconSharedPlugin>() {
            app.add_plugins(RepliconPlugins);
        }
        if !app.is_plugin_added::<RepliconRenetClientPlugin>() && !app.is_plugin_added::<RepliconRenetServerPlugin>() {
            app.add_plugins(RepliconRenetPlugins);
        }
        // The version is part of replicon's protocol hash: a peer on another build is refused by
        // replicon's own check on every transport (the Steam one has no protocol id of its own).
        match app.world_mut().get_resource_mut::<ProtocolHasher>() {
            Some(mut hasher) => hasher.add_custom(("bevy_net_session", self.protocol_version)),
            None => warn!("net session: replicon's ProtocolHasher is gone; add NetSessionPlugin before app.finish()"),
        }

        app.insert_resource(NetSessionConfig {
            protocol_version: self.protocol_version,
            join_timeout: self.join_timeout,
            client_close_grace: self.client_close_grace,
            netcode_protocol_id: self.netcode_protocol_id,
        })
        .init_resource::<NetSession>()
        .init_resource::<handshake::ClientJoin>()
        .init_resource::<transport::PendingClose>()
        .init_resource::<requests::DeferredStart>()
        .add_message::<HostSession>()
        .add_message::<JoinSession>()
        .add_message::<LeaveSession>()
        .add_message::<SessionStarted>()
        .add_message::<HostFailed>()
        .add_message::<JoinAccepted>()
        .add_message::<JoinFailed>()
        .add_message::<PeerConnected>()
        .add_message::<PeerDisconnected>()
        .add_message::<SessionEnded>()
        // The handshake. Registration order is part of the protocol: every peer adds this plugin
        // at the same point.
        .add_client_event::<handshake::SessionJoinRequest>(Channel::Ordered)
        .add_server_event::<handshake::SessionJoinReply>(Channel::Ordered)
        // Sent at once, not held until the client is replication-authorized (no entity inside).
        .make_event_independent::<handshake::SessionJoinReply>()
        .add_observer(handshake::on_join_request)
        .add_observer(handshake::on_join_reply)
        .add_observer(handshake::on_protocol_mismatch)
        .add_observer(handshake::on_peer_removed)
        .configure_sets(Update, (NetSessionSystems::Requests, NetSessionSystems::Watch).chain())
        .configure_sets(PostUpdate, NetSessionSystems::Close.after(RenetSend))
        .configure_sets(Last, NetSessionSystems::ExitClose)
        .add_systems(Update, requests::handle_requests.in_set(NetSessionSystems::Requests))
        .add_systems(Update, (handshake::send_join_request, handshake::join_watchdog).chain().in_set(NetSessionSystems::Watch))
        .add_systems(PostUpdate, (transport::flush_pending_close, transport::drive_closing_client).chain().in_set(NetSessionSystems::Close))
        .add_systems(Last, transport::close_on_exit.run_if(on_message::<AppExit>).in_set(NetSessionSystems::ExitClose));
    }
}

/// Replicon's protocol hash of `app`: every replicated component, event and message it
/// registered, in order, plus custom data such as [`NetSessionPlugin::protocol_version`].
/// `None` until `app.finish()` ran (the hash is computed there).
///
/// Two peers can talk only if their hashes are equal, and the hash depends on the ORDER of
/// registrations. Register replicated types in one shared function/plugin that the client and
/// the server both call, and assert it in a test:
///
/// ```
/// # use bevy::prelude::*;
/// # use bevy::state::app::StatesPlugin;
/// # use bevy_net_session::*;
/// fn stack(dedicated_server: bool) -> App {
///     let mut app = App::new();
///     app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin { protocol_version: 3, ..Default::default() }));
///     // ... the game's shared protocol registration here, identical on both ...
///     let _ = dedicated_server;
///     app.finish();
///     app.cleanup();
///     app
/// }
/// assert_eq!(protocol_hash(&stack(false)), protocol_hash(&stack(true)));
/// assert!(protocol_hash(&stack(false)).is_some());
/// ```
pub fn protocol_hash(app: &App) -> Option<ProtocolHash> {
    app.world().get_resource::<ProtocolHash>().copied()
}
