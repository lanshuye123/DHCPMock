#!/usr/bin/env bash
#
# Root-only demo: two isolated network namespaces connected by a veth pair,
# dhcp-mock in one, a real DHCP client in the other.
#
#   sudo scripts/netns-demo.sh [path-to-dhcp-mock]
#
# Everything happens inside the two netns; the host network is untouched.
# The client uses a custom script that only configures the interface, so the
# host's /etc/resolv.conf is never modified.

set -euo pipefail

BIN="${1:-./target/debug/dhcp-mock}"
SRV_NS="dhcp-mock-srv"
CLI_NS="dhcp-mock-cli"
SRV_IF="veth-s"
CLI_IF="veth-c"
SERVER_CIDR="10.99.99.1/24"

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
    echo "error: run as root: sudo $0" >&2
    exit 1
fi
if [[ ! -x "$BIN" ]]; then
    echo "error: $BIN not found or not executable; run 'cargo build' first" >&2
    exit 1
fi
BIN="$(readlink -f "$BIN")"

WORKDIR="$(mktemp -d)"
SERVER_PID=""
cleanup() {
    set +e
    if [[ -n "$SERVER_PID" ]]; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
    fi
    ip netns pids "$CLI_NS" 2>/dev/null | xargs -r kill 2>/dev/null
    ip netns del "$SRV_NS" 2>/dev/null
    ip netns del "$CLI_NS" 2>/dev/null
    rm -rf "$WORKDIR"
}
trap cleanup EXIT INT TERM

# Remove leftovers from an aborted previous run.
ip netns del "$SRV_NS" 2>/dev/null || true
ip netns del "$CLI_NS" 2>/dev/null || true

cat >"$WORKDIR/udhcpc-script.sh" <<'EOF'
#!/bin/sh
case "$1" in
  bound|renew)
    ip addr flush dev "$interface" scope global 2>/dev/null || true
    ip addr add "$ip/$mask" dev "$interface" 2>/dev/null || true
    ;;
esac
exit 0
EOF

cat >"$WORKDIR/dhclient-script.sh" <<'EOF'
#!/bin/sh
case "$reason" in
  BOUND|RENEW|REBIND|REBOOT|IPV4)
    ip addr flush dev "$interface" scope global 2>/dev/null || true
    ip addr add "$new_ip_address/$new_subnet_mask" dev "$interface" 2>/dev/null || true
    ;;
esac
exit 0
EOF
chmod +x "$WORKDIR"/*.sh

ip netns add "$SRV_NS"
ip netns add "$CLI_NS"
ip link add "$SRV_IF" type veth peer name "$CLI_IF"
ip link set "$SRV_IF" netns "$SRV_NS"
ip link set "$CLI_IF" netns "$CLI_NS"
ip -n "$SRV_NS" link set lo up
ip -n "$CLI_NS" link set lo up
# veth-s is deliberately left down: dhcp-mock must bring it up itself.
ip -n "$CLI_NS" link set "$CLI_IF" up

echo "== starting dhcp-mock in netns $SRV_NS =="
ip netns exec "$SRV_NS" "$BIN" \
    --iface "$SRV_IF" \
    --server-ip "$SERVER_CIDR" \
    --pool 10.99.99.100-10.99.99.150 \
    --lease 2h \
    --dns 1.1.1.1,8.8.8.8 \
    --log-dhcp \
    --cleanup-on-exit &
SERVER_PID=$!

for _ in $(seq 1 100); do
    if ip -n "$SRV_NS" -4 addr show dev "$SRV_IF" 2>/dev/null | grep -q "$SERVER_CIDR"; then
        break
    fi
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "error: dhcp-mock exited during startup" >&2
        exit 1
    fi
    sleep 0.05
done
sleep 0.2

echo "== requesting a lease in netns $CLI_NS =="
if command -v udhcpc >/dev/null 2>&1; then
    ip netns exec "$CLI_NS" udhcpc -i "$CLI_IF" -f -q -n -t 5 \
        -s "$WORKDIR/udhcpc-script.sh"
elif command -v dhclient >/dev/null 2>&1; then
    ip netns exec "$CLI_NS" timeout 15 dhclient -1 -v \
        -sf "$WORKDIR/dhclient-script.sh" \
        -lf "$WORKDIR/dhclient.leases" "$CLI_IF" || true
else
    echo "error: install udhcpc (BusyBox) or dhclient (ISC)" >&2
    exit 1
fi

echo "== client interface =="
ip -n "$CLI_NS" -4 addr show dev "$CLI_IF"
echo "== ping server =="
ip netns exec "$CLI_NS" ping -c 1 -W 1 10.99.99.1 || true

echo "== demo finished; cleaning up =="
