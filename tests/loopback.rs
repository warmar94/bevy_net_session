//! Real UDP over loopback: a host app and a client app in ONE process, each a strict headless
//! Bevy app (ambiguity detection = Error on `Update` and `Last`; `PostUpdate` records conflicts
//! and fails on any that touches this crate's state). Every test goes through the public API
//! only: request messages in, fact messages out.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::time::TimeUpdateStrategy;
use bevy_net_session::bevy_replicon::prelude::*;
use bevy_net_session::bevy_replicon::shared::RepliconSharedPlugin;
use bevy_net_session::bevy_replicon_renet::{RenetClient, RenetServer};
use bevy_net_session::*;
use serde::{Deserialize, Serialize};

/// Simulated time per frame.
const FRAME: Duration = Duration::from_millis(20);

/// A game message from a client (stands in for "save my character", "I dismount", ...).
#[derive(Event, Serialize, Deserialize, Clone, Debug)]
struct Ping;

/// A replicated component (stands in for anything the host replicates).
#[derive(Component, Serialize, Deserialize, Clone, Debug)]
struct HostThing;

/// Every fact one app saw, in order.
#[derive(Resource, Default, Debug)]
struct Seen {
    started: Vec<SessionStarted>,
    host_failed: Vec<HostFailed>,
    accepted: Vec<JoinAccepted>,
    failed: Vec<JoinFailed>,
    peers_in: Vec<PeerConnected>,
    peers_out: Vec<PeerDisconnected>,
    ended: Vec<SessionEnded>,
    /// `Ping`s from a remote client.
    remote_pings: usize,
    /// `Ping`s delivered locally (no connection: replicon's single-player path).
    local_pings: usize,
}

#[derive(SystemParam)]
struct Facts<'w, 's> {
    started: MessageReader<'w, 's, SessionStarted>,
    host_failed: MessageReader<'w, 's, HostFailed>,
    accepted: MessageReader<'w, 's, JoinAccepted>,
    failed: MessageReader<'w, 's, JoinFailed>,
    peers_in: MessageReader<'w, 's, PeerConnected>,
    peers_out: MessageReader<'w, 's, PeerDisconnected>,
    ended: MessageReader<'w, 's, SessionEnded>,
}

fn record(mut facts: Facts, mut seen: ResMut<Seen>) {
    seen.started.extend(facts.started.read().cloned());
    seen.host_failed.extend(facts.host_failed.read().cloned());
    seen.accepted.extend(facts.accepted.read().cloned());
    seen.failed.extend(facts.failed.read().cloned());
    seen.peers_in.extend(facts.peers_in.read().cloned());
    seen.peers_out.extend(facts.peers_out.read().cloned());
    seen.ended.extend(facts.ended.read().cloned());
}

fn strict(schedule: &mut Schedule) {
    schedule.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..default() });
}

/// replicon's own `PostUpdate` send sets carry unordered pairs of their own, so `PostUpdate` only
/// records conflicts; [`assert_no_conflicts_on_ours`] then fails on any that touches our state.
fn recording(schedule: &mut Schedule) {
    schedule.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Warn, ..default() });
}

struct Options {
    version: u64,
    join_timeout: Duration,
    /// `None` = replicon's default (`ProtocolCheck`).
    auth: Option<AuthMethod>,
}

impl Default for Options {
    fn default() -> Self {
        Self { version: 1, join_timeout: Duration::from_secs(10), auth: None }
    }
}

fn app_with(options: Options) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin));
    if let Some(auth_method) = options.auth {
        app.add_plugins(RepliconPlugins.set(RepliconSharedPlugin { auth_method }));
    }
    app.add_plugins(NetSessionPlugin { protocol_version: options.version, join_timeout: options.join_timeout, ..Default::default() });
    // The "game's" protocol, identical on both sides.
    app.add_client_event::<Ping>(Channel::Ordered).replicate::<HostThing>();
    app.add_observer(|ping: On<FromClient<Ping>>, mut seen: ResMut<Seen>| {
        if ping.client_id == ClientId::Server {
            seen.local_pings += 1;
        } else {
            seen.remote_pings += 1;
        }
    });
    app.init_resource::<Seen>().add_systems(Last, record.before(NetSessionSystems::ExitClose));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(FRAME));
    app.edit_schedule(Update, strict);
    // bevy_renet's netcode and Steam server plugins each add an unordered `disconnect_on_exit`
    // to `Last` that writes `RenetServer`: with both transports compiled in, `Last` records
    // instead (checked by `assert_no_conflicts_on_ours`).
    if cfg!(all(feature = "ip", feature = "steam")) {
        app.edit_schedule(Last, recording);
    } else {
        app.edit_schedule(Last, strict);
    }
    app.edit_schedule(PostUpdate, recording);
    app.finish();
    app.cleanup();
    app.update();
    app
}

