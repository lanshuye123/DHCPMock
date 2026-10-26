//! Address pool, lease table and allocation policy.
//!
//! All lease state is in memory only (see `Design.md` §1.3). Methods that
//! reason about time take an explicit `now: Instant` so tests can advance the
//! clock without sleeping.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use crate::dhcp::packet::MacAddr;

/// How long a DISCOVER reservation (OFFER) stays valid.
pub const OFFER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a DECLINEd address is kept out of rotation.
pub const DECLINE_COOLDOWN: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Offered,
    Bound,
    Declined,
}

impl fmt::Display for LeaseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LeaseState::Offered => "offered",
            LeaseState::Bound => "bound",
            LeaseState::Declined => "declined",
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub expires_at: Instant,
    pub state: LeaseState,
}

/// Why an offer or a confirmation failed. Also used as the NAK reason text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    Exhausted,
    NotInPool(Ipv4Addr),
    Reserved(Ipv4Addr),
    Declined { ip: Ipv4Addr, retry_in: Duration },
    Conflict { ip: Ipv4Addr, owner: MacAddr },
    WrongAddress { offered: Ipv4Addr },
    NoLease,
    StaticOwnedByOther { ip: Ipv4Addr, owner: MacAddr },
    StaticConflict { ip: Ipv4Addr, owner: MacAddr },
    StaticDeclined { ip: Ipv4Addr, retry_in: Duration },
    NotStaticAddress { expected: Ipv4Addr },
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::Exhausted => f.write_str("address pool exhausted"),
            PoolError::NotInPool(ip) => write!(f, "{ip} is not in the address pool"),
            PoolError::Reserved(ip) => write!(f, "{ip} is reserved"),
            PoolError::Declined { ip, retry_in } => {
                write!(
                    f,
                    "{ip} was declined and is cooling down for {}s",
                    retry_in.as_secs()
                )
            }
            PoolError::Conflict { ip, owner } => {
                write!(f, "{ip} is already leased to {owner}")
            }
            PoolError::WrongAddress { offered } => {
                write!(f, "client was offered {offered}, not the requested address")
            }
            PoolError::NoLease => f.write_str("no offer or lease exists for this client"),
            PoolError::StaticOwnedByOther { ip, owner } => {
                write!(f, "{ip} is statically bound to {owner}")
            }
            PoolError::StaticConflict { ip, owner } => {
                write!(f, "static address {ip} is currently claimed by {owner}")
            }
            PoolError::StaticDeclined { ip, retry_in } => {
                write!(
                    f,
                    "static address {ip} was declined; retry in {}s",
                    retry_in.as_secs()
                )
            }
            PoolError::NotStaticAddress { expected } => {
                write!(f, "this client is statically bound to {expected}")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LeaseView {
    pub mac: MacAddr,
    pub ip: Ipv4Addr,
    pub state: LeaseState,
    pub expires_in: Duration,
}

#[derive(Debug, Clone)]
pub struct DeclinedView {
    pub ip: Ipv4Addr,
    pub cooldown: Duration,
}

pub struct Pool {
    range: (Ipv4Addr, Ipv4Addr),
    reserved: HashSet<Ipv4Addr>,
    statics: BTreeMap<MacAddr, Ipv4Addr>,
    static_ips: HashSet<Ipv4Addr>,
    leases: HashMap<MacAddr, Lease>,
    assigned: HashMap<Ipv4Addr, MacAddr>,
    declined: HashMap<Ipv4Addr, Instant>,
    /// Round-robin cursor for the next allocation attempt.
    cursor: u32,
    lease_duration: Duration,
    offer_timeout: Duration,
    decline_cooldown: Duration,
}

impl Pool {
    pub fn new(
        range: (Ipv4Addr, Ipv4Addr),
        reserved: HashSet<Ipv4Addr>,
        statics: Vec<(MacAddr, Ipv4Addr)>,
        lease_duration: Duration,
    ) -> Self {
        let mut static_map = BTreeMap::new();
        let mut static_ips = HashSet::new();
        for (mac, ip) in statics {
            static_map.insert(mac, ip);
            static_ips.insert(ip);
        }
        Self {
            range,
            reserved,
            statics: static_map,
            static_ips,
            leases: HashMap::new(),
            assigned: HashMap::new(),
            declined: HashMap::new(),
            cursor: u32::from(range.0),
            lease_duration,
            offer_timeout: OFFER_TIMEOUT,
            decline_cooldown: DECLINE_COOLDOWN,
        }
    }

    pub fn range(&self) -> (Ipv4Addr, Ipv4Addr) {
        self.range
    }

    pub fn len(&self) -> usize {
        self.leases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    pub fn lookup(&self, mac: MacAddr) -> Option<&Lease> {
        self.leases.get(&mac)
    }

    /// Pick an address for a DISCOVER. Reserves it for `OFFER_TIMEOUT`.
    pub fn offer(&mut self, mac: MacAddr, now: Instant) -> Result<Ipv4Addr, PoolError> {
        self.sweep(now);

        // 1. Static binding.
        if let Some(&ip) = self.statics.get(&mac) {
            if let Some(&owner) = self.assigned.get(&ip) {
                if owner != mac {
                    return Err(PoolError::StaticConflict { ip, owner });
                }
            }
            if let Some(&until) = self.declined.get(&ip) {
                return Err(PoolError::StaticDeclined {
                    ip,
                    retry_in: until.saturating_duration_since(now),
                });
            }
            self.insert_lease(mac, ip, now + self.offer_timeout, LeaseState::Offered);
            return Ok(ip);
        }

        // 2. Existing reservation/lease for this client.
        if let Some(existing) = self.leases.get(&mac).copied() {
            match existing.state {
                LeaseState::Bound => return Ok(existing.ip),
                LeaseState::Offered => {
                    self.leases.insert(
                        mac,
                        Lease {
                            expires_at: now + self.offer_timeout,
                            ..existing
                        },
                    );
                    return Ok(existing.ip);
                }
                LeaseState::Declined => {}
            }
        }

        // 3. Fresh address.
        let ip = self.find_free().ok_or(PoolError::Exhausted)?;
        self.insert_lease(mac, ip, now + self.offer_timeout, LeaseState::Offered);
        Ok(ip)
    }

    /// Validate a REQUEST and turn the offer into a bound lease.
    pub fn confirm(
        &mut self,
        mac: MacAddr,
        requested: Ipv4Addr,
        now: Instant,
    ) -> Result<(), PoolError> {
        self.sweep(now);

        // Static bindings are authoritative: ACK even for INIT-REBOOT without
        // a prior DISCOVER, but only for the bound address.
        if let Some(&static_ip) = self.statics.get(&mac) {
            if requested != static_ip {
                return Err(PoolError::NotStaticAddress {
                    expected: static_ip,
                });
            }
            if let Some(&owner) = self.assigned.get(&requested) {
                if owner != mac {
                    return Err(PoolError::StaticConflict {
                        ip: requested,
                        owner,
                    });
                }
            }
            if let Some(&until) = self.declined.get(&requested) {
                return Err(PoolError::StaticDeclined {
                    ip: requested,
                    retry_in: until.saturating_duration_since(now),
                });
            }
            self.insert_lease(mac, requested, now + self.lease_duration, LeaseState::Bound);
            return Ok(());
        }

        if !self.in_range(requested) {
            return Err(PoolError::NotInPool(requested));
        }
        if self.reserved.contains(&requested) {
            return Err(PoolError::Reserved(requested));
        }
        if self.static_ips.contains(&requested) {
            let owner = self
                .statics
                .iter()
                .find(|(_, ip)| **ip == requested)
                .map(|(mac, _)| *mac)
                .unwrap_or(mac);
            return Err(PoolError::StaticOwnedByOther {
                ip: requested,
                owner,
            });
        }
        if let Some(&owner) = self.assigned.get(&requested) {
            if owner != mac {
                return Err(PoolError::Conflict {
                    ip: requested,
                    owner,
                });
            }
        }
        if let Some(&until) = self.declined.get(&requested) {
            return Err(PoolError::Declined {
                ip: requested,
                retry_in: until.saturating_duration_since(now),
            });
        }

        match self.leases.get(&mac).copied() {
            Some(lease) if lease.ip == requested => {
                self.insert_lease(mac, requested, now + self.lease_duration, LeaseState::Bound);
                Ok(())
            }
            Some(lease) => Err(PoolError::WrongAddress { offered: lease.ip }),
            None => Err(PoolError::NoLease),
        }
    }

    /// Release a client's lease (DHCPRELEASE). Returns the released lease.
    pub fn release(&mut self, mac: MacAddr) -> Option<Lease> {
        let lease = self.leases.remove(&mac)?;
        if self.assigned.get(&lease.ip) == Some(&mac) {
            self.assigned.remove(&lease.ip);
        }
        Some(lease)
    }

    /// Mark an address as conflicted (DHCPDECLINE). Returns true if the
    /// address belongs to this pool.
    pub fn decline(&mut self, mac: MacAddr, ip: Ipv4Addr, now: Instant) -> bool {
        if !(self.in_range(ip) || self.static_ips.contains(&ip)) {
            return false;
        }
        let owned_by_client = self
            .leases
            .get(&mac)
            .map(|lease| lease.ip == ip)
            .unwrap_or(false);
        if owned_by_client {
            self.leases.remove(&mac);
        }
        if self.assigned.get(&ip) == Some(&mac) {
            self.assigned.remove(&ip);
        }
        self.declined.insert(ip, now + self.decline_cooldown);
        true
    }

    /// Drop expired offers/leases and expired decline cooldowns.
    /// Returns true if anything changed.
    pub fn sweep(&mut self, now: Instant) -> bool {
        let mut changed = false;
        let expired: Vec<MacAddr> = self
            .leases
            .iter()
            .filter(|(_, lease)| lease.expires_at <= now)
            .map(|(mac, _)| *mac)
            .collect();
        for mac in expired {
            if let Some(lease) = self.leases.remove(&mac) {
                if self.assigned.get(&lease.ip) == Some(&mac) {
                    self.assigned.remove(&lease.ip);
                }
                changed = true;
            }
        }

        let expired_declined: Vec<Ipv4Addr> = self
            .declined
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(ip, _)| *ip)
            .collect();
        for ip in expired_declined {
            self.declined.remove(&ip);
            changed = true;
        }
        changed
    }

    /// Snapshot of leases and declined addresses for logging/JSON.
    pub fn views(&self, now: Instant) -> (Vec<LeaseView>, Vec<DeclinedView>) {
        let mut leases: Vec<LeaseView> = self
            .leases
            .iter()
            .map(|(mac, lease)| LeaseView {
                mac: *mac,
                ip: lease.ip,
                state: lease.state,
                expires_in: lease.expires_at.saturating_duration_since(now),
            })
            .collect();
        leases.sort_by_key(|view| u32::from(view.ip));

        let mut declined: Vec<DeclinedView> = self
            .declined
            .iter()
            .map(|(ip, until)| DeclinedView {
                ip: *ip,
                cooldown: until.saturating_duration_since(now),
            })
            .collect();
        declined.sort_by_key(|view| u32::from(view.ip));

        (leases, declined)
    }

    fn find_free(&mut self) -> Option<Ipv4Addr> {
        let start = u32::from(self.range.0);
        let end = u32::from(self.range.1);
        let len = u64::from(end) - u64::from(start) + 1;
        let mut candidate = if self.cursor < start || self.cursor > end {
            start
        } else {
            self.cursor
        };

        for _ in 0..len {
            let ip = Ipv4Addr::from(candidate);
            let free = !self.reserved.contains(&ip)
                && !self.static_ips.contains(&ip)
                && !self.assigned.contains_key(&ip)
                && !self.declined.contains_key(&ip);
            if free {
                self.cursor = if candidate == end {
                    start
                } else {
                    candidate + 1
                };
                return Some(ip);
            }
            candidate = if candidate == end {
                start
            } else {
                candidate + 1
            };
        }
        None
    }

    fn insert_lease(&mut self, mac: MacAddr, ip: Ipv4Addr, expires_at: Instant, state: LeaseState) {
        if let Some(previous) = self.leases.insert(
            mac,
            Lease {
                ip,
                expires_at,
                state,
            },
        ) {
            if previous.ip != ip && self.assigned.get(&previous.ip) == Some(&mac) {
                self.assigned.remove(&previous.ip);
            }
        }
        self.assigned.insert(ip, mac);
    }

    fn in_range(&self, ip: Ipv4Addr) -> bool {
        let ip = u32::from(ip);
        u32::from(self.range.0) <= ip && ip <= u32::from(self.range.1)
    }

    #[cfg(test)]
    fn with_timers(mut self, offer_timeout: Duration, decline_cooldown: Duration) -> Self {
        self.offer_timeout = offer_timeout;
        self.decline_cooldown = decline_cooldown;
        self
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn mac(last: u8) -> MacAddr {
        MacAddr([0x02, 0, 0, 0, 0, last])
    }

    fn pool(start: &str, end: &str, reserved: &[&str], statics: &[(u8, &str)]) -> Pool {
        Pool::new(
            (ip(start), ip(end)),
            reserved.iter().map(|s| ip(s)).collect(),
            statics.iter().map(|(m, s)| (mac(*m), ip(s))).collect(),
            Duration::from_secs(3_600),
        )
    }

    #[test]
    fn allocates_sequentially_and_skips_reserved() {
        let mut pool = pool("10.0.0.1", "10.0.0.5", &["10.0.0.1", "10.0.0.3"], &[]);
        let now = Instant::now();
        assert_eq!(pool.offer(mac(1), now).unwrap(), ip("10.0.0.2"));
        assert_eq!(pool.offer(mac(2), now).unwrap(), ip("10.0.0.4"));
        assert_eq!(pool.offer(mac(3), now).unwrap(), ip("10.0.0.5"));
        assert_eq!(pool.offer(mac(4), now), Err(PoolError::Exhausted));
    }

    #[test]
    fn reuses_offer_and_bound_lease() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[]);
        let now = Instant::now();
        let offered = pool.offer(mac(1), now).unwrap();
        assert_eq!(
            pool.offer(mac(1), now + Duration::from_secs(10)).unwrap(),
            offered
        );
        pool.confirm(mac(1), offered, now + Duration::from_secs(20))
            .unwrap();
        assert_eq!(pool.lookup(mac(1)).unwrap().state, LeaseState::Bound);
        assert_eq!(
            pool.offer(mac(1), now + Duration::from_secs(30)).unwrap(),
            offered
        );
    }

    #[test]
    fn request_without_offer_is_naked() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[]);
        let now = Instant::now();
        assert_eq!(
            pool.confirm(mac(1), ip("10.0.0.10"), now),
            Err(PoolError::NoLease)
        );
    }

    #[test]
    fn request_for_wrong_offered_address_is_rejected() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[]);
        let now = Instant::now();
        let offered = pool.offer(mac(1), now).unwrap();
        assert_eq!(
            pool.confirm(mac(1), Ipv4Addr::from(u32::from(offered) + 1), now),
            Err(PoolError::WrongAddress { offered })
        );
    }

    #[test]
    fn conflict_with_another_client_is_rejected() {
        let mut pool = pool("10.0.0.10", "10.0.0.11", &[], &[]);
        let now = Instant::now();
        let a = pool.offer(mac(1), now).unwrap();
        pool.offer(mac(2), now).unwrap();
        assert!(matches!(
            pool.confirm(mac(2), a, now),
            Err(PoolError::Conflict { .. })
        ));
    }

    #[test]
    fn offered_lease_expires() {
        let mut pool = pool("10.0.0.10", "10.0.0.10", &[], &[])
            .with_timers(Duration::from_secs(60), Duration::from_secs(300));
        let now = Instant::now();
        assert_eq!(pool.offer(mac(1), now).unwrap(), ip("10.0.0.10"));
        assert_eq!(pool.offer(mac(2), now), Err(PoolError::Exhausted));
        assert!(pool.sweep(now + Duration::from_secs(61)));
        assert_eq!(
            pool.offer(mac(2), now + Duration::from_secs(61)).unwrap(),
            ip("10.0.0.10")
        );
    }

    #[test]
    fn release_frees_the_address() {
        let mut pool = pool("10.0.0.10", "10.0.0.10", &[], &[]);
        let now = Instant::now();
        let leased = pool.offer(mac(1), now).unwrap();
        pool.confirm(mac(1), leased, now).unwrap();
        assert!(pool.release(mac(1)).is_some());
        assert!(pool.lookup(mac(1)).is_none());
        assert_eq!(pool.offer(mac(2), now).unwrap(), ip("10.0.0.10"));
    }

    #[test]
    fn declined_address_cools_down() {
        let mut pool = pool("10.0.0.10", "10.0.0.10", &[], &[])
            .with_timers(Duration::from_secs(60), Duration::from_secs(300));
        let now = Instant::now();
        let leased = pool.offer(mac(1), now).unwrap();
        assert!(pool.decline(mac(1), leased, now));
        assert_eq!(pool.offer(mac(2), now), Err(PoolError::Exhausted));
        assert!(pool.sweep(now + Duration::from_secs(301)));
        assert_eq!(
            pool.offer(mac(2), now + Duration::from_secs(301)).unwrap(),
            leased
        );
    }

    #[test]
    fn decline_outside_pool_is_ignored() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[]);
        assert!(!pool.decline(mac(1), ip("10.0.0.99"), Instant::now()));
    }

    #[test]
    fn static_binding_inside_pool_is_reserved() {
        let mut pool = pool("10.0.0.10", "10.0.0.12", &[], &[(9, "10.0.0.10")]);
        let now = Instant::now();
        assert_eq!(pool.offer(mac(9), now).unwrap(), ip("10.0.0.10"));
        assert_eq!(pool.offer(mac(1), now).unwrap(), ip("10.0.0.11"));
        assert_eq!(
            pool.confirm(mac(1), ip("10.0.0.10"), now),
            Err(PoolError::StaticOwnedByOther {
                ip: ip("10.0.0.10"),
                owner: mac(9)
            })
        );
    }

    #[test]
    fn static_binding_outside_pool_works() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[(9, "10.0.0.200")]);
        let now = Instant::now();
        assert_eq!(pool.offer(mac(9), now).unwrap(), ip("10.0.0.200"));
        pool.confirm(mac(9), ip("10.0.0.200"), now).unwrap();
        assert_eq!(pool.lookup(mac(9)).unwrap().state, LeaseState::Bound);
    }

    #[test]
    fn static_request_without_offer_is_acked() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[(9, "10.0.0.200")]);
        let now = Instant::now();
        pool.confirm(mac(9), ip("10.0.0.200"), now).unwrap();
        assert_eq!(pool.lookup(mac(9)).unwrap().state, LeaseState::Bound);
    }

    #[test]
    fn static_client_requesting_other_address_is_rejected() {
        let mut pool = pool("10.0.0.10", "10.0.0.20", &[], &[(9, "10.0.0.200")]);
        assert_eq!(
            pool.confirm(mac(9), ip("10.0.0.10"), Instant::now()),
            Err(PoolError::NotStaticAddress {
                expected: ip("10.0.0.200")
            })
        );
    }

    #[test]
    fn bound_lease_expires_and_address_is_reused() {
        let mut pool = pool("10.0.0.10", "10.0.0.10", &[], &[])
            .with_timers(Duration::from_secs(60), Duration::from_secs(300));
        let now = Instant::now();
        let leased = pool.offer(mac(1), now).unwrap();
        pool.confirm(mac(1), leased, now).unwrap();
        assert!(!pool.sweep(now + Duration::from_secs(3_599)));
        assert!(pool.sweep(now + Duration::from_secs(3_601)));
        assert_eq!(
            pool.offer(mac(2), now + Duration::from_secs(3_601))
                .unwrap(),
            leased
        );
    }

    #[test]
    fn views_are_sorted_by_ip() {
        let mut pool = pool("10.0.0.10", "10.0.0.12", &[], &[]);
        let now = Instant::now();
        let first = pool.offer(mac(2), now).unwrap();
        pool.confirm(mac(2), first, now).unwrap();
        let second = pool.offer(mac(1), now).unwrap();
        let (leases, _) = pool.views(now);
        assert_eq!(leases.len(), 2);
        assert!(leases[0].ip < leases[1].ip);
        assert_eq!(leases[0].state, LeaseState::Bound);
        assert_eq!(leases[1].state, LeaseState::Offered);
        assert_eq!(second, leases[1].ip);
    }
}
