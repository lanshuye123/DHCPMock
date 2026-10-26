//! CLI definition and configuration validation.
//!
//! All validation that can fail without touching the system happens here and
//! maps to exit code 2 (`AppError::Config`).

use std::collections::HashSet;
use std::fmt;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;

use crate::dhcp::packet::MacAddr;
use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------
// CLI value types
// ---------------------------------------------------------------------------

/// `ADDRESS/PREFIX`, e.g. `10.99.99.1/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (addr, prefix) = s.split_once('/').ok_or_else(|| {
            format!("invalid CIDR `{s}`: expected ADDRESS/PREFIX, e.g. 10.99.99.1/24")
        })?;
        let addr = addr
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid IPv4 address `{addr}`"))?;
        let prefix = prefix
            .parse::<u8>()
            .map_err(|_| format!("invalid prefix length `{prefix}` in `{s}`"))?;
        if prefix > 32 {
            return Err(format!("invalid prefix length /{prefix}: must be 0..=32"));
        }
        Ok(Cidr { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// `START-END`, e.g. `10.99.99.100-10.99.99.150`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRange {
    pub start: Ipv4Addr,
    pub end: Ipv4Addr,
}

impl FromStr for PoolRange {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (start, end) = s
            .split_once('-')
            .ok_or_else(|| format!("invalid pool `{s}`: expected START-END"))?;
        let start = start
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid pool start address `{start}`"))?;
        let end = end
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid pool end address `{end}`"))?;
        if start > end {
            return Err(format!("invalid pool `{s}`: start is greater than end"));
        }
        Ok(PoolRange { start, end })
    }
}

impl fmt::Display for PoolRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

/// `MAC=IP` static binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticBinding {
    pub mac: MacAddr,
    pub ip: Ipv4Addr,
}

impl FromStr for StaticBinding {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let (mac, ip) = s
            .split_once('=')
            .ok_or_else(|| format!("invalid static binding `{s}`: expected MAC=IP"))?;
        let mac = mac
            .trim()
            .parse::<MacAddr>()
            .map_err(|e| format!("invalid static binding `{s}`: {e}"))?;
        let ip = ip
            .trim()
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("invalid static binding `{s}`: bad IPv4 address `{ip}`"))?;
        Ok(StaticBinding { mac, ip })
    }
}

/// Lease duration; bare numbers are seconds, otherwise `s`/`m`/`h`/`d`/`w`
/// units may be combined (`90s`, `30m`, `12h`, `1d`, `1h30m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaseDuration(pub u64);

impl FromStr for LeaseDuration {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration".into());
        }

        if s.chars().all(|c| c.is_ascii_digit()) {
            let secs = s
                .parse::<u64>()
                .map_err(|e| format!("invalid duration `{s}`: {e}"))?;
            return Ok(LeaseDuration(secs));
        }

        let mut total: u64 = 0;
        let mut digits = String::new();
        let mut saw_unit = false;
        for ch in s.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                continue;
            }
            if digits.is_empty() {
                return Err(format!(
                    "invalid duration `{s}`: expected a number before `{ch}`"
                ));
            }
            let value = digits
                .parse::<u64>()
                .map_err(|e| format!("invalid duration `{s}`: {e}"))?;
            let multiplier: u64 = match ch.to_ascii_lowercase() {
                's' => 1,
                'm' => 60,
                'h' => 60 * 60,
                'd' => 24 * 60 * 60,
                'w' => 7 * 24 * 60 * 60,
                other => {
                    return Err(format!(
                        "invalid duration `{s}`: unknown unit `{other}` (use s, m, h, d or w)"
                    ));
                }
            };
            total = total
                .checked_add(
                    value
                        .checked_mul(multiplier)
                        .ok_or_else(|| format!("duration `{s}` is too large"))?,
                )
                .ok_or_else(|| format!("duration `{s}` is too large"))?;
            digits.clear();
            saw_unit = true;
        }

        if !digits.is_empty() || !saw_unit {
            return Err(format!("invalid duration `{s}`: missing unit"));
        }
        Ok(LeaseDuration(total))
    }
}

