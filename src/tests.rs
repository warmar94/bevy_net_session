//! Unit tests of the internals that need no network: the close grace, the non-send transport
//! helpers, the join watchdog against a fake renet client, request precedence, the missing
//! transports, and replicon's protocol-mismatch event.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy::time::TimeUpdateStrategy;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::{RenetClient, RenetServer};

use crate::handshake::{ClientJoin, SessionJoinRequest};
use crate::transport::{begin_closing, connection_config, finish_closing, generate_client_id, ClosingClient, ClosingLink};
use crate::*;

#[derive(Resource, Default)]
struct Seen {
    started: Vec<SessionStarted>,
    host_failed: Vec<HostFailed>,
    failed: Vec<JoinFailed>,
    ended: Vec<SessionEnded>,
    accepted: Vec<JoinAccepted>,
}

fn record(
    mut started: MessageReader<SessionStarted>,
    mut host_failed: MessageReader<HostFailed>,
    mut failed: MessageReader<JoinFailed>,
    mut ended: MessageReader<SessionEnded>,
    mut accepted: MessageReader<JoinAccepted>,
    mut seen: ResMut<Seen>,
) {
    seen.started.extend(started.read().cloned());
    seen.host_failed.extend(host_failed.read().cloned());
    seen.failed.extend(failed.read().cloned());
    seen.ended.extend(ended.read().cloned());
    seen.accepted.extend(accepted.read().cloned());
}

fn strict(schedule: &mut Schedule) {
    schedule.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..default() });
}

/// `Last` is strict, except with BOTH transports compiled in: bevy_renet's netcode and Steam
/// server plugins each add an unordered `disconnect_on_exit` to `Last` that writes `RenetServer`
/// (a library-internal pair this crate cannot order). Our own `Last` system only queues a command.
fn last_schedule(schedule: &mut Schedule) {
    let level = if cfg!(all(feature = "ip", feature = "steam")) { LogLevel::Warn } else { LogLevel::Error };
    schedule.set_build_settings(ScheduleBuildSettings { ambiguity_detection: level, ..default() });
}

/// A headless app with the plugin, 50 ms wall-clock frames, strict `Update` and `Last`.
fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin { join_timeout: Duration::from_secs(2), ..default() }));
    app.init_resource::<Seen>().add_systems(PostUpdate, record);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(50)));
    app.edit_schedule(Update, strict);
    app.edit_schedule(Last, last_schedule);
    app.finish();
    app.cleanup();
    app.update();
    app
}

fn seen(app: &App) -> &Seen {
    app.world().resource::<Seen>()
}

fn renet_client(app: &App) -> RenetClient {
    RenetClient::new(connection_config(app.world().resource::<RepliconChannels>()))
}

/// Pretend a join is in progress over a transport the test controls.
fn become_joining(app: &mut App, client: RenetClient) {
    let world = app.world_mut();
    world.resource_mut::<NetSession>().set(SessionRole::Client, SessionState::Connecting);
    world.resource_mut::<NetSession>().transport = Some(TransportKind::Ip);
    world.insert_resource(ClientJoin::default());
    world.insert_resource(client);
}

// -- the close grace -------------------------------------------------------------------------

#[derive(Default)]
struct LinkCounts {
    pumps: u32,
    closes: u32,
}

struct FakeLink(Arc<Mutex<LinkCounts>>);

impl ClosingLink for FakeLink {
    fn pump(&mut self, _dt: Duration) {
        if let Ok(mut c) = self.0.lock() {
            c.pumps += 1;
        }
    }
    fn close(&mut self) {
        if let Ok(mut c) = self.0.lock() {
            c.closes += 1;
        }
    }
}

fn fake_link() -> (Box<dyn ClosingLink>, Arc<Mutex<LinkCounts>>) {
    let counts = Arc::new(Mutex::new(LinkCounts::default()));
    (Box::new(FakeLink(counts.clone())), counts)
}

fn counts(c: &Arc<Mutex<LinkCounts>>) -> (u32, u32) {
    let g = c.lock().expect("counts");
    (g.pumps, g.closes)
}

#[test]
fn a_closing_link_is_pumped_for_the_grace_then_closed_once() {
    let mut app = app();
    let (link, c) = fake_link();
    begin_closing(app.world_mut(), link, Duration::from_millis(350));
    for _ in 0..6 {
        app.update();
    }
    println!(">>> close grace: {:?} (pumps, closes) after 300 ms", counts(&c));
    assert_eq!(counts(&c), (6, 0), "pumped every frame, still open");
    assert!(app.world().contains_resource::<ClosingClient>());
    app.update();
    app.update();
    assert_eq!(counts(&c).1, 1, "closed when the grace ran out");
    assert!(!app.world().contains_resource::<ClosingClient>());
    let pumps = counts(&c).0;
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(counts(&c), (pumps, 1), "never pumped or closed again");
}

