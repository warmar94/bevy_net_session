//! Host a session over Steam P2P, reachable by your Steam friends, and print who joins and
//! leaves. Stop it with Ctrl+C.
//!
//! Needs the `steam` feature and a running, logged-in Steam client. The app id is read from the
//! `STEAM_APP_ID` environment variable (default `480`, Valve's public test app "Spacewar").
//!
//! ```text
//! cargo run --example host_steam --features steam
//! ```
//!
//! The example prints the SteamID64 to join; on a friend's machine (another account):
//! `cargo run --example join_steam --features steam -- <that SteamID64>`.
//!
//! This example pumps Steam callbacks itself (one system in `First`). In a game that also uses a
//! Steam lobby plugin, let THAT plugin pump them instead: exactly one pump per frame.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

/// Must match the joiner's (see `join_steam`).
const PROTOCOL_VERSION: u64 = 1;

fn main() {
    let app_id: u32 = std::env::var("STEAM_APP_ID").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(480);
    let steam = match steamworks::Client::init_app(app_id) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Steam could not start (is the Steam client running and logged in?): {e}");
            return;
        }
    };
    steam.networking_utils().init_relay_network_access();

    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / 60.0))),
            LogPlugin::default(),
            StatesPlugin,
            NetSessionPlugin { protocol_version: PROTOCOL_VERSION, ..default() },
        ))
        .insert_resource(SteamNetClient(steam))
        .add_systems(First, pump_steam)
        .add_systems(Startup, |mut host: MessageWriter<HostSession>| {
            host.write(HostSession::steam(4));
        })
        .add_systems(Update, report.after(NetSessionSystems::Watch))
        .run();
}

/// The one Steam callback pump of this process.
fn pump_steam(steam: Res<SteamNetClient>) {
    steam.0.run_callbacks();
}

fn report(
    mut started: MessageReader<SessionStarted>,
    mut failed: MessageReader<HostFailed>,
    mut peers_in: MessageReader<PeerConnected>,
    mut peers_out: MessageReader<PeerDisconnected>,
    mut exit: MessageWriter<AppExit>,
) {
    for ev in started.read() {
        info!("hosting on Steam; friends join with: join_steam {}", ev.steam_id.unwrap_or_default());
    }
    for ev in failed.read() {
        error!("could not host: {:?} ({})", ev.reason, ev.message);
        exit.write(AppExit::error());
    }
    for ev in peers_in.read() {
        info!("Steam user {} joined", ev.id);
    }
    for ev in peers_out.read() {
        info!("Steam user {} left", ev.id);
    }
}
