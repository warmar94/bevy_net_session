# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0, a breaking
change or a Bevy / bevy_replicon / steamworks bump raises the minor version).

## [0.1.1] - 2026-09-30

Documentation only, no code changes.

### Changed

- README: the Steam lobby example (section 9) uses `bevy_steam_kit` (feature `lobby`), the
  successor of the retired `bevy_steam_lobby`; install lines name the full version.

## [0.1.0] - 2026-09-28

First release, for Bevy 0.19.0, bevy_replicon 0.44.2, bevy_replicon_renet 0.20.0 (bevy_renet 5.0)
and, optionally, steamworks 0.12.2.

### Added

- `NetSessionPlugin` (config `protocol_version`, `join_timeout`, `client_close_grace`,
  `netcode_protocol_id`; copied into the `NetSessionConfig` resource). Adds `RepliconPlugins` and
  `RepliconRenetPlugins` when missing; the public `NetSessionSystems::{Requests, Watch, Close,
  ExitClose}` sets.
- Request messages `HostSession` (`HostTransport::Ip { port }` / `Steam { access }`,
  `max_clients`), `JoinSession` (`JoinTarget::Ip` / `Steam`, opaque `payload`), `LeaveSession`.
  A new request replaces the current session; the last request of a frame stands.
- Fact messages `SessionStarted`, `HostFailed` (`HostFailReason`), `JoinAccepted`, `JoinFailed`
  (`JoinFailReason`: `CouldNotReach`, `Timeout`, `LostConnection`, `VersionMismatch`,
  `Rejected`, `TransportUnavailable`, `CouldNotStart`, `Cancelled`), `PeerConnected`,
  `PeerDisconnected`, `SessionEnded` (`SessionEndReason`). Every join gets exactly one
  `JoinAccepted` or `JoinFailed`; every started session exactly one `SessionEnded`.
- The `NetSession` resource (role, state, transport, local id and address, change counter) and
  the `SessionPeer` component on a host's accepted clients.
- The join handshake: protocol version + opaque payload, checked by the host (version first,
  then the game's `JoinValidator` / `JoinValidatorRes`), answered with an independent server
  event; refused clients are disconnected after the answer was sent. The protocol version is
  also added to replicon's protocol hash; replicon's `ProtocolMismatch` is reported as
  `VersionMismatch`.
- Transports: UDP (feature `ip`, default) and Steam P2P (feature `steam`, `SteamNetClient`,
  `SteamAccess`), one server transport per host, the Steam server transport inserted as non-send
  data.
- Teardown: a leave closes in `PostUpdate` after `RenetSend`, then keeps the connection pumped
  for `client_close_grace` so the leave frame's messages are delivered; a client's `Remote`
  entities are despawned; `AppExit` closes every connection on the exit frame.
- `protocol_hash(&App)` for asserting that client and server register the same protocol; the
  `MAX_IP_CLIENTS` and `DEFAULT_NETCODE_PROTOCOL_ID` constants; re-exports of `bevy_replicon`
  and `bevy_replicon_renet`.
- Examples `quick_start` (loopback, runs anywhere), `host_ip`, `join_ip`, `host_steam` and
  `join_steam` (feature `steam`).
