//! Join a Steam host by its SteamID64, stay for ten seconds, then leave.
//!
//! Needs the `steam` feature, a running, logged-in Steam client (another account than the
//! host's, a friend of it for the default friends-only host) and the host's SteamID64 (printed by
//! `host_steam`). The app id is read from `STEAM_APP_ID` (default `480`).
//!
//! ```text
//! cargo run --example join_steam --features steam -- <host SteamID64>
//! ```
//!
//! In a real game the host's id usually comes from a Steam lobby (an invite or "Join Game"), not
//! from the command line.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

/// Must match the host's (see `host_steam`).
const PROTOCOL_VERSION: u64 = 1;

/// How long to stay once joined.
const STAY: Duration = Duration::from_secs(10);

fn main() {
    let Some(host) = std::env::args().nth(1).and_then(|a| a.trim().parse::<u64>().ok()) else {
        eprintln!("usage: join_steam <host SteamID64>");
        return;
    };
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
        .add_systems(Startup, move |mut join: MessageWriter<JoinSession>| {
            join.write(JoinSession::steam(host));
        })
        .add_systems(Update, (report, leave_later).after(NetSessionSystems::Watch))
        .run();
}

/// The one Steam callback pump of this process.
fn pump_steam(steam: Res<SteamNetClient>) {
    steam.0.run_callbacks();
}

fn report(
    mut accepted: MessageReader<JoinAccepted>,
    mut failed: MessageReader<JoinFailed>,
    mut ended: MessageReader<SessionEnded>,
    mut exit: MessageWriter<AppExit>,
) {
    for ev in accepted.read() {
        info!("joined the Steam host as {}; leaving in {} s", ev.local_id, STAY.as_secs());
    }
    for ev in failed.read() {
        error!("join failed: {:?} ({})", ev.reason, ev.message);
        exit.write(AppExit::error());
    }
    for ev in ended.read() {
        info!("session ended: {:?}", ev.reason);
        exit.write(AppExit::Success);
    }
}

fn leave_later(session: Res<NetSession>, time: Res<Time>, mut joined_for: Local<Duration>, mut leave: MessageWriter<LeaveSession>) {
    if !session.is_joined() {
        return;
    }
    let before = *joined_for;
    *joined_for += time.delta();
    if before < STAY && *joined_for >= STAY {
        leave.write(LeaveSession);
    }
}