/// Human-readable duration used in logs; picks the largest exact unit.
pub fn format_duration(seconds: u64) -> String {
    if seconds >= 86_400 && seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 && seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 && seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

// ---------------------------------------------------------------------------
// clap definition
// ---------------------------------------------------------------------------

#[derive(Debug, Parser)]
#[command(
    name = "dhcp-mock",
    version,
    about = "Minimal DHCPv4 mock server for debugging directly attached devices",
    after_help = "EXAMPLES:\n  \
        sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 --log-dhcp\n  \
        sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 \\\n    \
        --pool 10.99.99.100-10.99.99.150 \\\n    \
        --static aa:bb:cc:dd:ee:ff=10.99.99.100 \\\n    \
        --lease 2h --dns 1.1.1.1 --cleanup-on-exit"
)]
pub struct Cli {
    /// Interface to serve DHCP on, e.g. eth1 or enp0s20u1
    #[arg(long, value_name = "NAME")]
    pub iface: String,

    /// Server address and prefix, e.g. 10.99.99.1/24
    #[arg(long, value_name = "CIDR")]
    pub server_ip: Cidr,

    /// Address pool START-END (default: derived from --server-ip)
    #[arg(long, value_name = "START-END")]
    pub pool: Option<PoolRange>,

    /// Lease duration (30m, 12h, 1d; default: 12h)
    #[arg(long, value_name = "DURATION", default_value = "12h")]
    pub lease: LeaseDuration,

    /// Gateway to hand out (default: the server address)
    #[arg(long, value_name = "IP")]
    pub gateway: Option<Ipv4Addr>,

    /// DNS server(s) to hand out; comma separated or repeated
    #[arg(long, value_name = "IP[,IP]", value_delimiter = ',')]
    pub dns: Vec<Ipv4Addr>,

    /// Static binding MAC=IP (repeatable)
    #[arg(long = "static", value_name = "MAC=IP")]
    pub statics: Vec<StaticBinding>,

    /// Remove the address added by this tool on exit
    #[arg(long)]
    pub cleanup_on_exit: bool,

    /// Do not add --server-ip to the interface
    #[arg(long)]
    pub no_config_addr: bool,

    /// Log extra DHCP details (xid, destination, hostname) at debug level
    #[arg(long)]
    pub log_dhcp: bool,

    /// Stay in the foreground (default; daemon mode is not implemented)
    #[arg(long)]
    pub foreground: bool,

    /// Reply with RFC 2131 unicast semantics instead of always broadcasting
    #[arg(long)]
    pub strict_unicast: bool,

