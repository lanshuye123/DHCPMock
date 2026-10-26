# Contributing

Thanks for taking a look at `dhcp-mock`. This is a small, debugging-oriented
tool, so changes should stay focused and easy to reason about.

## Requirements

- Linux kernel ≥ 5.0 (netlink, `SO_BINDTODEVICE`, user namespaces)
- Rust 1.85+; CI currently pins 1.97.1
- For the end-to-end suite: `ip` (iproute2), `unshare`/`nsenter` (util-linux),
  `ping` (iputils), and an environment that allows unprivileged user+net
  namespaces (`unshare -Urn true` must succeed)

## Build and checks

```bash
cargo build
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test              # unit + dual-netns end-to-end
cargo build --release
```

If user namespaces are unavailable, skip the end-to-end suite explicitly:

```bash
DHCP_MOCK_SKIP_NETNS=1 cargo test
```

CI runs formatting, clippy, unit tests and a release build. The netns suite is
run locally (or on a privileged runner) because it needs namespace support:

```bash
cargo test --test netns -- --nocapture
```

## Running the tool

The tool needs `CAP_NET_ADMIN`, `CAP_NET_RAW` and `CAP_NET_BIND_SERVICE`:

```bash
sudo ./target/debug/dhcp-mock --iface eth1 --server-ip 10.99.99.1/24
# or
sudo setcap cap_net_admin,cap_net_raw,cap_net_bind_service+ep ./target/debug/dhcp-mock
```

For a safe local demo, use the root-only script (it only touches two isolated
netns):

```bash
sudo scripts/netns-demo.sh
```

## Code layout

See the “目录结构” section of [`README.md`](README.md). In short:

- `src/config.rs` — CLI, validation, pool derivation
- `src/netlink.rs` — link/address operations
- `src/dhcp/` — packet codec, options, pool, server state machine, socket,
  lease JSON
- `tests/netns.rs` — dual-netns end-to-end scenarios

## Guidelines

- Keep the tool’s runtime free of external command invocations (`ip`,
  `ifconfig`, …); use netlink/sockets. Tests and demo scripts may use `ip`.
- Preserve the interface isolation guarantee: the server must only send and
  receive on `--iface` (`SO_BINDTODEVICE`).
- Add or update unit tests for protocol/pool/config changes. For behaviour
  visible to clients, extend `tests/netns.rs`.
- Update [`README.md`](README.md) and [`CHANGELOG.md`](CHANGELOG.md) when a
  user-visible behaviour changes. If a lease JSON field changes, update
  `docs/lease-snapshot.schema.json` and the example.
- Run `cargo fmt` and clippy before committing.
- Prefer Conventional Commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`)
  and keep unrelated changes in separate commits.

## Reporting issues

Include the distribution/kernel, the exact command line, whether the tool ran
as root or with `setcap`, the console log (redact real MAC addresses if
needed), and `ip -br addr` output for the interface inside the relevant
namespace.
