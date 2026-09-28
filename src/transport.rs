//! Opening and closing transports: UDP (feature `ip`) and Steam (feature `steam`), the session
//! close (with its deferred flush), the Steam client's close grace and the exit close.
//!
//! **One server transport per host.** netcode and Steam cannot share one `RenetServer`: both
//! transports' `send_packets` drain the packets of EVERY client id, so each would eat the other's.
//!
//! **The Steam server transport is non-send data**, not a resource: bevy_renet's Steam server
//! systems read `Option<NonSendMut<SteamServerTransport>>`. The type also derives `Resource`, so
//! `insert_resource` compiles and every Steam system then silently sees `None`. It is inserted
//! with `World::insert_non_send` and removed with `World::remove_non_send`, from a command (the
//! main thread, exclusive world access). The Steam CLIENT transport is a normal resource.

use std::net::SocketAddr;
use std::time::Duration;

use bevy_ecs::prelude::*;
use bevy_replicon::prelude::*;
use bevy_replicon_renet::renet::ConnectionConfig;
use bevy_replicon_renet::{RenetChannelsExt, RenetClient, RenetServer};
use bevy_time::{Real, Time};
use tracing::{info, warn};

use crate::handshake::ClientJoin;
use crate::messages::{JoinFailReason, JoinFailed, SessionEndReason, SessionEnded, TransportKind};
use crate::session::{NetSession, SessionState};
#[cfg(any(feature = "ip", feature = "steam"))]
use crate::NetSessionConfig;

/// renet's connection config from replicon's channels. Read channels only after every
/// replicated type is registered (i.e. in systems, never in a plugin's `build`).
#[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(dead_code))]
pub(crate) fn connection_config(channels: &RepliconChannels) -> ConnectionConfig {
    ConnectionConfig { server_channels_config: channels.server_configs(), client_channels_config: channels.client_configs(), ..Default::default() }
}

// ---------------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------------

/// What a started host reports.
pub(crate) struct HostStarted {
    pub(crate) transport: TransportKind,
    pub(crate) local_addr: Option<SocketAddr>,
    pub(crate) steam_id: Option<u64>,
}

/// Bind a UDP listen transport and queue its insertion. `Err` = a human-readable reason.
#[cfg(feature = "ip")]
pub(crate) fn start_ip_server(
    commands: &mut Commands,
    channels: &RepliconChannels,
    config: &NetSessionConfig,
    port: u16,
    max_clients: usize,
) -> Result<SocketAddr, String> {
    use bevy_replicon_renet::netcode::{NetcodeServerTransport, ServerAuthentication, ServerConfig};
    use std::net::{Ipv4Addr, UdpSocket};

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| format!("could not bind UDP port {port}: {e}"))?;
    let local = socket.local_addr().map_err(|e| format!("could not read the bound address: {e}"))?;
    let server_config = ServerConfig {
        current_time: unix_now()?,
        max_clients: max_clients.clamp(1, crate::MAX_IP_CLIENTS),
        protocol_id: config.netcode_protocol_id,
        authentication: ServerAuthentication::Unsecure,
        public_addresses: vec![local],
    };
    let transport = NetcodeServerTransport::new(server_config, socket).map_err(|e| format!("could not start the UDP transport: {e}"))?;
    commands.insert_resource(RenetServer::new(connection_config(channels)));
    commands.insert_resource(transport);
    Ok(local)
}