fn app() -> App {
    app_with(Options::default())
}

fn seen(app: &App) -> &Seen {
    app.world().resource::<Seen>()
}

fn session(app: &App) -> &NetSession {
    app.world().resource::<NetSession>()
}

fn count<F: bevy::ecs::query::QueryFilter>(app: &mut App) -> usize {
    let world = app.world_mut();
    let mut query = world.query_filtered::<(), F>();
    query.iter(world).count()
}

fn client_state(app: &App) -> ClientState {
    *app.world().resource::<State<ClientState>>().get()
}

/// `RenetServer` / `RenetClient`: with both transports compiled in, bevy_renet's netcode and
/// Steam systems (`send_packets` in `PostUpdate`, `disconnect_on_exit` in `Last`) conflict on them
/// between themselves; those pairs are theirs, not ours (this crate's systems never touch them).
fn is_renet_pair(world: &World, id: bevy::ecs::component::ComponentId) -> bool {
    let components = world.components();
    cfg!(all(feature = "ip", feature = "steam"))
        && (components.component_id::<RenetServer>() == Some(id) || components.component_id::<RenetClient>() == Some(id))
}

/// No unordered system pair in `PostUpdate` / `Last` conflicts on this crate's state.
fn assert_no_conflicts_on_ours(app: &App) {
    let world = app.world();
    let components = world.components();
    let ours: Vec<_> = [
        components.component_id::<NetSession>(),
        components.component_id::<RenetClient>(),
        components.component_id::<RenetServer>(),
        components.component_id::<Messages<SessionEnded>>(),
        components.component_id::<Messages<JoinFailed>>(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let schedules = world.resource::<Schedules>();
    for label in [PostUpdate.intern(), Last.intern()] {
        let schedule = schedules.get(label).expect("schedule");
        let conflicts = &schedule.graph().conflicting_systems().0;
        println!(">>> {label:?}: {} unordered conflicting pairs (library-internal)", conflicts.len());
        for (_, _, on) in conflicts {
            assert!(!on.iter().any(|id| ours.contains(id) && !is_renet_pair(world, *id)), "an unordered {label:?} pair on the session state");
        }
    }
}

/// Update every app once per frame, with a short real pause so loopback packets arrive, until
/// `done` or `max` frames. Returns the frames it took.
fn run_until(apps: &mut [&mut App], max: usize, mut done: impl FnMut(&mut [&mut App]) -> bool) -> usize {
    for frame in 0..max {
        if done(apps) {
            return frame;
        }
        for app in apps.iter_mut() {
            app.update();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(done(apps), "condition not reached in {max} frames");
    max
}

fn run(apps: &mut [&mut App], frames: usize) {
    for _ in 0..frames {
        for app in apps.iter_mut() {
            app.update();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Start hosting on a free UDP port; returns the loopback address to join.
fn host_on_loopback(host: &mut App) -> SocketAddr {
    host.world_mut().write_message(HostSession::ip(0, 4));
    host.update();
    let started = seen(host).started.last().cloned().expect("SessionStarted");
    assert_eq!(started.transport, TransportKind::Ip);
    let port = started.local_addr.expect("a UDP host reports its address").port();
    assert_ne!(port, 0, "the real port, not the requested 0");
    assert_eq!(session(host).state(), SessionState::Listening);
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// A host that only lets `secret` in, and a client that joined it.
fn joined_pair() -> (App, App) {
    let mut host = app();
    host.insert_resource(JoinValidatorRes::new(|req: &JoinRequestInfo| if req.payload == b"secret" { Ok(()) } else { Err("wrong password".to_string()) }));
    let addr = host_on_loopback(&mut host);
    let mut client = app();
    client.world_mut().write_message(JoinSession::ip(addr).with_payload("secret"));
    let frames = run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[1]).accepted.is_empty() && !seen(apps[0]).peers_in.is_empty());
    println!(">>> joined over UDP loopback in {frames} frames");
    (host, client)
}

#[test]
fn a_client_with_the_right_version_and_payload_is_accepted() {
    let (host, client) = joined_pair();
    let accepted = &seen(&client).accepted;
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].transport, TransportKind::Ip);
    let client_id = session(&client).local_id().expect("a client knows its id");
    assert_eq!(accepted[0].local_id, client_id);
    let peers = &seen(&host).peers_in;
    assert_eq!(peers.len(), 1, "exactly one PeerConnected despite the resends");
    assert_eq!(peers[0].id, client_id, "the host sees the client's own id");
    assert_eq!(host.world().get::<SessionPeer>(peers[0].entity).map(|p| p.id), Some(client_id));
    assert_eq!((session(&client).role(), session(&client).state()), (SessionRole::Client, SessionState::Joined));
    assert_eq!((session(&host).role(), session(&host).state()), (SessionRole::Host, SessionState::Listening));
    assert!(seen(&client).failed.is_empty() && seen(&host).failed.is_empty());
    assert_eq!(protocol_hash(&host), protocol_hash(&client), "same stack, same protocol");
    assert_no_conflicts_on_ours(&host);
    assert_no_conflicts_on_ours(&client);
}

#[test]
fn a_client_on_another_protocol_version_is_refused_as_a_version_mismatch() {
    let mut host = app();
    let addr = host_on_loopback(&mut host);
    let mut client = app_with(Options { version: 2, ..Default::default() });
    assert_ne!(protocol_hash(&host), protocol_hash(&client), "the version is part of the protocol hash");
    client.world_mut().write_message(JoinSession::ip(addr));
    run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[1]).failed.is_empty());
    run(&mut [&mut host, &mut client], 20);
    let failed = &seen(&client).failed;
    println!(">>> {failed:?}");
    assert_eq!(failed.len(), 1, "reported exactly once");
    assert_eq!(failed[0].reason, JoinFailReason::VersionMismatch);
    assert!(seen(&client).accepted.is_empty());
    assert!(seen(&host).peers_in.is_empty(), "never a peer");
    assert_eq!(session(&client).state(), SessionState::Idle);
    assert!(!client.world().contains_resource::<RenetClient>());
}

/// Without replicon's protocol check the handshake's own version check refuses the client, and
/// the host's number reaches it.
#[test]
fn without_replicons_protocol_check_the_handshake_still_refuses_another_version() {
    let mut host = app_with(Options { auth: Some(AuthMethod::None), ..Default::default() });
    let addr = host_on_loopback(&mut host);
    let mut client = app_with(Options { version: 7, auth: Some(AuthMethod::None), ..Default::default() });
    client.world_mut().write_message(JoinSession::ip(addr));
    run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[1]).failed.is_empty());
    let failed = &seen(&client).failed;
    assert_eq!(failed[0].reason, JoinFailReason::VersionMismatch);
    assert!(failed[0].message.contains("protocol 1") && failed[0].message.contains("this build 7"), "{failed:?}");
    assert!(seen(&host).peers_in.is_empty());
}

