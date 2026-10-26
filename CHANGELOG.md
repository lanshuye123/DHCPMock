# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-10-27

Initial release: a minimal DHCPv4 mock server for debugging directly attached
devices (M1–M5 of [`Design.md`](Design.md)).

### Added

- `--iface` / `--server-ip` CLI with CIDR, pool, lease, gateway, DNS and
  static-binding parsing and validation (exit code 2 on invalid config).
- Netlink link management: bring the interface up and add `server-ip/prefix`
  without depending on the `ip` command.
- Minimal DHCPv4 server on UDP/67 with `SO_BINDTODEVICE`:
  DISCOVER/OFFER, REQUEST/ACK/NAK, RELEASE, DECLINE, INFORM.
- Address pool derived from the server CIDR or supplied explicitly; reserved
  addresses, static bindings, round-robin allocation, offer timeout (60 s)and
  decline cooldown (300 s).
- Default broadcast reply policy and `--strict-unicast` for RFC 2131
  behaviour.
- Per-exchange console logging at INFO: `[DHCP] <mac> DISCOVER -> OFFER <ip>`,
  `[DHCP] <mac> REQUEST -> ACK <ip>`, plus RELEASE/DECLINE/NAK/INFORM and
  exhaustion warnings. `--log-dhcp` adds xid/destination/hostname details.
- Lease JSON snapshot via `--lease-file PATH` (atomic replace) and `SIGUSR1`;
  schema and example under [`docs/`](docs/).
- Graceful `SIGINT`/`SIGTERM` shutdown; `--cleanup-on-exit` removes the address
  added by the current run, while startup failures roll it back.
- Capability pre-check for `CAP_NET_ADMIN`, `CAP_NET_RAW` and
  `CAP_NET_BIND_SERVICE` with a `setcap` hint (exit code 3 if missing).
- 51 unit tests and a dual-network-namespace end-to-end suite (8 scenarios)
  runnable without root via unprivileged user namespaces.
- Root-only demo script [`scripts/netns-demo.sh`](scripts/netns-demo.sh).

### Changed

- DHCP exchanges are logged at INFO by default (the design document originally
  placed them at DEBUG behind `--log-dhcp`); this matches the embedded
  debugging use case. `--log-dhcp` now only adds extra details.
- The derived pool trims reserved addresses from its edges so logs and JSON
  show the usable range; explicit pools are validated instead of trimmed.

### Fixed

- `MsgType`'s `Display` implementation now honours width/alignment, keeping
  the console exchange lines aligned.

### Verified

- ISC `dhclient 4.4.3-P1` completes DISCOVER/OFFER/REQUEST/ACK in a two-netns
  setup, applies `10.99.99.100/24`, and accepts the emitted subnet mask,
  router, lease time, T1/T2, DNS and server identifier.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` and `cargo build --release` are clean.

### Known limitations

- DHCPv6, relay (`giaddr != 0`) and PXE boot extensions are out of scope.
- Leases are in memory only; restarting the server clears them.
- ISC `dhclient` verified; BusyBox `udhcpc`, Windows and ESP32 lwIP clients
  were not available on the development machine for the M6 matrix.
- `--foreground` is accepted for compatibility but daemonisation is not
  implemented.