/// Open a UDP client transport to `addr` and queue its insertion. Returns the client id.
#[cfg(feature = "ip")]
pub(crate) fn start_ip_client(commands: &mut Commands, channels: &RepliconChannels, config: &NetSessionConfig, addr: SocketAddr) -> Result<u64, String> {
    use bevy_replicon_renet::netcode::{ClientAuthentication, NetcodeClientTransport};
    use std::net::{Ipv4Addr, Ipv6Addr, UdpSocket};

    if addr.ip().is_unspecified() || addr.port() == 0 {
        return Err(format!("{addr} is not an address a client can connect to"));
    }
    let bind: SocketAddr = match addr {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(bind).map_err(|e| format!("could not open a UDP socket: {e}"))?;
    let client_id = generate_client_id();
    let authentication = ClientAuthentication::Unsecure { client_id, protocol_id: config.netcode_protocol_id, server_addr: addr, user_data: None };
    let transport = NetcodeClientTransport::new(unix_now()?, authentication, socket).map_err(|e| format!("could not start the UDP transport: {e}"))?;
    commands.insert_resource(RenetClient::new(connection_config(channels)));
    commands.insert_resource(transport);
    Ok(client_id)
}

#[cfg(feature = "ip")]
fn unix_now() -> Result<Duration, String> {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| format!("the system clock is before 1970: {e}"))
}

/// A process-unique, practically collision-free netcode client id (never 0). Not cryptographic;
/// the Unsecure netcode mode it is used with authenticates nothing anyway.
#[cfg_attr(not(feature = "ip"), allow(dead_code))]
pub(crate) fn generate_client_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut x = nanos ^ (u64::from(std::process::id()) << 32) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // splitmix64 finaliser
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x.max(1)
}

/// Open a Steam listen transport and queue its insertion (the transport as NON-SEND data).
/// Returns the host's SteamID64.
#[cfg(feature = "steam")]
pub(crate) fn start_steam_server(
    commands: &mut Commands,
    channels: &RepliconChannels,
    steam: &steamworks::Client,
    access: &crate::SteamAccess,
    max_clients: usize,
) -> Result<u64, String> {
    use bevy_replicon_renet::steam::{SteamServerConfig, SteamServerTransport};
    let config = SteamServerConfig { max_clients: max_clients.max(1), access_permission: crate::steam::access_permission(access) };
    let transport = SteamServerTransport::new(steam.clone(), config).map_err(|e| format!("could not listen on Steam: {e:?}"))?;
    commands.insert_resource(RenetServer::new(connection_config(channels)));
    commands.queue(move |world: &mut World| insert_server_transport(world, transport));
    Ok(steam.user().steam_id().raw())
}

/// Open a Steam client transport to the host's SteamID64 and queue its insertion. Returns this
/// client's SteamID64 (renet's client id over Steam).
#[cfg(feature = "steam")]
pub(crate) fn start_steam_client(commands: &mut Commands, channels: &RepliconChannels, steam: &steamworks::Client, host: u64) -> Result<u64, String> {
    use bevy_replicon_renet::steam::SteamClientTransport;
    if host == 0 {
        return Err("0 is not a SteamID64".to_string());
    }
    let transport =
        SteamClientTransport::new(steam.clone(), &steamworks::SteamId::from_raw(host)).map_err(|e| format!("could not connect to Steam host {host}: {e:?}"))?;
    commands.insert_resource(RenetClient::new(connection_config(channels)));
    commands.insert_resource(transport);
    Ok(steam.user().steam_id().raw())
}

/// Insert a server transport the way bevy_renet reads it: as non-send data.
#[cfg_attr(not(feature = "steam"), allow(dead_code))]
pub(crate) fn insert_server_transport<T: 'static>(world: &mut World, transport: T) {
    world.insert_non_send(transport);
}

/// Remove a server transport inserted by [`insert_server_transport`].
#[cfg_attr(not(feature = "steam"), allow(dead_code))]
pub(crate) fn remove_server_transport<T: 'static>(world: &mut World) -> Option<T> {
    world.remove_non_send::<T>()
}

// ---------------------------------------------------------------------------------------------
// Closing
// ---------------------------------------------------------------------------------------------

/// How a session's transports are closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Close {
    /// A leave: a connected Steam client keeps pumping for the close grace so its last messages
    /// go out (renet_steam closes a client without linger).
    Linger,
    /// A new session, a failure or the app exiting: close everything at once.
    Now,
}

