//! Host a session over UDP and print who joins and leaves. Stop it with Ctrl+C.
//!
//! ```text
//! cargo run --example host_ip                  # port 5000, no password
//! cargo run --example host_ip -- 5000 hunter2  # port 5000, password "hunter2"
//! ```
//!
//! Then, from this or another machine on the network: `cargo run --example join_ip -- <ip>:5000
//! [password]`. The port must be reachable (firewall / router) for joins from another network.

use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

/// Must match the joiner's (see `join_ip`).
const PROTOCOL_VERSION: u64 = 1;

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|p| p.parse().ok()).unwrap_or(5000);
    let password = std::env::args().nth(2).unwrap_or_default();

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / 60.0))),
        LogPlugin::default(),
        StatesPlugin,
        NetSessionPlugin { protocol_version: PROTOCOL_VERSION, ..default() },
    ));
    if !password.is_empty() {
        // The joiner presents its password as the join payload; the game decides what it means.
        app.insert_resource(JoinValidatorRes::new(
            move |req: &JoinRequestInfo| {
                if req.payload == password.as_bytes() {
                    Ok(())
                } else {
                    Err("wrong password".to_string())
                }
            },
        ));
    }
    app.add_systems(Startup, move |mut host: MessageWriter<HostSession>| {
        host.write(HostSession::ip(port, 8));
    })
    .add_systems(Update, report.after(NetSessionSystems::Watch))
    .run();
}

fn report(
    mut started: MessageReader<SessionStarted>,
    mut failed: MessageReader<HostFailed>,
    mut peers_in: MessageReader<PeerConnected>,
    mut peers_out: MessageReader<PeerDisconnected>,
    mut exit: MessageWriter<AppExit>,
) {
    for ev in started.read() {
        info!("hosting on {:?}; waiting for joiners", ev.local_addr);
    }
    for ev in failed.read() {
        error!("could not host: {:?} ({})", ev.reason, ev.message);
        exit.write(AppExit::error());
    }
    for ev in peers_in.read() {
        info!("client {} joined", ev.id);
    }
    for ev in peers_out.read() {
        info!("client {} left", ev.id);
    }
}
