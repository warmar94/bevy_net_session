//! Steam specifics (feature `steam`): the game's Steam client handle, the access mapping and the
//! Steam client's close grace.

use std::collections::HashSet;
use std::time::Duration;

use bevy_ecs::prelude::*;
use bevy_replicon_renet::steam::{AccessPermission, SteamClientTransport};
use bevy_replicon_renet::RenetClient;
use tracing::warn;

use crate::messages::SteamAccess;
use crate::transport::ClosingLink;

/// The game's Steam client, for the Steam transport. Insert it once Steam is initialised
/// (`steamworks::Client::init_app`); without it a Steam host or join fails with
/// `TransportUnavailable`. It keeps a clone of the client alive: Steam shuts down when the last
/// clone drops.
///
/// The plugin never pumps Steam callbacks. The Steam transport's connection events arrive through
/// them, so exactly one system in the game must call `run_callbacks` (or `process_callbacks`)
/// once per frame, e.g. a Steam lobby plugin's pump.
#[derive(Resource, Clone)]
pub struct SteamNetClient(pub steamworks::Client);

/// Our access setting as renet_steam's.
pub(crate) fn access_permission(access: &SteamAccess) -> AccessPermission {
    match access {
        SteamAccess::FriendsOnly => AccessPermission::FriendsOnly,
        SteamAccess::Public => AccessPermission::Public,
        SteamAccess::Private => AccessPermission::Private,
        SteamAccess::InLobby(lobby) => AccessPermission::InLobby(steamworks::LobbyId::from_raw(*lobby)),
        SteamAccess::InList(ids) => AccessPermission::InList(ids.iter().map(|id| steamworks::SteamId::from_raw(*id)).collect::<HashSet<_>>()),
    }
}

/// The Steam client in its close grace: the three steps bevy_renet's Steam client plugin runs
/// each frame (renet update, transport receive, transport send), on a link it no longer sees.
pub(crate) struct SteamClientLinger {
    pub(crate) client: RenetClient,
    pub(crate) transport: SteamClientTransport,
}

impl ClosingLink for SteamClientLinger {
    fn pump(&mut self, dt: Duration) {
        self.client.update(dt);
        self.transport.update(&mut self.client);
        if !self.client.is_disconnected() {
            if let Err(e) = self.transport.send_packets(&mut self.client) {
                warn!("net session: closing Steam link: send failed ({e})");
            }
        }
    }

    fn close(&mut self) {
        self.client.disconnect();
        self.transport.disconnect();
    }
}