/// A [`LeaveSession`](crate::LeaveSession) waits here for `PostUpdate` (after `RenetSend`).
#[derive(Resource, Default, Debug)]
pub(crate) struct PendingClose(pub(crate) Option<SessionEndReason>);

/// Close every transport of this process. `true` = something was open.
///
/// Host: remove `RenetServer` and its transport and despawn every connected-client entity. The
/// clients are disconnected at once, or, on a leave ([`Close::Linger`]) with clients connected,
/// after the close grace (a UDP host keeps pumping them so the last messages are delivered and
/// resent if lost; a Steam host closes with Steam's own linger). Client: remove `RenetClient` and
/// its transport (a connected client leaving keeps pumping its link for the grace) and despawn
/// every replicon `Remote` entity, which replicon's own client reset does not do.
///
/// A link in its grace is out of the session: replicon sees no renet resource any more, so it is
/// disconnected and applies nothing that still arrives.
pub(crate) fn close_transports(world: &mut World, how: Close) -> bool {
    let was_client = world.get_resource::<NetSession>().is_some_and(|s| s.is_client());
    #[cfg(any(feature = "ip", feature = "steam"))]
    let grace = match how {
        Close::Linger => world.get_resource::<NetSessionConfig>().map_or(Duration::ZERO, |c| c.client_close_grace),
        Close::Now => Duration::ZERO,
    };
    #[cfg(not(any(feature = "ip", feature = "steam")))]
    let _ = how;
    let mut closed = false;

    #[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(unused_mut))]
    let mut server = world.remove_resource::<RenetServer>();
    let had_server = server.is_some();
    closed |= had_server;
    #[cfg(feature = "ip")]
    if let Some(mut transport) = world.remove_resource::<bevy_replicon_renet::netcode::NetcodeServerTransport>() {
        closed = true;
        match server.take() {
            Some(renet) if !grace.is_zero() && renet.connected_clients() > 0 => {
                begin_closing(world, Box::new(NetcodeServerLinger { server: renet, transport }), grace);
            }
            Some(mut renet) => transport.disconnect_all(&mut renet),
            None => {}
        }
    }
    #[cfg(feature = "steam")]
    if let Some(mut transport) = remove_server_transport::<bevy_replicon_renet::steam::SteamServerTransport>(world) {
        if let Some(server) = server.as_mut() {
            // `true` = linger: Steam keeps each connection until what is queued on it is flushed.
            transport.disconnect_all(server, true);
        }
        closed = true;
    }
    if had_server {
        let clients: Vec<Entity> = world.query_filtered::<Entity, With<ConnectedClient>>().iter(world).collect();
        for entity in clients {
            if let Ok(entity) = world.get_entity_mut(entity) {
                entity.despawn();
            }
        }
    }
    drop(server);

    #[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(unused_mut))]
    let mut client = world.remove_resource::<RenetClient>();
    #[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(unused_mut))]
    let mut client_closed = client.is_some();
    #[cfg(feature = "ip")]
    if let Some(mut transport) = world.remove_resource::<bevy_replicon_renet::netcode::NetcodeClientTransport>() {
        client_closed = true;
        match client.take() {
            // netcode sends its disconnect packets the moment it is told to, and the host drops
            // whatever arrives in the same batch: keep the link for the grace instead.
            Some(renet) if !grace.is_zero() && renet.is_connected() => {
                begin_closing(world, Box::new(NetcodeClientLinger { client: renet, transport }), grace);
            }
            other => {
                if let Some(mut renet) = other {
                    renet.disconnect();
                }
                transport.disconnect();
            }
        }
    }
    #[cfg(feature = "steam")]
    if let Some(mut transport) = world.remove_resource::<bevy_replicon_renet::steam::SteamClientTransport>() {
        client_closed = true;
        match client.take() {
            // renet_steam closes a client WITHOUT linger: keep the link for the grace.
            Some(renet) if !grace.is_zero() && renet.is_connected() => {
                let link = crate::steam::SteamClientLinger { client: renet, transport };
                begin_closing(world, Box::new(link), grace);
            }
            other => {
                if let Some(mut renet) = other {
                    renet.disconnect();
                }
                transport.disconnect();
            }
        }
    }
    if let Some(mut client) = client {
        client.disconnect();
    }
    closed |= client_closed;

    if client_closed || was_client {
        let n = despawn_remote_entities(world);
        if n > 0 {
            info!("net session: dropped {n} replicated entities of the left session");
            closed = true;
        }
    }
    closed
}