#[test]
fn a_validator_rejection_reaches_the_client_with_its_reason() {
    let mut host = app();
    host.insert_resource(JoinValidatorRes::new(|req: &JoinRequestInfo| if req.payload == b"secret" { Ok(()) } else { Err("wrong password".to_string()) }));
    let addr = host_on_loopback(&mut host);
    let mut client = app();
    client.world_mut().write_message(JoinSession::ip(addr).with_payload("guess"));
    run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[1]).failed.is_empty());
    run(&mut [&mut host, &mut client], 20);
    let failed = &seen(&client).failed;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].reason, JoinFailReason::Rejected("wrong password".to_string()));
    assert!(failed[0].message.is_ascii());
    assert!(seen(&host).peers_in.is_empty() && seen(&host).peers_out.is_empty());
    assert_eq!(session(&client).state(), SessionState::Idle);
    assert_eq!(session(&host).state(), SessionState::Listening, "a rejection never stops the host");
    // The refused client was disconnected by the host.
    assert_eq!(host.world().resource::<RenetServer>().connected_clients(), 0);
}

#[test]
fn a_host_that_never_answers_times_out() {
    // A socket that takes the packets and never replies (a closed port could answer with an
    // ICMP error, which is "could not reach", not a timeout).
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let addr = silent.local_addr().expect("addr");
    let mut client = app_with(Options { join_timeout: Duration::from_secs(1), ..Default::default() });
    client.world_mut().write_message(JoinSession::ip(addr));
    let frames = run_until(&mut [&mut client], 200, |apps| !seen(apps[0]).failed.is_empty());
    println!(">>> timed out after {frames} frames of {} ms", FRAME.as_millis());
    assert!(frames >= 45, "not before the timeout ({frames} frames)");
    run(&mut [&mut client], 60);
    let failed = &seen(&client).failed;
    assert_eq!(failed.len(), 1, "reported once");
    assert_eq!(failed[0].reason, JoinFailReason::Timeout);
    assert_eq!(session(&client).state(), SessionState::Idle);
    assert!(!client.world().contains_resource::<RenetClient>());
    drop(silent);
}

#[test]
fn the_host_going_away_is_a_lost_connection_for_its_client() {
    let (mut host, mut client) = joined_pair();
    host.world_mut().write_message(LeaveSession);
    run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[1]).ended.is_empty());
    assert_eq!(seen(&client).ended, vec![SessionEnded { reason: SessionEndReason::LostConnection }]);
    assert_eq!(seen(&host).ended, vec![SessionEnded { reason: SessionEndReason::Left }]);
    assert_eq!(seen(&host).peers_out.len(), 1, "the host saw its peer go");
    assert_eq!(session(&client).state(), SessionState::Idle);
    assert!(!host.world().contains_resource::<RenetServer>());
}

