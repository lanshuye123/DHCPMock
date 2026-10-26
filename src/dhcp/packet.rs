//! BOOTP/DHCPv4 packet encoding and decoding.
//!
//! The wire format is fixed (RFC 2131 §2), so this is written by hand instead
//! of pulling in a packet-framework dependency.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::dhcp::options::{DhcpOption, MsgType, OPT_END, OPT_MESSAGE_TYPE, OPT_PAD};

pub const BOOTREQUEST: u8 = 1;
pub const BOOTREPLY: u8 = 2;

const HEADER_LEN: usize = 236;
const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

/// RFC 2131 requires DHCP messages to be at least 300 bytes (BOOTP 236 +
/// cookie + 60 bytes of options); shorter replies confuse some clients.
pub const MIN_MESSAGE_LEN: usize = 300;

pub const BROADCAST_FLAG: u16 = 0x8000;

// ---------------------------------------------------------------------------
// MAC address
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self.0 == [0; 6]
    }

    pub fn is_multicast(self) -> bool {
        self.0[0] & 0x01 != 0
    }

    pub fn is_broadcast(self) -> bool {
        self.0 == [0xff; 6]
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for MacAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if !s
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '-')
        {
            return Err(format!("invalid MAC address `{s}`"));
        }
        let hex: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
        if hex.len() != 12 {
            return Err(format!(
                "invalid MAC address `{s}`: expected 6 octets (12 hex digits)"
            ));
        }
        let mut octets = [0u8; 6];
        for (index, octet) in octets.iter_mut().enumerate() {
            let start = index * 2;
            *octet = u8::from_str_radix(&hex[start..start + 2], 16)
                .map_err(|_| format!("invalid MAC address `{s}`"))?;
        }
        Ok(MacAddr(octets))
    }
}

impl Serialize for MacAddr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Packet
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootp {
    pub op: u8,
    pub htype: u8,
    pub hlen: u8,
    pub hops: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ipv4Addr,
    pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr,
    pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub options: Vec<DhcpOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("packet is too short ({0} bytes)")]
    TooShort(usize),
    #[error("bad DHCP magic cookie")]
    BadCookie,
    #[error("truncated option {code}: need {need} bytes, have {have}")]
    TruncatedOption { code: u8, need: usize, have: usize },
}

impl Bootp {
    /// Build a client-side DHCP request skeleton (Ethernet, xid, MAC).
    pub fn new_request(mac: MacAddr, xid: u32) -> Self {
        let mut chaddr = [0u8; 16];
        chaddr[..6].copy_from_slice(&mac.octets());
        Self {
            op: BOOTREQUEST,
            htype: 1,
            hlen: 6,
            hops: 0,
            xid,
            secs: 0,
            flags: 0,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: Ipv4Addr::UNSPECIFIED,
            chaddr,
            options: Vec::new(),
        }
    }

    /// Build a server reply that echoes the request's identity fields.
    pub fn new_reply(request: &Bootp, yiaddr: Ipv4Addr) -> Self {
        Self {
            op: BOOTREPLY,
            htype: request.htype,
            hlen: request.hlen,
            hops: 0,
            xid: request.xid,
            secs: 0,
            flags: request.flags,
            ciaddr: Ipv4Addr::UNSPECIFIED,
            yiaddr,
            siaddr: Ipv4Addr::UNSPECIFIED,
            giaddr: request.giaddr,
            chaddr: request.chaddr,
            options: Vec::new(),
        }
    }

    pub fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        if buf.len() < HEADER_LEN + MAGIC_COOKIE.len() {
            return Err(ParseError::TooShort(buf.len()));
        }
        if buf[HEADER_LEN..HEADER_LEN + 4] != MAGIC_COOKIE {
            return Err(ParseError::BadCookie);
        }

        let mut chaddr = [0u8; 16];
        chaddr.copy_from_slice(&buf[28..44]);

        let mut packet = Bootp {
            op: buf[0],
            htype: buf[1],
            hlen: buf[2],
            hops: buf[3],
            xid: u32::from_be_bytes(buf[4..8].try_into().expect("slice length")),
            secs: u16::from_be_bytes(buf[8..10].try_into().expect("slice length")),
            flags: u16::from_be_bytes(buf[10..12].try_into().expect("slice length")),
            ciaddr: ipv4_at(buf, 12),
            yiaddr: ipv4_at(buf, 16),
            siaddr: ipv4_at(buf, 20),
            giaddr: ipv4_at(buf, 24),
            chaddr,
            options: Vec::new(),
        };

        let mut offset = HEADER_LEN + 4;
        while offset < buf.len() {
            let code = buf[offset];
            offset += 1;
            match code {
                OPT_PAD => continue,
                OPT_END => break,
                _ => {}
            }
            if offset >= buf.len() {
                return Err(ParseError::TruncatedOption {
                    code,
                    need: 1,
                    have: 0,
                });
            }
            let len = buf[offset] as usize;
            offset += 1;
            let have = buf.len() - offset;
            if have < len {
                return Err(ParseError::TruncatedOption {
                    code,
                    need: len,
                    have,
                });
            }
            packet
                .options
                .push(DhcpOption::new(code, &buf[offset..offset + len]));
            offset += len;
        }