#[test]
fn a_zero_grace_link_is_closed_on_the_next_frame_without_a_pump() {
    let mut app = app();
    let (link, c) = fake_link();
    begin_closing(app.world_mut(), link, Duration::ZERO);
    app.update();
    assert_eq!(counts(&c), (0, 1));
}

#[test]
fn a_new_request_during_the_grace_closes_the_old_link_at_once() {
    let mut app = app();
    let (link, c) = fake_link();
    begin_closing(app.world_mut(), link, Duration::from_millis(350));
    app.update();
    app.world_mut().write_message(HostSession { transport: HostTransport::Steam { access: SteamAccess::FriendsOnly }, max_clients: 2 });
    app.update();
    assert_eq!(counts(&c).1, 1, "closed by the new request");
    assert!(!app.world().contains_resource::<ClosingClient>());
    for _ in 0..10 {
        app.update();
    }
    assert_eq!(counts(&c).1, 1, "exactly once");
}

#[test]
fn a_second_closing_link_closes_the_first() {
    let mut app = app();
    let (a, ca) = fake_link();
    let (b, cb) = fake_link();
    begin_closing(app.world_mut(), a, Duration::from_millis(350));
    begin_closing(app.world_mut(), b, Duration::from_millis(350));
    assert_eq!((counts(&ca).1, counts(&cb).1), (1, 0));
    for _ in 0..10 {
        app.update();
    }
    assert_eq!((counts(&ca).1, counts(&cb).1), (1, 1));
    assert!(!finish_closing(app.world_mut()), "nothing left");
}

#[test]
fn app_exit_closes_a_link_in_its_grace() {
    let mut app = app();
    let (link, c) = fake_link();
    begin_closing(app.world_mut(), link, Duration::from_secs(5));
    app.update();
    app.world_mut().write_message(AppExit::Success);
    app.update();
    assert_eq!(counts(&c).1, 1);
    assert!(!app.world().contains_resource::<ClosingClient>());
}

#[test]
fn the_grace_is_capped_at_five_seconds() {
    let mut app = app();
    let (link, c) = fake_link();
    begin_closing(app.world_mut(), link, Duration::from_secs(3600));
    for _ in 0..101 {
        app.update();
    }
    assert_eq!(counts(&c).1, 1);
}

// -- the non-send server transport -------------------------------------------------------------

/// Mimics the Steam server transport's trap: it derives `Resource`, but is read as non-send.
#[derive(Resource)]
struct FakeServerTransport(u32);

#[test]
fn the_server_transport_goes_in_and_out_as_non_send_data() {
    let mut world = World::new();
    transport::insert_server_transport(&mut world, FakeServerTransport(7));
    assert!(world.get_resource::<FakeServerTransport>().is_none(), "never as a resource");
    assert_eq!(world.get_non_send::<FakeServerTransport>().map(|t| t.0), Some(7));
    assert_eq!(transport::remove_server_transport::<FakeServerTransport>(&mut world).map(|t| t.0), Some(7));
    assert!(world.get_non_send::<FakeServerTransport>().is_none());
    assert!(transport::remove_server_transport::<FakeServerTransport>(&mut world).is_none(), "idempotent");
}

// -- ids -----------------------------------------------------------------------------------

#[test]
fn generated_client_ids_are_nonzero_and_distinct() {
    let ids: std::collections::HashSet<u64> = (0..1000).map(|_| generate_client_id()).collect();
    assert_eq!(ids.len(), 1000);
    assert!(!ids.contains(&0));
}

// -- the watchdog against a fake renet client -----------------------------------------------

#[test]
fn a_transport_that_drops_before_connecting_could_not_reach_the_host() {
    let mut app = app();
    let mut client = renet_client(&app);
    client.disconnect();
    become_joining(&mut app, client);
    app.update();
    app.update();
    assert_eq!(seen(&app).failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![JoinFailReason::CouldNotReach]);
    assert!(!app.world().contains_resource::<RenetClient>());
    assert_eq!(app.world().resource::<NetSession>().state(), SessionState::Idle);
}

#[test]
fn a_transport_that_drops_after_connecting_lost_the_connection() {
    let mut app = app();
    let mut client = renet_client(&app);
    client.set_connected();
    become_joining(&mut app, client);
    app.update();
    assert!(seen(&app).failed.is_empty(), "connected, waiting for the answer");
    app.world_mut().resource_mut::<RenetClient>().disconnect();
    app.update();
    app.update();
    assert_eq!(seen(&app).failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![JoinFailReason::LostConnection]);
}

