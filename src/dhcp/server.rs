//! DHCP server state machine and receive loop.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing::{debug, info, warn};

use crate::config::{Config, format_duration};
use crate::dhcp::CLIENT_PORT;
use crate::dhcp::options::{
    DhcpOption, MsgType, OPT_DNS, OPT_LEASE_TIME, OPT_MESSAGE, OPT_MESSAGE_TYPE, OPT_ROUTER,
    OPT_SERVER_ID, OPT_SUBNET_MASK, OPT_T1, OPT_T2,
};
use crate::dhcp::packet::{BOOTREQUEST, Bootp};
use crate::dhcp::pool::{DECLINE_COOLDOWN, Pool, PoolError};
use crate::dhcp::report::{self, Counters, DeclinedRow, LeaseRow, Snapshot};
use crate::error::{AppError, Result};

/// Unix signals consumed by the server.
pub struct Signals {
    interrupt: Signal,
    terminate: Signal,
    user1: Signal,
}

impl Signals {
    pub fn new() -> Result<Self> {
        let interrupt = signal(SignalKind::interrupt())
            .map_err(|e| AppError::Runtime(format!("install SIGINT handler: {e}")))?;
        let terminate = signal(SignalKind::terminate())
            .map_err(|e| AppError::Runtime(format!("install SIGTERM handler: {e}")))?;
        let user1 = signal(SignalKind::user_defined1())
            .map_err(|e| AppError::Runtime(format!("install SIGUSR1 handler: {e}")))?;
        Ok(Self {
            interrupt,
            terminate,
            user1,
        })
    }
}

/// A serialized reply and its destination.
pub struct Outgoing {
    pub bytes: Vec<u8>,
    pub dest: SocketAddrV4,
}

pub struct HandleOutcome {
    pub reply: Option<Outgoing>,
    /// True when the lease table changed and `--lease-file` needs refreshing.
    pub pool_changed: bool,
}

impl HandleOutcome {
    fn none() -> Self {
        Self {
            reply: None,
            pool_changed: false,
        }
    }
}

pub struct Server {
    config: Config,
    ifindex: u32,
    pool: Pool,
    counters: Counters,
    started_at: Instant,
}

impl Server {
    pub fn new(config: Config, ifindex: u32, started_at: Instant) -> Self {
        let pool = Pool::new(
            config.pool,
            config.reserved.clone(),
            config.statics.clone(),
            config.lease,
        );
        Self {
            config,
            ifindex,
            pool,
            counters: Counters::default(),
            started_at,
        }
    }

    /// Receive loop. Returns `Ok(())` after a graceful signal-triggered stop.
    pub async fn run(&mut self, socket: UdpSocket, mut signals: Signals) -> Result<()> {
        let mut buffer = vec![0u8; 4096];
        let mut consecutive_errors = 0u32;
        let mut sweep = tokio::time::interval(Duration::from_secs(30));
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        sweep.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                biased;
                _ = signals.interrupt.recv() => {
                    info!("received SIGINT, shutting down");
                    break;
                }
                _ = signals.terminate.recv() => {
                    info!("received SIGTERM, shutting down");
                    break;
                }
                _ = signals.user1.recv() => {
                    self.dump_snapshot();
                }
                _ = sweep.tick() => {
                    if self.pool.sweep(Instant::now()) {
                        self.persist_snapshot();
                    }
                }
                result = socket.recv_from(&mut buffer) => {
                    match result {
                        Ok((len, _source)) => {
                            consecutive_errors = 0;
                            self.counters.received += 1;
                            let outcome = self.handle_datagram(&buffer[..len], Instant::now());
                            if outcome.pool_changed {
                                self.persist_snapshot();
                            }
                            if let Some(outgoing) = outcome.reply {
                                match socket.send_to(&outgoing.bytes, outgoing.dest).await {
                                    Ok(_) => self.counters.sent += 1,
                                    Err(e) => {
                                        self.counters.send_errors += 1;
                                        warn!(dest = %outgoing.dest, error = %e, "failed to send DHCP reply");
                                    }
                                }
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            consecutive_errors += 1;
                            warn!(error = %e, consecutive_errors, "recvfrom failed");
                            if consecutive_errors >= 10 {
                                self.pool.sweep(Instant::now());
                                self.persist_snapshot();
                                return Err(AppError::Runtime(format!(
                                    "too many consecutive socket receive errors: {e}"
                                )));
                            }
                        }
                    }
                }
            }
        }

