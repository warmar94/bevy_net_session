# bevy_net_session

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/warmar94/bevy_net_session/actions/workflows/ci.yml/badge.svg)](https://github.com/warmar94/bevy_net_session/actions/workflows/ci.yml)
[![Bevy 0.19.0](https://img.shields.io/badge/Bevy-0.19.0-informational)](https://bevyengine.org)
[![bevy_replicon 0.44.2](https://img.shields.io/badge/bevy__replicon-0.44.2-orange)](https://crates.io/crates/bevy_replicon)
[![bevy_renet 5.0](https://img.shields.io/badge/bevy__renet-5.0-informational)](https://crates.io/crates/bevy_renet)
[![steamworks 0.12.2 (optional)](https://img.shields.io/badge/steamworks-0.12.2%20(optional)-informational)](https://crates.io/crates/steamworks)

Game-agnostic **multiplayer sessions for [Bevy](https://bevyengine.org)** on
[`bevy_replicon`](https://crates.io/crates/bevy_replicon) and renet: host, join and leave, the
same way over **UDP** or **Steam P2P**, with a version-checked join handshake, a hook for your
game to accept or refuse each joiner, join timeouts with a reason, and a teardown that delivers
the last messages before it disconnects.

It is the **connection layer**, not a framework: your game sends requests (`HostSession`,
`JoinSession`, `LeaveSession`) and reacts to facts (`SessionStarted`, `JoinAccepted`,
`JoinFailed`, `PeerConnected`, `SessionEnded`, ...). What a session *means* (which world, which
character, which rules) is your game's own replicated data, sent once the join is accepted.
Finding a host (lobbies, invites, a server browser) is not in scope either; see
[Combining it with Steam lobbies](#9-combining-it-with-steam-lobbies).

## Contents

- [Highlights](#highlights)
- [Quick start](#quick-start)
- [How to use it](#how-to-use-it)
  - [1. Add the plugin](#1-add-the-plugin)
  - [2. Host](#2-host)
  - [3. Join](#3-join)
  - [4. Accept or refuse joiners](#4-accept-or-refuse-joiners)
  - [5. After the join: your game's data](#5-after-the-join-your-games-data)
  - [6. Leave](#6-leave)
  - [7. Errors](#7-errors)
  - [8. Steam](#8-steam)
  - [9. Combining it with Steam lobbies](#9-combining-it-with-steam-lobbies)
  - [10. Protocol version and registration order](#10-protocol-version-and-registration-order)
  - [11. System order](#11-system-order)
  - [12. Testing your game](#12-testing-your-game)
- [How it works](#how-it-works)
- [API reference](#api-reference)
- [Cargo features](#cargo-features)
- [Compatibility](#compatibility)
- [Examples](#examples)
- [Testing with real Steam](#testing-with-real-steam)
- [Limitations and FAQ](#limitations-and-faq)
- [License](#license)
- [Contributing](#contributing)

## Highlights

- **One plugin, ECS-shaped API**: three request messages in, seven fact messages out, one
  read-only resource (`NetSession`) and public `SystemSet`s to order your systems against.
- **Two transports, one API**: `HostTransport::Ip { port }` / `HostTransport::Steam { access }`
  and `JoinTarget::Ip(addr)` / `JoinTarget::Steam(host_id)`, each behind a cargo feature.
- **A real join handshake**: the client presents your protocol version and an opaque payload
  (a password, a character, a token: your choice); the host checks the version first, then asks
  your `JoinValidator`. A refusal reaches the client with its reason.
- **Every join gets exactly one answer**: `JoinAccepted` or `JoinFailed` with a reason
  (`CouldNotReach`, `Timeout`, `LostConnection`, `VersionMismatch`, `Rejected(..)`,
  `TransportUnavailable`, `CouldNotStart`, `Cancelled`). Every started session gets exactly one
  `SessionEnded`.
- **Versioned protocol**: the version is also fed into replicon's protocol hash, so a peer on
  another build is refused on every transport, including Steam, which has no protocol id of its
  own. `protocol_hash(&app)` lets you assert in a test that your client and server stacks match.
- **A teardown that does not lose the last word**: a leave runs after the frame's packets were
  sent, and the connection keeps being pumped for a short grace before it disconnects, so a
  "save my character" sent on the leave frame arrives. A client's replicated entities are
  despawned; single-player works again right after.
- **The Steam traps handled**: the Steam server transport is inserted as non-send data (a
  resource insert compiles and silently does nothing), one server transport per host, a Steam
  client that cannot linger gets a flush grace.
- **Fail closed, never panic**: a busy port, a missing Steam client, an unreachable host are
  facts with reasons, never a crash.
- **Tested headless**: real UDP host and client in one process, strict ambiguity detection.

## Quick start

Add the crate. From crates.io (once published):

```toml
[dependencies]
bevy = "0.19.0"
bevy_net_session = "0.1.1"
```

or from the repository, pinned to a release tag:

```toml
[dependencies]
bevy = "0.19.0"
bevy_net_session = { git = "https://github.com/warmar94/bevy_net_session", tag = "v0.1.1" }
```

A game that hosts when started with `host` and joins otherwise:

```rust,no_run
use bevy::prelude::*;
use bevy_net_session::*;

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, NetSessionPlugin { protocol_version: 1, ..default() }))
        .add_systems(Startup, start)
        .add_systems(Update, react.after(NetSessionSystems::Watch))
        .run();
}

fn start(mut host: MessageWriter<HostSession>, mut join: MessageWriter<JoinSession>) {
    if std::env::args().any(|a| a == "host") {
        host.write(HostSession::ip(5000, 8));
    } else {
        join.write(JoinSession::ip("127.0.0.1:5000".parse().unwrap()));
    }
}

fn react(mut joined: MessageReader<JoinAccepted>, mut failed: MessageReader<JoinFailed>, mut peers: MessageReader<PeerConnected>) {
    for ev in joined.read() {
        info!("joined as {}", ev.local_id);
    }
    for ev in failed.read() {
        warn!("could not join: {}", ev.message);
    }
    for ev in peers.read() {
        info!("client {} joined", ev.id);
    }
}
```

`cargo run --example quick_start` runs host and client in one process over loopback: a refused
password, an accepted join and a leave, all printed.

## How to use it

Everything below uses `use bevy::prelude::*; use bevy_net_session::*;`.

### 1. Add the plugin

```rust
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;
use std::time::Duration;

let mut app = App::new();
app.add_plugins((
    MinimalPlugins,
    StatesPlugin, // part of DefaultPlugins; replicon needs it
    NetSessionPlugin {
        protocol_version: 3,
        join_timeout: Duration::from_secs(15),
        ..default()
    },
));
```

| field | default | meaning |
|---|---|---|
| `protocol_version` | `0` | your wire-protocol version. Checked in the join handshake and added to replicon's protocol hash. Bump it whenever what crosses the wire changes |
| `join_timeout` | 15 s | a join that is not decided by then fails with `Timeout`. Each join has its own clock (wall-clock time) |
| `client_close_grace` | 350 ms | how long a connection keeps being pumped after a leave so the last messages are delivered (and resent once if lost) before the disconnect. Capped at 5 s; zero closes at once |
| `netcode_protocol_id` | `DEFAULT_NETCODE_PROTOCOL_ID` | the UDP transport's protocol id. Peers with different ids never connect (`CouldNotReach` / `Timeout`); keep the default so a version difference is reported as `VersionMismatch` |

- The plugin adds `RepliconPlugins` and `RepliconRenetPlugins` when they are missing. To
  configure them (e.g. replicon's `AuthMethod`), add them **before** `NetSessionPlugin`; adding
  them after it panics (Bevy refuses duplicate plugins).
- `StatesPlugin` is required, as for replicon itself. `DefaultPlugins` has it.
- The config is copied into the `NetSessionConfig` resource; treat it as read-only.
- Re-exports: `bevy_net_session::bevy_replicon` and `bevy_net_session::bevy_replicon_renet`, so
  you can use exactly the versions this crate is built on.

### 2. Host

```rust
use bevy::prelude::*;
use bevy_net_session::*;

fn host(mut requests: MessageWriter<HostSession>) {
    requests.write(HostSession::ip(5000, 8)); // UDP port 5000, up to 8 clients
    // or: HostSession::steam(4), or HostSession { transport: HostTransport::Steam { access: SteamAccess::InLobby(lobby_id) }, max_clients: 4 }
}

fn on_hosting(mut started: MessageReader<SessionStarted>, mut failed: MessageReader<HostFailed>) {
    for ev in started.read() {
        info!("listening: {:?} / Steam id {:?}", ev.local_addr, ev.steam_id);
    }
    for ev in failed.read() {
        error!("could not host: {:?} {}", ev.reason, ev.message);
    }
}
```

- `HostTransport::Ip { port }` binds `0.0.0.0:port`. Port `0` lets the OS pick one; the real
  address is in `SessionStarted::local_addr` and `NetSession::local_addr()`.
- `max_clients` is at least 1; UDP caps it at `MAX_IP_CLIENTS` (1024).
- **One transport per host**: a host listens on UDP **or** Steam. (renet's two server
  transports cannot share one server: each would consume the other's packets.)
- A failure (`HostFailReason::CouldNotStart` for a busy port, `TransportUnavailable` for a
  transport not compiled in or no Steam) leaves nothing open.
- Connected clients show up as `PeerConnected { entity, id }` once they pass the handshake, and
  as `PeerDisconnected` when they go. The entity is replicon's connected-client entity and carries
  `SessionPeer { id }`: `Query<&SessionPeer>` lists everyone in the session.

### 3. Join

```rust
use bevy::prelude::*;
use bevy_net_session::*;

fn join(mut requests: MessageWriter<JoinSession>) {
    let addr = "192.168.1.20:5000".parse().unwrap();
    requests.write(JoinSession::ip(addr).with_payload("hunter2"));
    // or: JoinSession::steam(host_steam_id)
}

fn on_join(mut accepted: MessageReader<JoinAccepted>, mut failed: MessageReader<JoinFailed>) {
    for ev in accepted.read() {
        info!("in! my id on the host: {}", ev.local_id);
    }
    for ev in failed.read() {
        warn!("join failed: {:?} ({})", ev.reason, ev.message);
    }
}
```

Every `JoinSession` gets **exactly one** answer: `JoinAccepted` or `JoinFailed`. While the join
is in progress `NetSession::state()` is `Connecting`, then `Joined`.

### 4. Accept or refuse joiners

Install a `JoinValidatorRes` on the host. It runs after the protocol version matched and sees the
client's entity, id, version and **payload**, bytes the client chose in `JoinSession::payload`.
What they mean is up to you: a password, a serialized character, an auth ticket.

```rust
use bevy::prelude::*;
use bevy_net_session::*;

fn setup(mut commands: Commands) {
    let banned = vec![76561197960265730];
    commands.insert_resource(JoinValidatorRes::new(move |req: &JoinRequestInfo| {
        if banned.contains(&req.id) {
            return Err("you are banned".to_string());
        }
        if req.payload != b"hunter2" {
            return Err("wrong password".to_string());
        }
        Ok(())
    }));
}
```

- `Err(reason)` refuses: the client receives `JoinFailed { reason: Rejected(reason), .. }` and
  is then disconnected (after the answer was sent). Keep reasons short and ASCII.
- No validator = everyone with the right version is accepted.
- The validator is a closure or any type implementing `JoinValidator`. Replace or remove the
  resource at any time (e.g. to change the password or lock the session).
- It runs synchronously inside an observer. For a check that needs other ECS state, keep that
  state inside the validator (e.g. an `Arc<Mutex<..>>` shared with your systems) or accept here
  and kick later with replicon's `DisconnectRequest`.

### 5. After the join: your game's data

The crate stops at "this client is in". Send your world info with your own replicated message,
from the host, when a peer connects:

```rust
use bevy::prelude::*;
use bevy_net_session::*;
use bevy_net_session::bevy_replicon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Event, Serialize, Deserialize, Clone)]
struct WorldInfo {
    seed: u64,
}

fn register(app: &mut App) {
    // In your shared protocol registration (the same on every peer, see section 10).
    app.add_server_event::<WorldInfo>(Channel::Ordered);
}

fn welcome(mut peers: MessageReader<PeerConnected>, mut commands: Commands) {
    for peer in peers.read() {
        commands.server_trigger(ToClients {
            targets: SendTargets::Single(ClientId::Client(peer.entity)),
            message: WorldInfo { seed: 42 },
        });
    }
}
```

Under replicon's default `AuthMethod::ProtocolCheck` a client is already authorized for
replication when `PeerConnected` is written, so normal (non-independent) events reach it.

### 6. Leave

```rust
use bevy::prelude::*;
use bevy_net_session::*;

fn quit_to_menu(mut leave: MessageWriter<LeaveSession>) {
    leave.write(LeaveSession);
}
```

- The close runs in `PostUpdate`, **after** renet sent this frame's packets, so whatever your
  game sent in the same frame (a final save request, a "player left" notice) goes out first.
- The connection is then kept for `client_close_grace` (default 350 ms): pumped, but out of the
  session, so the last messages are acknowledged or resent before the disconnect. (Without it a
  UDP peer that receives "message + disconnect" in one batch drops the message.) A Steam host
  uses Steam's own linger instead.
- The session itself ends at once: `SessionEnded { reason: Left }`, `NetSession` is idle, a
  client's replicated entities (replicon's `Remote`) are despawned, replicon is disconnected and
  single-player delivery of client events works again. A host's `PeerDisconnected`s are written.
- A leave while a join is still in progress ends it with `JoinFailed { reason: Cancelled }`.
- **A new request replaces the session**: a `HostSession` / `JoinSession` while a session is
  open closes the old one at once (`SessionEnded { Replaced }` or `JoinFailed { Cancelled }`) and
  starts the new one on the next frame (replicon's state must see the old session go first).
- **Quitting** (`AppExit`) closes every connection on the exit frame so peers see a disconnect
  instead of a timeout. Messages written in `Last` of that frame are not sent any more (renet
  sends in `PostUpdate`); save before, or leave first.

### 7. Errors

`JoinFailed { reason, message }` (client). `message` is a readable ASCII sentence for a toast or
log; match on `reason`:

| `JoinFailReason` | when |
|---|---|
| `CouldNotReach` | the transport dropped before it ever connected (nobody there, refused, unreachable, a Steam host that is not your friend under `FriendsOnly`) |
| `Timeout` | no decision within `join_timeout` |
| `LostConnection` | connected, then dropped before the host answered |
| `VersionMismatch` | the host runs another `protocol_version`, or registers a different replicated protocol |
| `Rejected(reason)` | the host's validator refused, with its reason |
| `TransportUnavailable` | the transport's feature is not compiled in, or no `SteamNetClient` |
| `CouldNotStart` | the local transport would not start (socket error, an address like `0.0.0.0`) |
| `Cancelled` | a `LeaveSession`, a new request or the app exiting abandoned the join |

`HostFailed { reason, message }` (host): `TransportUnavailable` or `CouldNotStart`.

`SessionEnded { reason }`: `Left`, `Replaced`, `LostConnection` (client: the host went away,
kicked you, or the network dropped), `AppExit`.

All three reason enums are `#[non_exhaustive]`.

### 8. Steam

Enable the feature and add `steamworks` itself, at exactly the version this crate uses:

```toml
[dependencies]
bevy_net_session = { version = "0.1.1", features = ["steam"] }
steamworks = "=0.12.2"
```

Initialise Steam **yourself** and hand the client to the plugin:

```rust,no_run
use bevy::prelude::*;
use bevy_net_session::*;

fn main() {
    let mut app = App::new();
    app.add_plugins((DefaultPlugins, NetSessionPlugin::default()));
    // 480 is Valve's public test app ("Spacewar"); use your own app id in a shipped game.
    match steamworks::Client::init_app(480) {
        Ok(client) => {
            client.networking_utils().init_relay_network_access();
            app.insert_resource(SteamNetClient(client));
            // Exactly ONE system pumps Steam callbacks (see below). If nothing else does:
            app.add_systems(First, |steam: Res<SteamNetClient>| steam.0.run_callbacks());
        }
        Err(e) => warn!("Steam is not available: {e}"),
    }
    app.run();
}
```

- **Host**: `HostSession::steam(max_clients)` listens on Steam P2P as the host's SteamID64
  (`SessionStarted::steam_id`). Who may connect is `SteamAccess`: `FriendsOnly` (default, the
  host's direct friends), `Public`, `Private`, `InLobby(lobby_id)` (members of that lobby only),
  `InList(ids)`.
- **Join**: `JoinSession::steam(host_steam_id)`. Over Steam a client's id is its SteamID64.
- **The one-pump rule**: renet_steam's connections are driven by Steam callbacks. This crate never
  pumps them; exactly one system in your game must call `run_callbacks` (or `process_callbacks`)
  once per frame. A Steam lobby plugin usually does that already; then do not add a second pump.
- **Keep the `Client` alive**: Steam shuts down when the last clone drops. `SteamNetClient` holds
  one.
- The Steam server transport is **non-send data** in bevy_renet 5.0; the plugin inserts and
  removes it correctly. If you ever manage it yourself: `world.insert_non_send(transport)`, never
  `insert_resource` (it compiles, and every Steam system then silently sees nothing).
- A dedicated server has no Steam user: host it over UDP.

### 9. Combining it with Steam lobbies

Discovery is a separate concern. With a Steam lobby crate such as
[`bevy_steam_kit`](https://crates.io/crates/bevy_steam_kit) (feature `lobby`) the flow is: the host
opens a session and a lobby carrying its SteamID64; a friend's "Join Game" or an accepted invite
becomes `JoinRequested`; the game joins the lobby; `LobbyEntered` gives the host's id; the game
sends `JoinSession::steam(host)`. Let the Steam kit be the one Steam callback pump.

```rust,ignore
use bevy::prelude::*;
use bevy_net_session::*;
use bevy_steam_kit::*;

/// Host: session first, then a lobby that tells friends where to connect.
fn host(mut session: MessageWriter<HostSession>) {
    session.write(HostSession::steam(3));
}

fn open_lobby(mut started: MessageReader<SessionStarted>, mut lobby: MessageWriter<CreateLobby>) {
    for ev in started.read() {
        let Some(me) = ev.steam_id else { continue };
        lobby.write(CreateLobby {
            kind: LobbyKind::FriendsOnly,
            max_members: 4,
            data: vec![("host".into(), me.to_string())],
        });
    }
}

/// Joiner: accept the request, enter the lobby, connect to the host it names.
fn accept(mut requests: MessageReader<JoinRequested>, mut join: MessageWriter<JoinLobby>) {
    for req in requests.read() {
        join.write(JoinLobby { lobby: req.lobby });
    }
}

fn connect(
    mut entered: MessageReader<LobbyEntered>,
    backend: Res<SteamBackendRes>,
    mut join: MessageWriter<JoinSession>,
) {
    for ev in entered.read() {
        let host = backend
            .0
            .lobby()
            .and_then(|l| l.lobby_data(ev.lobby, "host"))
            .and_then(|h| h.parse::<u64>().ok());
        if let Some(host) = host {
            join.write(JoinSession::steam(host));
        }
    }
}
```

Leaving the session and leaving the lobby are independent requests; send both when the player
quits to the menu.

### 10. Protocol version and registration order

Two peers can talk only if they register the **same replicated types in the same order**
(replicon hashes the registrations into its `ProtocolHash` and refuses a mismatch). The plugin
registers its two handshake events in its `build`, and adds `("bevy_net_session",
protocol_version)` to the hash. So:

- Add `NetSessionPlugin` at the same point of your plugin order on every peer (client, listen
  host, dedicated server).
- Put every `replicate::<C>()`, `add_client_event`, `add_server_event`, ... of your game into ONE
  shared function or plugin that every build calls.
- Assert it in a test with `protocol_hash`:

```rust
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_net_session::*;

fn stack(dedicated_server: bool) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin, NetSessionPlugin { protocol_version: 3, ..default() }));
    // your_game::register_protocol(&mut app); // the same call in both builds
    let _ = dedicated_server;
    app.finish(); // the hash is computed here
    app.cleanup();
    app
}

assert_eq!(protocol_hash(&stack(false)), protocol_hash(&stack(true)));
```

- Bump `protocol_version` on every wire change. A peer on another version fails with
  `VersionMismatch`: through replicon's protocol check (its default `AuthMethod::ProtocolCheck`)
  or, if you turned that off, through the handshake's own version check.

### 11. System order

| set | schedule | what runs |
|---|---|---|
| `NetSessionSystems::Requests` | `Update` | read `LeaveSession`, `HostSession`, `JoinSession`; open transports; write `SessionStarted`, `HostFailed`, early `JoinFailed` |
| `NetSessionSystems::Watch` | `Update`, after `Requests` | send (and repeat every 0.5 s) the join request, run the join timeout, notice a lost connection |
| `NetSessionSystems::Close` | `PostUpdate`, after renet's `RenetSend` | execute a `LeaveSession`, pump a connection in its close grace |
| `NetSessionSystems::ExitClose` | `Last`, only on an `AppExit` frame | close everything |

The handshake answers (`JoinAccepted`, `PeerConnected`, handshake `JoinFailed`s) are written by
observers while replicon receives, in `PreUpdate`.

| your system | order it |
|---|---|
| writes a request and wants it handled this frame | `.before(NetSessionSystems::Requests)` in `Update` (otherwise next frame, also fine) |
| reads the facts | `.after(NetSessionSystems::Watch)` in `Update` to see this frame's, or anywhere next frame |
| sends a final message on leave | anywhere in `Update` of the leave frame: the close runs after `RenetSend` |
| saves on quit | before `NetSessionSystems::ExitClose` in `Last` |

Requests are read in the order leaves, hosts, joins, and the last one read stands.

### 12. Testing your game

The crate's own tests run a host app and a client app in one process over real UDP on
`127.0.0.1` (see `tests/loopback.rs`). The same works for your game: host on port `0`, read
`SessionStarted::local_addr`, join `127.0.0.1:<port>`, and update both apps in a loop.
`TimeUpdateStrategy::ManualDuration` makes timeouts deterministic. Replicon delivers client
events **locally** while no connection is up; a test that expects a message on the host must
wait for `JoinAccepted` first.

## How it works

- **Requests** (`Update`): the last request of the frame stands. Starting a session while one is
  open closes the old one and starts the new one a frame later. Transports are created in the
  request system (sockets, Steam handles) and inserted through commands; the Steam server
  transport as non-send data.
- **Handshake**: the client sends `(protocol_version, payload)` over replicon every 0.5 s until
  it gets an answer. The host answers only while hosting, only a remote client, and under
  replicon's `ProtocolCheck` only an authorized one (the resend covers the gap). Version first,
  then the validator. The answer is an independent server event (sent even before replication
  is authorized); a refused client is disconnected after the answer was sent.
- **Watchdog**: wall-clock time. A client transport that reports disconnected before the answer
  is `CouldNotReach` (never connected) or `LostConnection`; `join_timeout` gives `Timeout`;
  replicon's `ProtocolMismatch` event gives `VersionMismatch`.
- **Close**: remove the renet resources (replicon then sees the session end), despawn a client's
  `Remote` entities and a host's connected-client entities, keep the transport pumping for the
  grace on a leave, then disconnect.
- **State**: `NetSession` (role, state, transport, local id / address, a change counter).
  Nothing is saved; nothing of this crate's state is replicated beyond the handshake.

## API reference

### Plugin, configuration, state

| item | what |
|---|---|
| `NetSessionPlugin { protocol_version, join_timeout, client_close_grace, netcode_protocol_id }` | the plugin; `Default` = version 0, 15 s, 350 ms, `DEFAULT_NETCODE_PROTOCOL_ID` |
| `NetSessionConfig` | resource: a copy of the plugin's fields |
| `NetSession` | resource: `role()`, `state()`, `is_host()`, `is_client()`, `is_joined()`, `is_active()`, `transport()`, `local_id()`, `local_addr()`, `generation()` |
| `SessionRole` | `None`, `Host`, `Client` |
| `SessionState` | `Idle`, `Listening` (host), `Connecting` (client, handshake in progress), `Joined` |
| `SessionPeer { id }` | component on a host's connected-client entity once it passed the handshake |
| `NetSessionSystems` | `Requests`, `Watch` (`Update`), `Close` (`PostUpdate`), `ExitClose` (`Last`) |
| `SteamNetClient(steamworks::Client)` | resource (feature `steam`): the Steam client for the Steam transport |

### Request messages (you write)

| message | fields | effect |
|---|---|---|
| `HostSession` | `transport: HostTransport`, `max_clients: usize`; `HostSession::ip(port, max)`, `HostSession::steam(max)` | host; replaces any session |
| `JoinSession` | `target: JoinTarget`, `payload: Vec<u8>`; `JoinSession::ip(addr)`, `JoinSession::steam(host)`, `.with_payload(..)` | join; replaces any session |
| `LeaveSession` | none | leave after this frame's sends |

`HostTransport::Ip { port }` / `HostTransport::Steam { access: SteamAccess }`;
`JoinTarget::Ip(SocketAddr)` / `JoinTarget::Steam(u64)`;
`SteamAccess::{FriendsOnly (default), Public, Private, InLobby(u64), InList(Vec<u64>)}`.

### Fact messages (you read)

| message | fields | when |
|---|---|---|
| `SessionStarted` | `transport`, `local_addr: Option<SocketAddr>`, `steam_id: Option<u64>` | host listening |
| `HostFailed` | `reason: HostFailReason`, `message` | host did not start |
| `JoinAccepted` | `transport`, `local_id: u64` | the host accepted this client |
| `JoinFailed` | `reason: JoinFailReason`, `message` | a join ended without a session |
| `PeerConnected` | `entity`, `id` | host: a client passed the handshake |
| `PeerDisconnected` | `entity`, `id` | host: such a client is gone |
| `SessionEnded` | `reason: SessionEndReason` | a started session is over |

`TransportKind::{Ip, Steam}`. Reasons: see [Errors](#7-errors).

### Validation

| item | what |
|---|---|
| `JoinValidator` | trait: `fn validate(&self, &JoinRequestInfo) -> Result<(), String>`; implemented for matching closures |
| `JoinValidatorRes` | resource holding the validator; `JoinValidatorRes::new(closure)` |
| `JoinRequestInfo { client, id, version, payload }` | what the validator sees |

### Helpers and constants

| item | what |
|---|---|
| `protocol_hash(&App) -> Option<ProtocolHash>` | replicon's protocol hash after `app.finish()` |
| `MAX_IP_CLIENTS` | 1024, the UDP transport's cap |
| `DEFAULT_NETCODE_PROTOCOL_ID` | the default UDP protocol id |
| `bevy_net_session::bevy_replicon`, `::bevy_replicon_renet` | the exact dependencies, re-exported |

## Cargo features

| feature | default | adds |
|---|---|---|
| `ip` | on | UDP hosting and joining (renet netcode) |
| `steam` | off | Steam P2P hosting and joining (renet_steam 3.0.0), `SteamNetClient`; pulls `steamworks` 0.12.2 |

Without a transport feature the crate still builds; such requests fail with
`TransportUnavailable`.

## Compatibility

| bevy_net_session | Bevy | bevy_replicon | bevy_replicon_renet / bevy_renet | steamworks (optional) | Rust |
|---|---|---|---|---|---|
| 0.1 | 0.19.0 | 0.44.2 | 0.20.0 / 5.0 | 0.12.2 | 1.95+ |

Two peers must run the same `bevy_net_session`, replicon and `protocol_version`.

## Examples

| example | what it shows | needs |
|---|---|---|
| `cargo run --example quick_start` | host and client in one process over loopback: a refused password, an accepted join, a leave | nothing |
| `cargo run --example host_ip [-- <port> <password>]` | a UDP host printing who joins and leaves | a free UDP port |
| `cargo run --example join_ip -- <ip:port> [password]` | join, stay 10 s, leave | a running `host_ip` |
| `cargo run --example host_steam --features steam` | a friends-only Steam host; prints the SteamID64 to join | Steam running |
| `cargo run --example join_steam --features steam -- <host SteamID64>` | join a Steam host, stay 10 s, leave | Steam running, a friend hosting |

The Steam examples read the app id from `STEAM_APP_ID` (default 480) and pump Steam callbacks
themselves.

## Testing with real Steam

1. Two Steam accounts on two machines (one logged-in account per machine), friends with each
   other.
2. On both, Steam running and logged in. For `cargo run` on Windows, `steam_api64.dll` must be
   next to the executable when you start it outside cargo (steamworks ships it in its
   `redistributable_bin` folder).
3. Host: `cargo run --example host_steam --features steam`; note the printed SteamID64.
4. Friend: `cargo run --example join_steam --features steam -- <that id>`.
5. What to look for: the host logs `hosting on Steam as <id>`, the friend `joined the Steam host`,
   the host `Steam user <id> joined`, and after 10 s `left` on both sides. A non-friend under
   `FriendsOnly` gets `CouldNotReach`.

The crate's own tests never talk to Steam. CI builds everything with the `steam` feature on
Linux, Windows and macOS, and runs the tests with it on Windows.

## Limitations and FAQ

**Is the payload secure?** No. UDP runs renet netcode's *unsecure* mode and the payload is not
encrypted; treat a password as light gatekeeping for friends, not authentication. Over Steam the
transport is encrypted and `SteamAccess` gates who connects at all.

**Why steamworks 0.12.2 and not 0.13?** `renet_steam` 3.0.0 (the Steam transport of
bevy_renet 5.0 / bevy_replicon_renet 0.20) requires `steamworks` ^0.12.2, and two versions of
`steamworks` cannot link in one build.

**Can a host listen on UDP and Steam at once?** No (renet's two server transports cannot share
a server). Pick one per session.

**Host migration / reconnect?** No. When the host leaves, clients get
`SessionEnded { LostConnection }`; reconnecting is a new `JoinSession`.

**Dedicated servers?** Yes: a headless app with the plugin sends `HostSession::ip(..)`. The
dedicated server is a host with no local player; nothing here assumes a player.

**Does it replace `bevy_replicon`?** No. It opens and closes replicon's transports and runs a
join handshake on top; everything you replicate is plain replicon.

**Does it save anything or touch my world?** It saves nothing. It despawns a client's `Remote`
entities when the session ends (they belong to the host) and nothing else.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

## Contributing

Issues and pull requests are welcome. Before opening a pull request, please run:

```text
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo test --all-features
cargo doc --no-deps --all-features
cargo build --no-default-features
```

Tests must not need a Steam client or a network beyond loopback. Keep the README examples
compiling (they are checked by `cargo test --all-features`) and add a line to `CHANGELOG.md`.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