#[test]
fn a_join_gets_its_own_timeout_clock() {
    let mut app = app();
    let client = renet_client(&app);
    become_joining(&mut app, client);
    for _ in 0..30 {
        app.update(); // 1.5 s of the 2 s
    }
    assert!(seen(&app).failed.is_empty());
    // A new join restarts the clock.
    app.world_mut().insert_resource(ClientJoin::default());
    for _ in 0..30 {
        app.update();
    }
    assert!(seen(&app).failed.is_empty(), "3 s in, but only 1.5 s into this join");
    for _ in 0..12 {
        app.update();
    }
    assert_eq!(seen(&app).failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![JoinFailReason::Timeout]);
}

#[test]
fn a_joined_client_whose_transport_drops_ends_the_session() {
    let mut app = app();
    let mut client = renet_client(&app);
    client.set_connected();
    become_joining(&mut app, client);
    app.world_mut().resource_mut::<NetSession>().set(SessionRole::Client, SessionState::Joined);
    let from_host = app.world_mut().spawn(Remote).id();
    for _ in 0..(3 * 20) {
        app.update(); // no timeout once joined
    }
    assert!(seen(&app).ended.is_empty() && seen(&app).failed.is_empty());
    app.world_mut().resource_mut::<RenetClient>().disconnect();
    app.update();
    app.update();
    assert_eq!(seen(&app).ended, vec![SessionEnded { reason: SessionEndReason::LostConnection }]);
    assert!(app.world().get_entity(from_host).is_err(), "the host's entities went with it");
}

#[test]
fn a_protocol_mismatch_while_joining_is_a_version_mismatch_once() {
    let mut app = app();
    let mut client = renet_client(&app);
    client.set_connected();
    become_joining(&mut app, client);
    app.update();
    app.world_mut().trigger(ProtocolMismatch);
    if let Some(mut c) = app.world_mut().get_resource_mut::<RenetClient>() {
        c.disconnect();
    }
    for _ in 0..5 {
        app.update();
    }
    let failed = &seen(&app).failed;
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].reason, JoinFailReason::VersionMismatch);
    assert!(failed[0].message.is_ascii());
}

#[test]
fn a_protocol_mismatch_means_nothing_to_a_host_or_an_idle_process() {
    let mut app = app();
    app.world_mut().trigger(ProtocolMismatch);
    app.update();
    app.world_mut().resource_mut::<NetSession>().set(SessionRole::Host, SessionState::Listening);
    app.world_mut().trigger(ProtocolMismatch);
    app.update();
    assert!(seen(&app).failed.is_empty());
    assert!(app.world().resource::<NetSession>().is_host());
}

// -- requests --------------------------------------------------------------------------------

#[test]
fn a_missing_steam_transport_fails_cleanly_and_changes_nothing() {
    // Without the `steam` feature, and with it but without a `SteamNetClient`.
    let mut app = app();
    app.world_mut().write_message(HostSession::steam(4));
    app.update();
    app.world_mut().write_message(JoinSession::steam(76_561_197_960_265_730));
    app.update();
    assert_eq!(seen(&app).host_failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![HostFailReason::TransportUnavailable]);
    assert_eq!(seen(&app).failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![JoinFailReason::TransportUnavailable]);
    assert_eq!(*app.world().resource::<NetSession>(), NetSession { generation: 0, ..default() });
    assert!(!app.world().contains_resource::<RenetServer>() && !app.world().contains_resource::<RenetClient>());
}

#[test]
fn a_leave_with_no_session_writes_nothing() {
    let mut app = app();
    app.world_mut().write_message(LeaveSession);
    for _ in 0..3 {
        app.update();
    }
    assert!(seen(&app).ended.is_empty() && seen(&app).failed.is_empty());
}

#[cfg(feature = "ip")]
#[test]
fn the_last_request_of_a_frame_stands() {
    let mut app = app();
    app.world_mut().write_message(HostSession::ip(0, 2));
    app.world_mut().write_message(LeaveSession);
    app.update();
    // Order of reading: leaves, then hosts, then joins -> the host stands.
    assert_eq!(seen(&app).started.len(), 1);
    assert!(app.world().resource::<NetSession>().is_host());
    assert!(app.world().contains_resource::<RenetServer>());
    app.world_mut().write_message(LeaveSession);
    app.update();
    app.update();
    assert_eq!(seen(&app).ended, vec![SessionEnded { reason: SessionEndReason::Left }]);
    assert!(!app.world().contains_resource::<RenetServer>());
}

