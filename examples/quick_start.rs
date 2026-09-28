//! The whole session lifecycle in one process, over real UDP on loopback: no second machine, no
//! Steam.
//!
//! 1. The host opens a session on a free port (`SessionStarted`) and installs a validator that
//!    only lets in clients presenting the password `open sesame`.
//! 2. A client with the wrong password is refused (`JoinFailed`, `Rejected`).
//! 3. A client with the right one is accepted (`JoinAccepted` on its side, `PeerConnected` on the
//!    host's).
//! 4. The client leaves (`SessionEnded`, `Left`; the host sees `PeerDisconnected`).
//!
//! `cargo run --example quick_start`

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

/// Your game's wire-protocol version: bump it whenever what crosses the wire changes.
const PROTOCOL_VERSION: u64 = 1;

/// Which app this is, for the printout.
#[derive(Resource)]
struct Who(&'static str);

fn peer(name: &'static str) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin { protocol_version: PROTOCOL_VERSION, ..default() }))
        .insert_resource(Who(name))
        .add_systems(Update, print_facts.after(NetSessionSystems::Watch));
    app
}

/// Print every fact the plugin writes.
fn print_facts(
    name: Res<Who>,
    mut started: MessageReader<SessionStarted>,
    mut accepted: MessageReader<JoinAccepted>,
    mut failed: MessageReader<JoinFailed>,
    mut peers_in: MessageReader<PeerConnected>,
    mut peers_out: MessageReader<PeerDisconnected>,
    mut ended: MessageReader<SessionEnded>,
) {
    let n = name.0;
    for ev in started.read() {
        println!("[{n}] hosting on {:?}", ev.local_addr);
    }
    for ev in accepted.read() {
        println!("[{n}] joined as client {}", ev.local_id);
    }
    for ev in failed.read() {
        println!("[{n}] join failed: {:?} ({})", ev.reason, ev.message);
    }
    for ev in peers_in.read() {
        println!("[{n}] client {} joined", ev.id);
    }
    for ev in peers_out.read() {
        println!("[{n}] client {} left", ev.id);
    }
    for ev in ended.read() {
        println!("[{n}] session ended: {:?}", ev.reason);
    }
}

/// Run both apps for `frames` frames of ~10 ms.
fn run(host: &mut App, client: &mut App, frames: usize) {
    for _ in 0..frames {
        host.update();
        client.update();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn main() {
    let mut host = peer("host");
    host.insert_resource(JoinValidatorRes::new(|req: &JoinRequestInfo| if req.payload == b"open sesame" { Ok(()) } else { Err("wrong password".to_string()) }));
    let mut client = peer("client");
    host.finish();
    host.cleanup();
    client.finish();
    client.cleanup();

    // 1. Host on a free port.
    host.world_mut().write_message(HostSession::ip(0, 4));
    run(&mut host, &mut client, 2);
    let Some(port) = host.world().resource::<NetSession>().local_addr().map(|a| a.port()) else {
        eprintln!("could not host (is UDP available?)");
        return;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    // 2. The wrong password.
    client.world_mut().write_message(JoinSession::ip(addr).with_payload("let me in"));
    run(&mut host, &mut client, 50);

    // 3. The right one.
    client.world_mut().write_message(JoinSession::ip(addr).with_payload("open sesame"));
    run(&mut host, &mut client, 50);
    println!("client state: {:?}", client.world().resource::<NetSession>().state());

    // 4. Leave.
    client.world_mut().write_message(LeaveSession);
    run(&mut host, &mut client, 50);
    println!("client state: {:?}", client.world().resource::<NetSession>().state());
}