        self.pool.sweep(Instant::now());
        self.persist_snapshot();
        info!(
            leases = self.pool.len(),
            counters = ?self.counters,
            "stopped"
        );
        Ok(())
    }

    /// Handle one datagram. Kept synchronous and socket-free so it can be
    /// unit-tested directly.
    pub fn handle_datagram(&mut self, data: &[u8], now: Instant) -> HandleOutcome {
        let request = match Bootp::parse(data) {
            Ok(packet) => packet,
            Err(e) => {
                self.counters.parse_errors += 1;
                self.counters.dropped += 1;
                debug!(error = %e, "dropping malformed DHCP packet");
                return HandleOutcome::none();
            }
        };

        if request.op != BOOTREQUEST {
            self.counters.dropped += 1;
            debug!(op = request.op, "ignoring non-BOOTREQUEST packet");
            return HandleOutcome::none();
        }

        let msg_type = match request.msg_type() {
            Some(msg_type) => msg_type,
            None => {
                self.counters.dropped += 1;
                debug!(mac = %request.mac(), "ignoring packet without a valid DHCP message type");
                return HandleOutcome::none();
            }
        };

        if !request.giaddr.is_unspecified() {
            self.counters.dropped += 1;
            debug!(giaddr = %request.giaddr, "ignoring relayed DHCP packet (giaddr != 0)");
            return HandleOutcome::none();
        }

        if request.htype != 1 || request.hlen != 6 {
            self.counters.dropped += 1;
            debug!(
                htype = request.htype,
                hlen = request.hlen,
                "ignoring non-Ethernet DHCP packet"
            );
            return HandleOutcome::none();
        }

        let mac = request.mac();

        match msg_type {
            MsgType::Discover => {
                self.counters.discover += 1;
                match self.pool.offer(mac, now) {
                    Ok(ip) => HandleOutcome {
                        reply: self.make_reply(&request, MsgType::Offer, ip, true, msg_type),
                        pool_changed: true,
                    },
                    Err(e) => {
                        self.counters.dropped += 1;
                        warn!(
                            target: "dhcp",
                            "{}",
                            format!(
                                "[DHCP] {mac}  {:<8} -> no offer  ({e})",
                                MsgType::Discover
                            )
                        );
                        HandleOutcome::none()
                    }
                }
            }

            MsgType::Request => {
                self.counters.request += 1;
                let mut outcome = HandleOutcome::none();

                if let Some(server_id) = request.server_id() {
                    if server_id != self.config.server_ip {
                        // RFC 2131 §4.3.2: the client selected another server;
                        // stay silent and release our offer.
                        info!(
                            target: "dhcp",
                            "{}",
                            format!(
                                "[DHCP] {mac}  {:<8} ignored (client selected server {server_id})",
                                MsgType::Request
                            )
                        );
                        outcome.pool_changed = self.pool.release(mac).is_some();
                        return outcome;
                    }
                }

                let requested = if !request.ciaddr.is_unspecified() {
                    Some(request.ciaddr)
                } else {
                    request.requested_ip()
                };
                let Some(requested) = requested else {
                    self.counters.dropped += 1;
                    warn!(
                        target: "dhcp",
                        "{}",
                        format!(
                            "[DHCP] {mac}  {:<8} dropped (missing requested IP)",
                            MsgType::Request
                        )
                    );
                    return outcome;
                };

                match self.pool.confirm(mac, requested, now) {
                    Ok(()) => {
                        outcome.reply =
                            self.make_reply(&request, MsgType::Ack, requested, true, msg_type);
                        outcome.pool_changed = true;
                    }
                    Err(e) => {
                        outcome.pool_changed = self.pool.release(mac).is_some();
                        outcome.reply = self.make_nak(&request, &e, msg_type);
                    }
                }
                outcome
            }

            MsgType::Release => {
                self.counters.release += 1;
                if let Some(server_id) = request.server_id() {
                    if server_id != self.config.server_ip {
                        debug!(mac = %mac, server_id = %server_id, "ignoring RELEASE for another server");
                        return HandleOutcome::none();
                    }
                }
                let released = self.pool.release(mac);
                match released {
                    Some(lease) => info!(
                        target: "dhcp",
                        "{}",
                        dhcp_line(mac, MsgType::Release, None, lease.ip, None)
                    ),
                    None => {
                        let ip = if !request.ciaddr.is_unspecified() {
                            request.ciaddr
                        } else {
                            request.requested_ip().unwrap_or(Ipv4Addr::UNSPECIFIED)
                        };
                        info!(
                            target: "dhcp",
                            "{}",
                            dhcp_line(
                                mac,
                                MsgType::Release,
                                None,
                                ip,
                                Some("no tracked lease")
                            )
                        );
                    }
                }
                HandleOutcome {
                    reply: None,
                    pool_changed: released.is_some(),
                }
            }

            MsgType::Decline => {
                self.counters.decline += 1;
                if let Some(server_id) = request.server_id() {
                    if server_id != self.config.server_ip {
                        debug!(mac = %mac, server_id = %server_id, "ignoring DECLINE for another server");
                        return HandleOutcome::none();
                    }
                }
                let Some(ip) = request.requested_ip() else {
                    self.counters.dropped += 1;
                    warn!(
                        target: "dhcp",
                        "{}",
                        format!(
                            "[DHCP] {mac}  {:<8} dropped (missing requested IP)",
                            MsgType::Decline
                        )
                    );
                    return HandleOutcome::none();
                };
                if self.pool.decline(mac, ip, now) {
                    let note = format!(
                        "client reported conflict, cooldown {}",
                        format_duration(DECLINE_COOLDOWN.as_secs())
                    );
                    warn!(
                        target: "dhcp",
                        "{}",
                        dhcp_line(mac, MsgType::Decline, None, ip, Some(&note))
                    );
                    HandleOutcome {
                        reply: None,
                        pool_changed: true,
                    }
                } else {
                    warn!(
                        target: "dhcp",
                        "{}",
                        dhcp_line(
                            mac,
                            MsgType::Decline,
                            None,
                            ip,
                            Some("outside pool, ignored")
                        )
                    );
                    HandleOutcome::none()
                }
            }

            MsgType::Inform => {
                self.counters.inform += 1;
                if let Some(server_id) = request.server_id() {
                    if server_id != self.config.server_ip {
                        debug!(mac = %mac, server_id = %server_id, "ignoring INFORM for another server");
                        return HandleOutcome::none();
                    }
                }
                HandleOutcome {
                    reply: self.make_reply(
                        &request,
                        MsgType::Ack,
                        Ipv4Addr::UNSPECIFIED,
                        false,
                        msg_type,
                    ),
                    pool_changed: false,
                }
            }

            MsgType::Offer | MsgType::Ack | MsgType::Nak => {
                self.counters.dropped += 1;
                debug!(kind = %msg_type, "ignoring server-to-server DHCP message");
                HandleOutcome::none()
            }
        }
    }

    pub fn write_snapshot(&self, path: &std::path::Path) -> Result<()> {
        report::write_snapshot(path, &self.snapshot())
    }

    fn snapshot(&self) -> Snapshot {
        let now = Instant::now();
        let (leases, declined) = self.pool.views(now);
        Snapshot {
            schema_version: report::SCHEMA_VERSION,
            iface: self.config.iface.clone(),
            ifindex: self.ifindex,
            server_ip: self.config.server_ip.to_string(),
            prefix: self.config.prefix,
            network: self.config.network.to_string(),
            broadcast: self.config.broadcast.to_string(),
            pool_start: self.config.pool.0.to_string(),
            pool_end: self.config.pool.1.to_string(),
            lease_seconds: self.config.lease_secs,
            uptime_seconds: self.started_at.elapsed().as_secs(),
            generated_at_unix: report::unix_now(),
            leases: leases
                .into_iter()
                .map(|view| LeaseRow {
                    mac: view.mac,
                    ip: view.ip.to_string(),
                    state: view.state.to_string(),
                    expires_in_secs: view.expires_in.as_secs(),
                })
                .collect(),
            declined: declined
                .into_iter()
                .map(|view| DeclinedRow {
                    ip: view.ip.to_string(),
                    cooldown_secs: view.cooldown.as_secs(),
                })
                .collect(),
            counters: self.counters,
        }
    }

    fn persist_snapshot(&self) {
        if let Some(path) = &self.config.lease_file {
            if let Err(e) = self.write_snapshot(path) {
                warn!(path = %path.display(), error = %e, "failed to update lease snapshot");
            }
        }
    }

    fn dump_snapshot(&self) {
        let snapshot = self.snapshot();
        if let Some(path) = &self.config.lease_file {
            match report::write_snapshot(path, &snapshot) {
                Ok(()) => info!(path = %path.display(), "lease snapshot written"),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to write lease snapshot")
                }
            }
        } else {
            match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => info!(target: "lease", "{json}"),
                Err(e) => warn!(error = %e, "failed to serialize lease snapshot"),
            }
        }
    }

    fn make_reply(
        &mut self,
        request: &Bootp,
        msg_type: MsgType,
        yiaddr: Ipv4Addr,
        include_lease: bool,
        request_kind: MsgType,
    ) -> Option<Outgoing> {
        let dest_ip = reply_destination(request, yiaddr, self.config.strict_unicast);
        let dest = SocketAddrV4::new(dest_ip, CLIENT_PORT);
        let reply = self.build_reply(request, msg_type, yiaddr, include_lease);

        match msg_type {
            MsgType::Offer => self.counters.offer += 1,
            MsgType::Ack => self.counters.ack += 1,
            MsgType::Nak => self.counters.nak += 1,
            _ => {}
        }

        self.log_reply(request, request_kind, msg_type, yiaddr);

        if tracing::enabled!(target: "dhcp", tracing::Level::DEBUG) {
            let host = request.hostname().unwrap_or_else(|| "-".to_string());
            debug!(
                target: "dhcp",
                "[DHCP] xid={} mac={} kind={} reply={} yiaddr={} dest={} host={}",
                HexXid(request.xid),
                request.mac(),
                request_kind,
                msg_type,
                display_ip(yiaddr),
                dest,
                host
            );
        }
        Some(Outgoing {
            bytes: reply.serialize(),
            dest,
        })
    }

    /// Always-visible one-liner for the console: which MAC got which address.
    fn log_reply(&self, request: &Bootp, request_kind: MsgType, reply: MsgType, ip: Ipv4Addr) {
        let note = if request_kind == MsgType::Inform {
            "no lease".to_string()
        } else {
            format!(
                "lease {}",
                format_duration(u64::from(self.config.lease_secs))
            )
        };
        info!(
            target: "dhcp",
            "{}",
            dhcp_line(
                request.mac(),
                request_kind,
                Some(reply),
                ip,
                Some(&note)
            )
        );
    }

    fn make_nak(
        &mut self,
        request: &Bootp,
        error: &PoolError,
        request_kind: MsgType,
    ) -> Option<Outgoing> {
        let dest_ip = reply_destination(request, Ipv4Addr::UNSPECIFIED, self.config.strict_unicast);
        let dest = SocketAddrV4::new(dest_ip, CLIENT_PORT);
        let reply = self.build_nak(request, error);
        self.counters.nak += 1;

        self.log_nak(request, request_kind, error);

        if tracing::enabled!(target: "dhcp", tracing::Level::DEBUG) {
            let host = request.hostname().unwrap_or_else(|| "-".to_string());
            debug!(
                target: "dhcp",
                "[DHCP] xid={} mac={} kind={} reply={} requested={} dest={} host={} reason={}",
                HexXid(request.xid),
                request.mac(),
                request_kind,
                MsgType::Nak,
                display_ip(request.requested_ip().unwrap_or(Ipv4Addr::UNSPECIFIED)),
                dest,
                host,
                error
            );
        }
        Some(Outgoing {
            bytes: reply.serialize(),
            dest,
        })
    }

    fn log_nak(&self, request: &Bootp, request_kind: MsgType, error: &PoolError) {
        let requested = if !request.ciaddr.is_unspecified() {
            Some(request.ciaddr)
        } else {
            request.requested_ip()
        };
        let reason = error.to_string();
        info!(
            target: "dhcp",
            "{}",
            dhcp_line(
                request.mac(),
                request_kind,
                Some(MsgType::Nak),
                requested.unwrap_or(Ipv4Addr::UNSPECIFIED),
                Some(&reason)
            )
        );
    }

    fn build_reply(
        &self,
        request: &Bootp,
        msg_type: MsgType,
        yiaddr: Ipv4Addr,
        include_lease: bool,
    ) -> Bootp {
        let mut reply = Bootp::new_reply(request, yiaddr);
        reply.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, msg_type.code()));
        reply.push_option(DhcpOption::ipv4(OPT_SERVER_ID, self.config.server_ip));
        if include_lease {
            reply.push_option(DhcpOption::u32(OPT_LEASE_TIME, self.config.lease_secs));
        }
        reply.push_option(DhcpOption::ipv4(OPT_SUBNET_MASK, self.config.mask));
        if let Some(gateway) = self.config.gateway {
            reply.push_option(DhcpOption::ipv4(OPT_ROUTER, gateway));
        }
        if !self.config.dns.is_empty() {
            reply
                .options
                .extend(DhcpOption::ipv4_list(OPT_DNS, &self.config.dns));
        }
        if include_lease {
            if let Some((t1, t2)) = self.config.t1_t2() {
                reply.push_option(DhcpOption::u32(OPT_T1, t1));
                reply.push_option(DhcpOption::u32(OPT_T2, t2));
            }
        }
        reply
    }

    fn build_nak(&self, request: &Bootp, error: &PoolError) -> Bootp {
        let mut reply = Bootp::new_reply(request, Ipv4Addr::UNSPECIFIED);
        reply.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Nak.code()));
        reply.push_option(DhcpOption::ipv4(OPT_SERVER_ID, self.config.server_ip));
        let message = error.to_string();
        let bytes = message.as_bytes();
        let take = bytes.len().min(255);
        reply.push_option(DhcpOption::new(OPT_MESSAGE, bytes[..take].to_vec()));
        reply
    }
}