/// Despawn every entity replicon spawned for this client (`Remote`). Descendants go with their
/// parent; an entity already gone that way is skipped.
fn despawn_remote_entities(world: &mut World) -> usize {
    let remote: Vec<Entity> = world.query_filtered::<Entity, With<Remote>>().iter(world).collect();
    let mut n = 0;
    for entity in remote {
        if let Ok(entity) = world.get_entity_mut(entity) {
            entity.despawn();
            n += 1;
        }
    }
    n
}

/// Reset the session bookkeeping to idle and return what the session was.
fn reset_session(world: &mut World) -> NetSession {
    let before = world.get_resource::<NetSession>().cloned().unwrap_or_default();
    if let Some(mut session) = world.get_resource_mut::<NetSession>() {
        session.reset();
    }
    if let Some(mut join) = world.get_resource_mut::<ClientJoin>() {
        *join = ClientJoin::default();
    }
    if let Some(mut pending) = world.get_resource_mut::<PendingClose>() {
        pending.0 = None;
    }
    before
}

/// End the session for `reason`: close the transports, reset the bookkeeping, and tell the game
/// ([`SessionEnded`] for a started session, [`JoinFailed`] `Cancelled` for a join still pending).
/// Idempotent: an idle process writes nothing.
pub(crate) fn end_session(world: &mut World, how: Close, reason: SessionEndReason) {
    let closed = close_transports(world, how);
    let before = reset_session(world);
    match before.state {
        SessionState::Listening | SessionState::Joined => {
            info!("net session: ended ({reason:?})");
            world.write_message(SessionEnded { reason });
        }
        SessionState::Connecting => {
            info!("net session: join cancelled ({reason:?})");
            world.write_message(JoinFailed { reason: JoinFailReason::Cancelled, message: "the join was abandoned before the host answered".to_string() });
        }
        SessionState::Idle => {
            if closed {
                info!("net session: transports closed");
            }
        }
    }
}

/// A join failed: tell the game, close everything at once. Only while a join is pending (a late
/// second failure of the same join is ignored).
pub(crate) fn fail_join(world: &mut World, reason: JoinFailReason, message: String) {
    let joining = world.get_resource::<NetSession>().is_some_and(|s| s.is_client() && s.state() == SessionState::Connecting);
    if !joining {
        return;
    }
    warn!("net session: join failed ({reason:?}): {message}");
    close_transports(world, Close::Now);
    reset_session(world);
    world.write_message(JoinFailed { reason, message });
}

/// `PostUpdate`, after `RenetSend`: run the close a [`LeaveSession`](crate::LeaveSession) asked for.
pub(crate) fn flush_pending_close(mut pending: ResMut<PendingClose>, mut commands: Commands) {
    if let Some(reason) = pending.0.take() {
        commands.queue(move |world: &mut World| end_session(world, Close::Linger, reason));
    }
}

/// `Last`, on the frame an `AppExit` is written: close everything now, so peers see a disconnect
/// instead of a timeout. A link still in its close grace is closed too (nothing would pump it).
pub(crate) fn close_on_exit(mut commands: Commands) {
    commands.queue(|world: &mut World| {
        finish_closing(world);
        if let Some(mut pending) = world.get_resource_mut::<crate::requests::DeferredStart>() {
            pending.0 = None;
        }
        end_session(world, Close::Now, SessionEndReason::AppExit);
    });
}

// ---------------------------------------------------------------------------------------------
// The close grace (a leave flushing its last packets)
// ---------------------------------------------------------------------------------------------