#[test]
fn leaving_despawns_the_hosts_entities_and_restores_local_delivery() {
    let (mut host, mut client) = joined_pair();
    host.world_mut().spawn((Replicated, HostThing));
    run_until(&mut [&mut host, &mut client], 300, |apps| count::<(With<Remote>, With<HostThing>)>(apps[1]) == 1);
    let own = client.world_mut().spawn(HostThing).id();

    client.world_mut().write_message(LeaveSession);
    run(&mut [&mut host, &mut client], 3);
    let remote = count::<With<Remote>>(&mut client);
    assert_eq!(remote, 0, "every entity the host replicated went with the session");
    assert!(client.world().get_entity(own).is_ok(), "a local entity is not the host's");
    assert_eq!(client_state(&client), ClientState::Disconnected);
    assert_eq!(seen(&client).ended, vec![SessionEnded { reason: SessionEndReason::Left }]);

    client.world_mut().commands().client_trigger(Ping);
    run(&mut [&mut client], 2);
    assert_eq!(seen(&client).local_pings, 1, "single-player delivery works again");
    run_until(&mut [&mut host, &mut client], 300, |apps| !seen(apps[0]).peers_out.is_empty());
}

#[test]
fn a_message_sent_on_the_leave_frame_reaches_the_host() {
    let (mut host, mut client) = joined_pair();
    client.world_mut().commands().client_trigger(Ping);
    client.world_mut().write_message(LeaveSession);
    run_until(&mut [&mut host, &mut client], 300, |apps| !seen(apps[0]).peers_out.is_empty());
    assert_eq!(seen(&host).remote_pings, 1, "the leave frame's message arrived before the disconnect");
    assert_eq!(seen(&client).local_pings, 0, "sent to the host, not delivered locally");
    assert_eq!(seen(&client).ended, vec![SessionEnded { reason: SessionEndReason::Left }]);
}

#[test]
fn a_new_request_replaces_the_session() {
    let (mut host, mut client) = joined_pair();
    client.world_mut().write_message(HostSession::ip(0, 2));
    run_until(&mut [&mut host, &mut client], 100, |apps| !seen(apps[1]).started.is_empty());
    assert_eq!(seen(&client).ended, vec![SessionEnded { reason: SessionEndReason::Replaced }]);
    assert_eq!(session(&client).state(), SessionState::Listening);
    assert!(client.world().contains_resource::<RenetServer>() && !client.world().contains_resource::<RenetClient>());
    run_until(&mut [&mut host, &mut client], 500, |apps| !seen(apps[0]).peers_out.is_empty());
    assert_eq!(seen(&client).failed, vec![], "no stale join watchdog");
}

#[test]
fn leaving_a_join_still_in_progress_cancels_it() {
    let silent = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let mut client = app();
    client.world_mut().write_message(JoinSession::ip(silent.local_addr().expect("addr")));
    run(&mut [&mut client], 5);
    client.world_mut().write_message(LeaveSession);
    run(&mut [&mut client], 3);
    let failed = &seen(&client).failed;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].reason, JoinFailReason::Cancelled);
    assert!(seen(&client).ended.is_empty(), "no session had started");
    assert_eq!(session(&client).state(), SessionState::Idle);
}

#[test]
fn app_exit_closes_every_connection() {
    let (mut host, mut client) = joined_pair();
    client.world_mut().write_message(AppExit::Success);
    client.update();
    assert!(!client.world().contains_resource::<RenetClient>(), "closed on the exit frame");
    // Written on the exit frame itself; a (normally absent) next frame is needed to read it here.
    client.update();
    assert_eq!(seen(&client).ended, vec![SessionEnded { reason: SessionEndReason::AppExit }]);
    run_until(&mut [&mut host], 300, |apps| !seen(apps[0]).peers_out.is_empty());

    host.world_mut().write_message(AppExit::Success);
    host.update();
    assert!(!host.world().contains_resource::<RenetServer>());
    assert_eq!(session(&host).state(), SessionState::Idle);
}

#[test]
fn hosting_on_a_port_in_use_fails_cleanly() {
    let taken = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind");
    let port = taken.local_addr().expect("addr").port();
    let mut host = app();
    host.world_mut().write_message(HostSession::ip(port, 4));
    host.update();
    let failed = &seen(&host).host_failed;
    assert_eq!(failed.len(), 1, "{:?}", seen(&host));
    assert_eq!(failed[0].reason, HostFailReason::CouldNotStart);
    assert_eq!(session(&host).state(), SessionState::Idle);
    assert!(!host.world().contains_resource::<RenetServer>());
}