        Ok(packet)
    }

    /// Serialize to wire format, appending END and padding to 300 bytes.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER_LEN + 4];
        out[0] = self.op;
        out[1] = self.htype;
        out[2] = self.hlen;
        out[3] = self.hops;
        out[4..8].copy_from_slice(&self.xid.to_be_bytes());
        out[8..10].copy_from_slice(&self.secs.to_be_bytes());
        out[10..12].copy_from_slice(&self.flags.to_be_bytes());
        out[12..16].copy_from_slice(&self.ciaddr.octets());
        out[16..20].copy_from_slice(&self.yiaddr.octets());
        out[20..24].copy_from_slice(&self.siaddr.octets());
        out[24..28].copy_from_slice(&self.giaddr.octets());
        out[28..44].copy_from_slice(&self.chaddr);
        out[HEADER_LEN..HEADER_LEN + 4].copy_from_slice(&MAGIC_COOKIE);

        for option in &self.options {
            if option.code == OPT_PAD || option.code == OPT_END {
                continue;
            }
            if option.data.is_empty() {
                out.push(option.code);
                out.push(0);
                continue;
            }
            // RFC 3396: long options may be split; our generated options are
            // well below 255 bytes, but be defensive.
            for chunk in option.data.chunks(255) {
                out.push(option.code);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out.push(OPT_END);
        if out.len() < MIN_MESSAGE_LEN {
            out.resize(MIN_MESSAGE_LEN, 0);
        }
        out
    }

    pub fn mac(&self) -> MacAddr {
        let mut octets = [0u8; 6];
        octets.copy_from_slice(&self.chaddr[..6]);
        MacAddr(octets)
    }

    pub fn msg_type(&self) -> Option<MsgType> {
        match self.option(OPT_MESSAGE_TYPE)? {
            [code] => MsgType::from_code(*code),
            _ => None,
        }
    }

    pub fn wants_broadcast(&self) -> bool {
        self.flags & BROADCAST_FLAG != 0
    }

    /// First occurrence of an option's data.
    pub fn option(&self, code: u8) -> Option<&[u8]> {
        self.options
            .iter()
            .find(|option| option.code == code)
            .map(|option| option.data.as_slice())
    }

    pub fn options_by_code(&self, code: u8) -> impl Iterator<Item = &[u8]> {
        self.options
            .iter()
            .filter(move |option| option.code == code)
            .map(|option| option.data.as_slice())
    }

    pub fn ipv4_option(&self, code: u8) -> Option<Ipv4Addr> {
        self.option(code)
            .filter(|data| data.len() == 4)
            .map(ipv4_from_slice)
    }

    /// Concatenate repeated options of the same code (RFC 3396).
    pub fn ipv4_list_option(&self, code: u8) -> Vec<Ipv4Addr> {
        let mut data = Vec::new();
        for chunk in self.options_by_code(code) {
            data.extend_from_slice(chunk);
        }
        data.chunks_exact(4).map(ipv4_from_slice).collect()
    }

    pub fn requested_ip(&self) -> Option<Ipv4Addr> {
        self.ipv4_option(crate::dhcp::options::OPT_REQUESTED_IP)
    }

    pub fn server_id(&self) -> Option<Ipv4Addr> {
        self.ipv4_option(crate::dhcp::options::OPT_SERVER_ID)
    }

    pub fn lease_time(&self) -> Option<u32> {
        self.option(crate::dhcp::options::OPT_LEASE_TIME)
            .filter(|data| data.len() == 4)
            .map(|data| u32::from_be_bytes(data.try_into().expect("slice length")))
    }

    pub fn hostname(&self) -> Option<String> {
        self.option(crate::dhcp::options::OPT_HOSTNAME)
            .map(|data| String::from_utf8_lossy(data).into_owned())
    }

    pub fn push_option(&mut self, option: DhcpOption) {
        self.options.push(option);
    }
}

fn ipv4_at(buf: &[u8], offset: usize) -> Ipv4Addr {
    Ipv4Addr::new(
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    )
}