/// Where a server reply should be sent.
///
/// The default (non-strict) policy broadcasts whenever the client has no IP,
/// because unicasting to `yiaddr` requires the client to answer ARP for an
/// address it does not own yet. `--strict-unicast` keeps RFC 2131 behaviour.
pub fn reply_destination(request: &Bootp, yiaddr: Ipv4Addr, strict_unicast: bool) -> Ipv4Addr {
    let dest = if request.wants_broadcast() {
        Ipv4Addr::BROADCAST
    } else if !request.ciaddr.is_unspecified() {
        request.ciaddr
    } else if strict_unicast {
        yiaddr
    } else {
        Ipv4Addr::BROADCAST
    };

    if dest.is_unspecified() {
        Ipv4Addr::BROADCAST
    } else {
        dest
    }
}

/// Build one aligned console line, formatted eagerly so width specifiers work
/// (tracing's message-field interpolation drops them).
fn dhcp_line(
    mac: crate::dhcp::packet::MacAddr,
    request: MsgType,
    reply: Option<MsgType>,
    ip: Ipv4Addr,
    note: Option<&str>,
) -> String {
    let ip = display_ip(ip);
    let line = match reply {
        Some(reply) => format!("[DHCP] {mac}  {request:<8} -> {reply:<5}  {ip}"),
        None => format!("[DHCP] {mac}  {request:<8}     {ip}"),
    };
    match note {
        Some(note) => format!("{line}  ({note})"),
        None => line,
    }
}