    /// Write a JSON snapshot of the lease table to PATH (updated on change)
    #[arg(long, value_name = "PATH")]
    pub lease_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Validated configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Config {
    pub iface: String,
    pub server_ip: Ipv4Addr,
    pub prefix: u8,
    pub mask: Ipv4Addr,
    pub network: Ipv4Addr,
    pub broadcast: Ipv4Addr,
    pub pool: (Ipv4Addr, Ipv4Addr),
    pub lease: Duration,
    pub lease_secs: u32,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub statics: Vec<(MacAddr, Ipv4Addr)>,
    /// Addresses that must never be handed out dynamically.
    pub reserved: HashSet<Ipv4Addr>,
    pub cleanup_on_exit: bool,
    pub no_config_addr: bool,
    pub log_dhcp: bool,
    pub strict_unicast: bool,
    pub lease_file: Option<PathBuf>,
}

impl Config {
    pub fn from_cli(cli: Cli) -> Result<Self> {
        validate_iface(&cli.iface)?;

        let Cidr {
            addr: server_ip,
            prefix,
        } = cli.server_ip;
        if !(1..=30).contains(&prefix) {
            return Err(config_err(format!(
                "--server-ip prefix /{prefix} is not usable for DHCP (need /1../30)"
            )));
        }
        validate_unicast("--server-ip", server_ip)?;

        let mask = prefix_to_mask(prefix);
        let network = network_of(server_ip, prefix);
        let broadcast = broadcast_of(network, prefix);
        if server_ip == network || server_ip == broadcast {
            return Err(config_err(format!(
                "--server-ip {server_ip}/{prefix} is the network or broadcast address of {network}/{prefix}"
            )));
        }

        let gateway = cli.gateway.unwrap_or(server_ip);
        validate_unicast("--gateway", gateway)?;
        if !in_subnet(gateway, network, mask) {
            return Err(config_err(format!(
                "--gateway {gateway} is not inside {network}/{prefix}"
            )));
        }
        if gateway == network || gateway == broadcast {
            return Err(config_err(format!(
                "--gateway {gateway} is the network or broadcast address"
            )));
        }

        let mut dns: Vec<Ipv4Addr> = Vec::new();
        for ip in cli.dns {
            validate_unicast("--dns", ip)?;
            if !dns.contains(&ip) {
                dns.push(ip);
            }
        }

        let mut statics: Vec<(MacAddr, Ipv4Addr)> = Vec::with_capacity(cli.statics.len());
        let mut seen_macs = HashSet::new();
        let mut seen_ips = HashSet::new();
        for binding in cli.statics {
            validate_static(&binding, network, broadcast, server_ip, gateway, prefix)?;
            if !seen_macs.insert(binding.mac) {
                return Err(config_err(format!(
                    "--static: duplicate MAC address {}",
                    binding.mac
                )));
            }
            if !seen_ips.insert(binding.ip) {
                return Err(config_err(format!(
                    "--static: duplicate static IP address {}",
                    binding.ip
                )));
            }
            statics.push((binding.mac, binding.ip));
        }

        let mut reserved = HashSet::new();
        reserved.insert(network);
        reserved.insert(broadcast);
        reserved.insert(server_ip);
        reserved.insert(gateway);
        for ip in &dns {
            if in_subnet(*ip, network, mask) {
                reserved.insert(*ip);
            }
        }

        let (pool_start, pool_end) = match cli.pool {
            Some(range) => {
                validate_explicit_pool(
                    range, network, broadcast, mask, prefix, server_ip, gateway,
                )?;
                (range.start, range.end)
            }
            None => {
                // Derive [network+1, broadcast-1], then trim reserved
                // addresses from the edges so logs/JSON show the usable
                // bounds (Design.md §3.3).
                let mut start = u32::from(network) + 1;
                let mut end = u32::from(broadcast) - 1;
                while start <= end && reserved.contains(&Ipv4Addr::from(start)) {
                    start += 1;
                }
                while end >= start && reserved.contains(&Ipv4Addr::from(end)) {
                    end -= 1;
                }
                if start > end {
                    return Err(config_err(format!(
                        "derived pool for {network}/{prefix} has no usable addresses after \
                         excluding the server, gateway and in-subnet DNS servers; pass --pool"
                    )));
                }
                (Ipv4Addr::from(start), Ipv4Addr::from(end))
            }
        };

        let mut blocked = reserved.clone();
        blocked.extend(statics.iter().map(|(_, ip)| *ip));
        let range_size = u64::from(u32::from(pool_end)) - u64::from(u32::from(pool_start)) + 1;
        let blocked_in_range = blocked
            .iter()
            .filter(|ip| pool_start <= **ip && **ip <= pool_end)
            .count() as u64;
        if blocked_in_range >= range_size {
            return Err(config_err(format!(
                "pool {pool_start}-{pool_end} has no usable addresses after excluding the \
                 server, gateway, in-subnet DNS servers and static bindings; use a larger --pool"
            )));
        }

        let lease_secs = cli.lease.0;
        if lease_secs == 0 {
            return Err(config_err("--lease must be greater than zero"));
        }
        if lease_secs > u64::from(u32::MAX) {
            return Err(config_err(format!(
                "--lease is too long: at most {} seconds are supported",
                u32::MAX
            )));
        }
        let lease_secs = lease_secs as u32;
        let lease = Duration::from_secs(lease_secs as u64);

        if let Some(path) = &cli.lease_file {
            validate_lease_file(path)?;
        }

        Ok(Config {
            iface: cli.iface,
            server_ip,
            prefix,
            mask,
            network,
            broadcast,
            pool: (pool_start, pool_end),
            lease,
            lease_secs,
            gateway: Some(gateway),
            dns,
            statics,
            reserved,
            cleanup_on_exit: cli.cleanup_on_exit,
            no_config_addr: cli.no_config_addr,
            log_dhcp: cli.log_dhcp,
            strict_unicast: cli.strict_unicast,
            lease_file: cli.lease_file,
        })
    }

