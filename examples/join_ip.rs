//! Join a UDP host, stay for ten seconds, then leave. Exits when the session is over.
//!
//! ```text
//! cargo run --example host_ip                                  # in one terminal
//! cargo run --example join_ip -- 127.0.0.1:5000                # in another
//! cargo run --example join_ip -- 127.0.0.1:5000 hunter2        # with a password
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use bevy::app::ScheduleRunnerPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

/// Must match the host's (see `host_ip`).
const PROTOCOL_VERSION: u64 = 1;

/// How long to stay once joined.
const STAY: Duration = Duration::from_secs(10);

fn main() {
    let Some(addr) = std::env::args().nth(1).and_then(|a| a.parse::<SocketAddr>().ok()) else {
        eprintln!("usage: join_ip <host ip:port> [password]");
        return;
    };
    let password = std::env::args().nth(2).unwrap_or_default();

    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(1.0 / 60.0))),
            LogPlugin::default(),
            StatesPlugin,
            NetSessionPlugin { protocol_version: PROTOCOL_VERSION, ..default() },
        ))
        .add_systems(Startup, move |mut join: MessageWriter<JoinSession>| {
            join.write(JoinSession::ip(addr).with_payload(password.clone()));
        })
        .add_systems(Update, (report, leave_later).after(NetSessionSystems::Watch))
        .run();
}

fn report(
    mut accepted: MessageReader<JoinAccepted>,
    mut failed: MessageReader<JoinFailed>,
    mut ended: MessageReader<SessionEnded>,
    mut exit: MessageWriter<AppExit>,
) {
    for ev in accepted.read() {
        info!("joined as client {}; leaving in {} s", ev.local_id, STAY.as_secs());
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