fn display_ip(ip: Ipv4Addr) -> String {
    if ip.is_unspecified() {
        "-".to_string()
    } else {
        ip.to_string()
    }
}

struct HexXid(u32);

impl fmt::Display for HexXid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08x}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Cidr, Cli, LeaseDuration, PoolRange};
    use crate::dhcp::options::{OPT_MESSAGE, OPT_REQUESTED_IP};
    use crate::dhcp::packet::BROADCAST_FLAG;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn mac(s: &str) -> crate::dhcp::packet::MacAddr {
        s.parse().unwrap()
    }

    fn test_config() -> Config {
        Config::from_cli(Cli {
            iface: "veth-test".into(),
            server_ip: Cidr {
                addr: ip("10.99.99.1"),
                prefix: 24,
            },
            pool: Some(PoolRange {
                start: ip("10.99.99.100"),
                end: ip("10.99.99.110"),
            }),
            lease: LeaseDuration(7_200),
            gateway: None,
            dns: vec![ip("1.1.1.1"), ip("8.8.8.8")],
            statics: Vec::new(),
            cleanup_on_exit: false,
            no_config_addr: false,
            log_dhcp: true,
            foreground: false,
            strict_unicast: false,
            lease_file: None,
        })
        .unwrap()
    }

    fn discover(mac: crate::dhcp::packet::MacAddr, xid: u32) -> Bootp {
        let mut packet = Bootp::new_request(mac, xid);
        packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Discover.code()));
        packet
    }

    fn request(
        mac: crate::dhcp::packet::MacAddr,
        xid: u32,
        requested: Ipv4Addr,
        server_id: Option<Ipv4Addr>,
    ) -> Bootp {
        let mut packet = Bootp::new_request(mac, xid);
        packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Request.code()));
        packet.push_option(DhcpOption::ipv4(OPT_REQUESTED_IP, requested));
        if let Some(server_id) = server_id {
            packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, server_id));
        }
        packet
    }

    #[test]
    fn discover_request_release_flow() {
        let mut server = Server::new(test_config(), 7, Instant::now());
        let client = mac("02:00:00:00:00:01");
        let now = Instant::now();

        let outcome = server.handle_datagram(&discover(client, 0x1111).serialize(), now);
        let reply = Bootp::parse(&outcome.reply.unwrap().bytes).unwrap();
        assert_eq!(reply.op, 2);
        assert_eq!(reply.msg_type(), Some(MsgType::Offer));
        assert_eq!(reply.yiaddr, ip("10.99.99.100"));
        assert_eq!(reply.server_id(), Some(ip("10.99.99.1")));
        assert_eq!(reply.lease_time(), Some(7_200));
        assert_eq!(
            reply.ipv4_option(OPT_SUBNET_MASK),
            Some(ip("255.255.255.0"))
        );
        assert_eq!(reply.ipv4_option(OPT_ROUTER), Some(ip("10.99.99.1")));
        assert_eq!(
            reply.ipv4_list_option(OPT_DNS),
            vec![ip("1.1.1.1"), ip("8.8.8.8")]
        );

        let outcome = server.handle_datagram(
            &request(client, 0x1112, ip("10.99.99.100"), Some(ip("10.99.99.1"))).serialize(),
            now,
        );
        let reply = Bootp::parse(&outcome.reply.unwrap().bytes).unwrap();
        assert_eq!(reply.msg_type(), Some(MsgType::Ack));
        assert_eq!(reply.yiaddr, ip("10.99.99.100"));
        assert_eq!(server.pool.len(), 1);

        let mut release = Bootp::new_request(client, 0x1113);
        release.ciaddr = ip("10.99.99.100");
        release.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Release.code()));
        release.push_option(DhcpOption::ipv4(OPT_SERVER_ID, ip("10.99.99.1")));
        let outcome = server.handle_datagram(&release.serialize(), now);
        assert!(outcome.reply.is_none());
        assert!(outcome.pool_changed);
        assert_eq!(server.pool.len(), 0);
    }

    #[test]
    fn request_without_offer_gets_nak() {
        let mut server = Server::new(test_config(), 1, Instant::now());
        let client = mac("02:00:00:00:00:02");
        let outcome = server.handle_datagram(
            &request(client, 0x2222, ip("10.99.99.105"), Some(ip("10.99.99.1"))).serialize(),
            Instant::now(),
        );
        let reply = Bootp::parse(&outcome.reply.unwrap().bytes).unwrap();
        assert_eq!(reply.msg_type(), Some(MsgType::Nak));
        assert_eq!(reply.yiaddr, ip("0.0.0.0"));
        assert!(reply.option(OPT_MESSAGE).is_some());
    }

    #[test]
    fn request_for_other_server_is_ignored() {
        let mut server = Server::new(test_config(), 1, Instant::now());
        let client = mac("02:00:00:00:00:03");
        let now = Instant::now();
        server.handle_datagram(&discover(client, 0x3333).serialize(), now);
        assert!(server.pool.lookup(client).is_some());

        let outcome = server.handle_datagram(
            &request(client, 0x3334, ip("10.99.99.100"), Some(ip("10.99.99.254"))).serialize(),
            now,
        );
        assert!(outcome.reply.is_none());
        assert!(outcome.pool_changed);
        assert!(server.pool.lookup(client).is_none());
    }

    #[test]
    fn inform_ack_has_no_lease_time() {
        let mut server = Server::new(test_config(), 1, Instant::now());
        let client = mac("02:00:00:00:00:04");
        let mut inform = Bootp::new_request(client, 0x4444);
        inform.ciaddr = ip("10.99.99.50");
        inform.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Inform.code()));
        let outcome = server.handle_datagram(&inform.serialize(), Instant::now());
        let outgoing = outcome.reply.unwrap();
        assert_eq!(outgoing.dest.ip(), &ip("10.99.99.50"));
        let reply = Bootp::parse(&outgoing.bytes).unwrap();
        assert_eq!(reply.msg_type(), Some(MsgType::Ack));
        assert_eq!(reply.yiaddr, ip("0.0.0.0"));
        assert_eq!(reply.lease_time(), None);
        assert_eq!(
            reply.ipv4_option(OPT_SUBNET_MASK),
            Some(ip("255.255.255.0"))
        );
    }

    #[test]
    fn decline_cools_down_the_address() {
        let mut server = Server::new(test_config(), 1, Instant::now());
        let client = mac("02:00:00:00:00:05");
        let now = Instant::now();
        let outcome = server.handle_datagram(&discover(client, 0x5555).serialize(), now);
        let offered = Bootp::parse(&outcome.reply.unwrap().bytes).unwrap().yiaddr;

        let mut decline = Bootp::new_request(client, 0x5556);
        decline.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Decline.code()));
        decline.push_option(DhcpOption::ipv4(OPT_REQUESTED_IP, offered));
        decline.push_option(DhcpOption::ipv4(OPT_SERVER_ID, ip("10.99.99.1")));
        let outcome = server.handle_datagram(&decline.serialize(), now);
        assert!(outcome.reply.is_none());
        assert!(outcome.pool_changed);

        let other = mac("02:00:00:00:00:06");
        let outcome = server.handle_datagram(&discover(other, 0x5557).serialize(), now);
        let reply = Bootp::parse(&outcome.reply.unwrap().bytes).unwrap();
        assert_ne!(reply.yiaddr, offered);
    }

    #[test]
    fn reply_destination_policy() {
        let mut request = Bootp::new_request(mac("02:00:00:00:00:01"), 1);
        let yiaddr = ip("10.99.99.100");

        request.flags = BROADCAST_FLAG;
        request.ciaddr = ip("10.99.99.50");
        assert_eq!(
            reply_destination(&request, yiaddr, false),
            Ipv4Addr::BROADCAST
        );
        assert_eq!(
            reply_destination(&request, yiaddr, true),
            Ipv4Addr::BROADCAST
        );

        request.flags = 0;
        request.ciaddr = ip("10.99.99.50");
        assert_eq!(
            reply_destination(&request, yiaddr, false),
            ip("10.99.99.50")
        );
        assert_eq!(reply_destination(&request, yiaddr, true), ip("10.99.99.50"));

        request.ciaddr = Ipv4Addr::UNSPECIFIED;
        assert_eq!(
            reply_destination(&request, yiaddr, false),
            Ipv4Addr::BROADCAST
        );
        assert_eq!(reply_destination(&request, yiaddr, true), yiaddr);

        // A NAK with no yiaddr must fall back to broadcast even in strict mode.
        assert_eq!(
            reply_destination(&request, Ipv4Addr::UNSPECIFIED, true),
            Ipv4Addr::BROADCAST
        );
    }
}