    /// `10.99.99.1/24`
    pub fn server_cidr(&self) -> String {
        format!("{}/{}", self.server_ip, self.prefix)
    }

    /// `10.99.99.2-10.99.99.254`
    pub fn pool_display(&self) -> String {
        format!("{}-{}", self.pool.0, self.pool.1)
    }

    /// T1 = lease/2, T2 = 7/8 lease. Omitted for very short leases where the
    /// RFC ordering (T1 < T2 < lease) cannot be satisfied.
    pub fn t1_t2(&self) -> Option<(u32, u32)> {
        let lease = self.lease_secs;
        if lease < 3 {
            return None;
        }
        let t1 = (lease / 2).max(1);
        let t2 = ((u64::from(lease) * 7 / 8) as u32)
            .max(t1 + 1)
            .min(lease - 1);
        Some((t1, t2))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn config_err(message: impl Into<String>) -> AppError {
    AppError::Config(message.into())
}

fn validate_iface(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(config_err("--iface must not be empty"));
    }
    // IFNAMSIZ is 16 including the trailing NUL.
    if name.len() >= 16 {
        return Err(config_err(format!(
            "--iface `{name}` is too long (Linux interface names are at most 15 bytes)"
        )));
    }
    if name
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '/')
    {
        return Err(config_err(format!(
            "--iface `{name}` contains invalid characters"
        )));
    }
    Ok(())
}

fn validate_unicast(label: &str, ip: Ipv4Addr) -> Result<()> {
    if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
        return Err(config_err(format!(
            "{label} {ip} is not a usable unicast IPv4 address"
        )));
    }
    if ip.is_loopback() {
        return Err(config_err(format!(
            "{label} {ip} is a loopback address; use a real network address"
        )));
    }
    Ok(())
}

fn validate_static(
    binding: &StaticBinding,
    network: Ipv4Addr,
    broadcast: Ipv4Addr,
    server_ip: Ipv4Addr,
    gateway: Ipv4Addr,
    prefix: u8,
) -> Result<()> {
    if binding.mac.is_zero() || binding.mac.is_multicast() {
        return Err(config_err(format!(
            "--static {}: MAC must be a non-zero unicast address",
            binding.mac
        )));
    }
    if !in_subnet(binding.ip, network, prefix_to_mask(prefix)) {
        return Err(config_err(format!(
            "--static {}={}: IP is not inside {network}/{prefix}",
            binding.mac, binding.ip
        )));
    }
    if binding.ip == network || binding.ip == broadcast {
        return Err(config_err(format!(
            "--static {}={}: IP is the network or broadcast address",
            binding.mac, binding.ip
        )));
    }
    if binding.ip == server_ip {
        return Err(config_err(format!(
            "--static {}={}: IP is the server address",
            binding.mac, binding.ip
        )));
    }
    if binding.ip == gateway {
        return Err(config_err(format!(
            "--static {}={}: IP is the gateway address",
            binding.mac, binding.ip
        )));
    }
    Ok(())
}

fn validate_explicit_pool(
    range: PoolRange,
    network: Ipv4Addr,
    broadcast: Ipv4Addr,
    mask: Ipv4Addr,
    prefix: u8,
    server_ip: Ipv4Addr,
    gateway: Ipv4Addr,
) -> Result<()> {
    for ip in [range.start, range.end] {
        if !in_subnet(ip, network, mask) {
            return Err(config_err(format!(
                "--pool {range}: address {ip} is not inside {network}/{prefix}"
            )));
        }
        if ip == network || ip == broadcast {
            return Err(config_err(format!(
                "--pool {range}: address {ip} is the network or broadcast address"
            )));
        }
    }

    let overlaps = [
        ("server address", server_ip),
        ("gateway", gateway),
        ("network address", network),
        ("broadcast address", broadcast),
    ];
    for (label, ip) in overlaps {
        if range.start <= ip && ip <= range.end {
            return Err(config_err(format!(
                "--pool {range} overlaps the {label} {ip}"
            )));
        }
    }
    Ok(())
}