#[cfg(feature = "ip")]
#[test]
fn a_replaced_session_starts_its_successor_on_the_next_frame() {
    let mut app = app();
    app.world_mut().write_message(HostSession::ip(0, 2));
    app.update();
    let first = seen(&app).started[0].local_addr;
    app.world_mut().write_message(HostSession::ip(0, 3));
    app.update();
    assert_eq!(seen(&app).ended, vec![SessionEnded { reason: SessionEndReason::Replaced }]);
    assert_eq!(seen(&app).started.len(), 1, "not in the frame the old one closed");
    app.update();
    app.update();
    assert_eq!(seen(&app).started.len(), 2, "started one frame later");
    assert_ne!(seen(&app).started[1].local_addr, None);
    assert_ne!(seen(&app).started[1].local_addr, first, "a new socket");
    assert_eq!(*app.world().resource::<State<ServerState>>().get(), ServerState::Running);
}

#[cfg(feature = "ip")]
#[test]
fn a_client_address_that_cannot_be_connected_to_is_refused() {
    let mut app = app();
    app.world_mut().write_message(JoinSession::ip("0.0.0.0:5000".parse().expect("addr")));
    app.update();
    assert_eq!(seen(&app).failed.iter().map(|f| f.reason.clone()).collect::<Vec<_>>(), vec![JoinFailReason::CouldNotStart]);
    assert_eq!(app.world().resource::<NetSession>().state(), SessionState::Idle);
}

// -- the handshake, host side, without a network ---------------------------------------------

#[test]
fn a_join_request_delivered_locally_is_never_answered() {
    let mut app = app();
    app.world_mut().resource_mut::<NetSession>().set(SessionRole::Host, SessionState::Listening);
    app.world_mut().trigger(FromClient { client_id: ClientId::Server, message: SessionJoinRequest { version: 0, payload: vec![] } });
    app.update();
    let peers = app.world().resource::<Messages<PeerConnected>>().len();
    assert_eq!(peers, 0);
}

/// No unordered pair in `PostUpdate` or `Last` touches this crate's own state (replicon and
/// bevy_renet carry unordered pairs of their own there, so those schedules cannot be strict).
#[test]
fn no_unordered_pair_touches_the_session_state() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin::default()));
    let record = |s: &mut Schedule| {
        s.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Warn, ..default() });
    };
    app.edit_schedule(PostUpdate, record);
    app.edit_schedule(Last, record);
    app.finish();
    app.cleanup();
    app.update();
    let world = app.world();
    let components = world.components();
    let ours: Vec<_> = [
        components.component_id::<NetSession>(),
        components.component_id::<transport::PendingClose>(),
        components.component_id::<ClosingClient>(),
        components.component_id::<ClientJoin>(),
        components.component_id::<requests::DeferredStart>(),
    ]
    .into_iter()
    .flatten()
    .collect();
    assert!(ours.len() >= 4, "the watched resources exist");
    let schedules = world.resource::<Schedules>();
    for label in [PostUpdate.intern(), Last.intern()] {
        let schedule = schedules.get(label).expect("schedule");
        let conflicts = &schedule.graph().conflicting_systems().0;
        println!(">>> {label:?}: {} unordered conflicting pairs, none on our state", conflicts.len());
        for (_, _, on) in conflicts {
            assert!(!on.iter().any(|id| ours.contains(id)), "an unordered {label:?} pair on the session state");
        }
    }
}

#[test]
fn the_protocol_hash_covers_the_protocol_version() {
    fn hash(version: u64) -> Option<ProtocolHash> {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin { protocol_version: version, ..default() }));
        app.finish();
        app.cleanup();
        protocol_hash(&app)
    }
    assert!(hash(1).is_some());
    assert_eq!(hash(1), hash(1), "deterministic");
    assert_ne!(hash(1), hash(2), "another version is another protocol");
}

#[cfg(feature = "steam")]
#[test]
fn steam_access_maps_to_renet_steam() {
    use bevy_replicon_renet::steam::AccessPermission;
    assert!(matches!(steam::access_permission(&SteamAccess::FriendsOnly), AccessPermission::FriendsOnly));
    assert!(matches!(steam::access_permission(&SteamAccess::Public), AccessPermission::Public));
    assert!(matches!(steam::access_permission(&SteamAccess::Private), AccessPermission::Private));
    match steam::access_permission(&SteamAccess::InLobby(42)) {
        AccessPermission::InLobby(lobby) => assert_eq!(lobby.raw(), 42),
        _ => panic!("InLobby"),
    }
    match steam::access_permission(&SteamAccess::InList(vec![76_561_197_960_265_730, 76_561_197_960_265_730])) {
        AccessPermission::InList(ids) => assert_eq!(ids.len(), 1),
        _ => panic!("InList"),
    }
}