/// A connection kept open after its session closed (a UDP client or host, a Steam client). A
/// trait so the grace is testable without a network.
pub(crate) trait ClosingLink: Send + Sync + 'static {
    /// One frame of the grace: advance the connection by `dt`, take in acks, send what is due.
    fn pump(&mut self, dt: Duration);
    /// The grace is over (or something needs the slot): close for good.
    fn close(&mut self);
}

/// A client connection in its close grace: already out of the session (no `RenetClient`, so
/// replicon is disconnected and applies nothing it receives), pumped until the grace runs out,
/// then closed. At most one exists.
#[derive(Resource)]
pub(crate) struct ClosingClient {
    link: Box<dyn ClosingLink>,
    remaining: Duration,
}

/// Start the close grace for `link` (closing any link already in its grace first).
#[cfg_attr(not(any(feature = "ip", feature = "steam")), allow(dead_code))]
pub(crate) fn begin_closing(world: &mut World, link: Box<dyn ClosingLink>, grace: Duration) {
    finish_closing(world);
    let remaining = grace.min(Duration::from_secs(5));
    info!("net session: client link closing in {} ms (flushing the last packets)", remaining.as_millis());
    world.insert_resource(ClosingClient { link, remaining });
}

/// Close a link still in its grace right now. `true` = there was one.
pub(crate) fn finish_closing(world: &mut World) -> bool {
    match world.remove_resource::<ClosingClient>() {
        Some(mut closing) => {
            closing.link.close();
            true
        }
        None => false,
    }
}

/// `PostUpdate`, after the pending close: pump the link in its grace; close it when the grace is
/// over. Wall-clock time.
pub(crate) fn drive_closing_client(time: Res<Time<Real>>, closing: Option<ResMut<ClosingClient>>, mut commands: Commands) {
    let Some(mut closing) = closing else {
        return;
    };
    let dt = time.delta();
    if closing.remaining <= dt {
        closing.remaining = Duration::ZERO;
        commands.queue(|world: &mut World| {
            if finish_closing(world) {
                info!("net session: client link closed");
            }
        });
    } else {
        closing.remaining -= dt;
        closing.link.pump(dt);
    }
}

/// A UDP client in its close grace: what bevy_renet's netcode client plugin runs each frame
/// (renet update, transport update, send), on a link it no longer sees. Closing sends netcode's
/// disconnect packets.
#[cfg(feature = "ip")]
struct NetcodeClientLinger {
    client: RenetClient,
    transport: bevy_replicon_renet::netcode::NetcodeClientTransport,
}

#[cfg(feature = "ip")]
impl ClosingLink for NetcodeClientLinger {
    fn pump(&mut self, dt: Duration) {
        self.client.update(dt);
        if let Err(e) = self.transport.update(dt, &mut self.client) {
            warn!("net session: closing UDP link: receive failed ({e})");
        }
        if !self.client.is_disconnected() {
            if let Err(e) = self.transport.send_packets(&mut self.client) {
                warn!("net session: closing UDP link: send failed ({e})");
            }
        }
    }

    fn close(&mut self) {
        self.client.disconnect();
        self.transport.disconnect();
    }
}

/// A UDP host in its close grace: its clients keep being served (acks, resends) until the grace
/// is over, then netcode's disconnect packets go out to all of them.
#[cfg(feature = "ip")]
struct NetcodeServerLinger {
    server: RenetServer,
    transport: bevy_replicon_renet::netcode::NetcodeServerTransport,
}

#[cfg(feature = "ip")]
impl ClosingLink for NetcodeServerLinger {
    fn pump(&mut self, dt: Duration) {
        self.server.update(dt);
        if let Err(e) = self.transport.update(dt, &mut self.server) {
            warn!("net session: closing UDP host: receive failed ({e})");
        }
        // Nothing reads the events of a closed host; do not let them pile up.
        while self.server.get_event().is_some() {}
        self.transport.send_packets(&mut self.server);
    }

    fn close(&mut self) {
        self.transport.disconnect_all(&mut self.server);
    }
}