fn validate_lease_file(path: &std::path::Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(config_err("--lease-file must not be empty"));
    }
    if path.is_dir() {
        return Err(config_err(format!(
            "--lease-file {} is a directory",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(config_err(format!(
                "--lease-file parent directory {} does not exist",
                parent.display()
            )));
        }
    }
    Ok(())
}

pub fn prefix_to_mask(prefix: u8) -> Ipv4Addr {
    debug_assert!(prefix <= 32);
    let bits = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    Ipv4Addr::from(bits)
}

pub fn network_of(ip: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) & u32::from(prefix_to_mask(prefix)))
}

pub fn broadcast_of(network: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(network) | !u32::from(prefix_to_mask(prefix)))
}

pub fn in_subnet(ip: Ipv4Addr, network: Ipv4Addr, mask: Ipv4Addr) -> bool {
    u32::from(ip) & u32::from(mask) == u32::from(network)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cli() -> Cli {
        Cli {
            iface: "eth1".into(),
            server_ip: "10.99.99.1/24".parse().unwrap(),
            pool: None,
            lease: LeaseDuration(12 * 3_600),
            gateway: None,
            dns: Vec::new(),
            statics: Vec::new(),
            cleanup_on_exit: false,
            no_config_addr: false,
            log_dhcp: false,
            foreground: false,
            strict_unicast: false,
            lease_file: None,
        }
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_cidr() {
        let cidr: Cidr = "10.99.99.1/24".parse().unwrap();
        assert_eq!(cidr.addr, ip("10.99.99.1"));
        assert_eq!(cidr.prefix, 24);
        assert!("10.99.99.1".parse::<Cidr>().is_err());
        assert!("10.99.99.1/33".parse::<Cidr>().is_err());
        assert!("300.1.1.1/24".parse::<Cidr>().is_err());
    }

    #[test]
    fn parses_pool_range() {
        let range: PoolRange = "10.99.99.100-10.99.99.150".parse().unwrap();
        assert_eq!(range.start, ip("10.99.99.100"));
        assert_eq!(range.end, ip("10.99.99.150"));
        assert!("10.99.99.150-10.99.99.100".parse::<PoolRange>().is_err());
        assert!("10.99.99.1".parse::<PoolRange>().is_err());
    }

    #[test]
    fn parses_lease_durations() {
        assert_eq!("30m".parse::<LeaseDuration>().unwrap().0, 1_800);
        assert_eq!("12h".parse::<LeaseDuration>().unwrap().0, 43_200);
        assert_eq!("1d".parse::<LeaseDuration>().unwrap().0, 86_400);
        assert_eq!("90s".parse::<LeaseDuration>().unwrap().0, 90);
        assert_eq!("1h30m".parse::<LeaseDuration>().unwrap().0, 5_400);
        assert_eq!("45".parse::<LeaseDuration>().unwrap().0, 45);
        assert!("".parse::<LeaseDuration>().is_err());
        assert!("12x".parse::<LeaseDuration>().is_err());
        assert!("1h30".parse::<LeaseDuration>().is_err());
    }

    #[test]
    fn derives_default_pool() {
        let config = Config::from_cli(base_cli()).unwrap();
        assert_eq!(config.pool, (ip("10.99.99.2"), ip("10.99.99.254")));
        assert!(config.reserved.contains(&ip("10.99.99.1")));
        assert!(config.reserved.contains(&ip("10.99.99.0")));
        assert!(config.reserved.contains(&ip("10.99.99.255")));
        assert_eq!(config.gateway, Some(ip("10.99.99.1")));
    }

    #[test]
    fn derives_pool_and_reserves_gateway_dns() {
        let mut cli = base_cli();
        cli.gateway = Some(ip("10.99.99.254"));
        cli.dns = vec![ip("10.99.99.253"), ip("1.1.1.1")];
        let config = Config::from_cli(cli).unwrap();
        assert_eq!(config.pool, (ip("10.99.99.2"), ip("10.99.99.252")));
        assert!(config.reserved.contains(&ip("10.99.99.254")));
        assert!(config.reserved.contains(&ip("10.99.99.253")));
        assert!(!config.reserved.contains(&ip("1.1.1.1")));
    }

    #[test]
    fn explicit_pool_overlapping_server_is_rejected() {
        let mut cli = base_cli();
        cli.pool = Some("10.99.99.1-10.99.99.10".parse().unwrap());
        let err = Config::from_cli(cli).unwrap_err();
        assert!(err.to_string().contains("overlaps the server address"));
    }

    #[test]
    fn explicit_pool_outside_subnet_is_rejected() {
        let mut cli = base_cli();
        cli.pool = Some("10.99.98.100-10.99.98.110".parse().unwrap());
        assert!(Config::from_cli(cli).is_err());
    }

    #[test]
    fn explicit_pool_must_have_usable_addresses() {
        let mut cli = base_cli();
        cli.pool = Some("10.99.99.1-10.99.99.1".parse().unwrap());
        assert!(Config::from_cli(cli).is_err());
    }

    #[test]
    fn rejects_unusable_prefixes() {
        for prefix in ["31", "32"] {
            let mut cli = base_cli();
            cli.server_ip = format!("10.99.99.1/{prefix}").parse().unwrap();
            assert!(Config::from_cli(cli).is_err(), "/{prefix} must be rejected");
        }
    }

    #[test]
    fn static_bindings_are_validated() {
        let mut cli = base_cli();
        cli.statics = vec!["02:00:00:00:00:01=10.99.99.50".parse().unwrap()];
        let config = Config::from_cli(cli).unwrap();
        assert_eq!(config.statics[0].1, ip("10.99.99.50"));

        let mut cli = base_cli();
        cli.statics = vec!["02:00:00:00:00:01=10.99.98.50".parse().unwrap()];
        assert!(Config::from_cli(cli).is_err());

        let mut cli = base_cli();
        cli.statics = vec![
            "02:00:00:00:00:01=10.99.99.50".parse().unwrap(),
            "02:00:00:00:00:02=10.99.99.50".parse().unwrap(),
        ];
        assert!(Config::from_cli(cli).is_err());

        let mut cli = base_cli();
        cli.statics = vec!["ff:ff:ff:ff:ff:ff=10.99.99.50".parse().unwrap()];
        assert!(Config::from_cli(cli).is_err());
    }

    #[test]
    fn lease_file_parent_is_checked() {
        let mut cli = base_cli();
        cli.lease_file = Some("/definitely/not/a/dir/leases.json".into());
        assert!(Config::from_cli(cli).is_err());

        let mut cli = base_cli();
        cli.lease_file = Some(std::env::temp_dir().join("leases.json"));
        assert!(Config::from_cli(cli).is_ok());
    }

    #[test]
    fn t1_t2_follow_rfc_ordering() {
        let mut cli = base_cli();
        cli.lease = LeaseDuration(3_600);
        let config = Config::from_cli(cli).unwrap();
        assert_eq!(config.t1_t2(), Some((1_800, 3_150)));

        let mut cli = base_cli();
        cli.lease = LeaseDuration(2);
        let config = Config::from_cli(cli).unwrap();
        assert_eq!(config.t1_t2(), None);
    }

    #[test]
    fn mask_helpers() {
        assert_eq!(prefix_to_mask(24), ip("255.255.255.0"));
        assert_eq!(prefix_to_mask(30), ip("255.255.255.252"));
        assert_eq!(prefix_to_mask(8), ip("255.0.0.0"));
        assert_eq!(network_of(ip("10.99.99.42"), 24), ip("10.99.99.0"));
        assert_eq!(broadcast_of(ip("10.99.99.0"), 24), ip("10.99.99.255"));
        assert!(in_subnet(
            ip("10.99.99.42"),
            ip("10.99.99.0"),
            ip("255.255.255.0")
        ));
        assert!(!in_subnet(
            ip("10.99.98.42"),
            ip("10.99.99.0"),
            ip("255.255.255.0")
        ));
    }

    #[test]
    fn gateway_outside_subnet_is_rejected() {
        let mut cli = base_cli();
        cli.gateway = Some(ip("192.168.1.1"));
        assert!(Config::from_cli(cli).is_err());
    }

    #[test]
    fn loopback_server_is_rejected() {
        let mut cli = base_cli();
        cli.server_ip = "127.0.0.1/24".parse().unwrap();
        assert!(Config::from_cli(cli).is_err());
    }
}