fn ipv4_from_slice(data: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(data[0], data[1], data[2], data[3])
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dhcp::options::{
        OPT_CLIENT_ID, OPT_DNS, OPT_LEASE_TIME, OPT_REQUESTED_IP, OPT_SERVER_ID,
    };

    fn mac() -> MacAddr {
        "aa:bb:cc:dd:ee:ff".parse().unwrap()
    }

    #[test]
    fn mac_parsing_and_display() {
        assert_eq!(mac().to_string(), "aa:bb:cc:dd:ee:ff");
        assert_eq!(
            "AA-BB-CC-DD-EE-FF".parse::<MacAddr>().unwrap(),
            MacAddr([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert_eq!(
            "aabbccddeeff".parse::<MacAddr>().unwrap(),
            MacAddr([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        assert!("aa:bb:cc:dd:ee".parse::<MacAddr>().is_err());
        assert!("aa:bb:cc:dd:ee:gg".parse::<MacAddr>().is_err());
        assert!(!mac().is_multicast());
        assert!(
            "01:00:5e:00:00:01"
                .parse::<MacAddr>()
                .unwrap()
                .is_multicast()
        );
        assert!(
            "ff:ff:ff:ff:ff:ff"
                .parse::<MacAddr>()
                .unwrap()
                .is_broadcast()
        );
    }

    fn sample_request() -> Bootp {
        let mut packet = Bootp::new_request(mac(), 0x1a2b_3c4d);
        packet.flags = BROADCAST_FLAG;
        packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Discover.code()));
        packet.push_option(DhcpOption::new(
            OPT_CLIENT_ID,
            [1, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
        ));
        packet.push_option(DhcpOption::ipv4(
            OPT_REQUESTED_IP,
            Ipv4Addr::new(10, 0, 0, 5),
        ));
        packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, Ipv4Addr::new(10, 0, 0, 1)));
        packet.push_option(DhcpOption::u32(OPT_LEASE_TIME, 3_600));
        packet
    }

    #[test]
    fn round_trips_request() {
        let request = sample_request();
        let bytes = request.serialize();
        assert!(bytes.len() >= MIN_MESSAGE_LEN);
        assert_eq!(&bytes[236..240], &MAGIC_COOKIE);
        let parsed = Bootp::parse(&bytes).unwrap();
        assert_eq!(parsed.op, BOOTREQUEST);
        assert_eq!(parsed.xid, 0x1a2b_3c4d);
        assert_eq!(parsed.flags, BROADCAST_FLAG);
        assert!(parsed.wants_broadcast());
        assert_eq!(parsed.mac(), mac());
        assert_eq!(parsed.msg_type(), Some(MsgType::Discover));
        assert_eq!(parsed.requested_ip(), Some(Ipv4Addr::new(10, 0, 0, 5)));
        assert_eq!(parsed.server_id(), Some(Ipv4Addr::new(10, 0, 0, 1)));
        assert_eq!(parsed.lease_time(), Some(3_600));
        assert_eq!(parsed.options, request.options);
    }

    #[test]
    fn parses_padding_between_options() {
        let mut bytes = sample_request().serialize();
        // Insert a pad byte right after the cookie.
        bytes.insert(240, OPT_PAD);
        let parsed = Bootp::parse(&bytes).unwrap();
        assert_eq!(parsed.msg_type(), Some(MsgType::Discover));
    }

    #[test]
    fn rejects_short_packets_and_bad_cookie() {
        assert!(matches!(
            Bootp::parse(&[0u8; 10]),
            Err(ParseError::TooShort(10))
        ));
        let mut bytes = sample_request().serialize();
        bytes[236] = 0;
        assert_eq!(Bootp::parse(&bytes), Err(ParseError::BadCookie));
    }

    #[test]
    fn rejects_truncated_option() {
        let mut bytes = sample_request().serialize();
        bytes.truncate(242);
        // Option code at 240 with a length that runs past the buffer.
        bytes[240] = 53;
        bytes[241] = 5;
        assert!(matches!(
            Bootp::parse(&bytes),
            Err(ParseError::TruncatedOption {
                code: 53,
                need: 5,
                ..
            })
        ));
    }

    #[test]
    fn concatenates_split_options() {
        let mut packet = sample_request();
        packet.options.retain(|o| o.code != OPT_DNS);
        packet.push_option(
            DhcpOption::ipv4_list(
                OPT_DNS,
                &[Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)],
            )[0]
            .clone(),
        );
        packet.push_option(DhcpOption::ipv4(OPT_DNS, Ipv4Addr::new(9, 9, 9, 9)));
        let parsed = Bootp::parse(&packet.serialize()).unwrap();
        assert_eq!(
            parsed.ipv4_list_option(OPT_DNS),
            vec![
                Ipv4Addr::new(1, 1, 1, 1),
                Ipv4Addr::new(8, 8, 8, 8),
                Ipv4Addr::new(9, 9, 9, 9)
            ]
        );
    }

    #[test]
    fn reply_echoes_identity() {
        let request = sample_request();
        let mut reply = Bootp::new_reply(&request, Ipv4Addr::new(10, 0, 0, 5));
        reply.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Offer.code()));
        assert_eq!(reply.op, BOOTREPLY);
        assert_eq!(reply.xid, request.xid);
        assert_eq!(reply.chaddr, request.chaddr);
        assert_eq!(reply.flags, request.flags);
        assert_eq!(reply.yiaddr, Ipv4Addr::new(10, 0, 0, 5));
        let parsed = Bootp::parse(&reply.serialize()).unwrap();
        assert_eq!(parsed.msg_type(), Some(MsgType::Offer));
    }
}
