//! DHCP option codes, message types and TLV helpers.

use std::fmt;
use std::net::Ipv4Addr;

pub const OPT_PAD: u8 = 0;
pub const OPT_SUBNET_MASK: u8 = 1;
pub const OPT_ROUTER: u8 = 3;
pub const OPT_DNS: u8 = 6;
pub const OPT_HOSTNAME: u8 = 12;
pub const OPT_BROADCAST: u8 = 28;
pub const OPT_REQUESTED_IP: u8 = 50;
pub const OPT_LEASE_TIME: u8 = 51;
pub const OPT_MESSAGE_TYPE: u8 = 53;
pub const OPT_SERVER_ID: u8 = 54;
pub const OPT_MESSAGE: u8 = 56;
pub const OPT_MAX_MESSAGE_SIZE: u8 = 57;
pub const OPT_T1: u8 = 58;
pub const OPT_T2: u8 = 59;
pub const OPT_CLIENT_ID: u8 = 61;
pub const OPT_END: u8 = 255;

/// DHCP message type (option 53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Discover = 1,
    Offer = 2,
    Request = 3,
    Decline = 4,
    Ack = 5,
    Nak = 6,
    Release = 7,
    Inform = 8,
}

impl MsgType {
    pub const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => MsgType::Discover,
            2 => MsgType::Offer,
            3 => MsgType::Request,
            4 => MsgType::Decline,
            5 => MsgType::Ack,
            6 => MsgType::Nak,
            7 => MsgType::Release,
            8 => MsgType::Inform,
            _ => return None,
        })
    }

    pub const fn code(self) -> u8 {
        self as u8
    }

    pub const fn name(self) -> &'static str {
        match self {
            MsgType::Discover => "DISCOVER",
            MsgType::Offer => "OFFER",
            MsgType::Request => "REQUEST",
            MsgType::Decline => "DECLINE",
            MsgType::Ack => "ACK",
            MsgType::Nak => "NAK",
            MsgType::Release => "RELEASE",
            MsgType::Inform => "INFORM",
        }
    }
}

impl fmt::Display for MsgType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `pad` honours width/alignment (e.g. `{:<8}` in the console lines).
        f.pad(self.name())
    }
}

/// A raw DHCP option (TLV without the pad/end markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhcpOption {
    pub code: u8,
    pub data: Vec<u8>,
}

impl DhcpOption {
    pub fn new(code: u8, data: impl Into<Vec<u8>>) -> Self {
        Self {
            code,
            data: data.into(),
        }
    }

    pub fn u8(code: u8, value: u8) -> Self {
        Self::new(code, [value])
    }

    pub fn u32(code: u8, value: u32) -> Self {
        Self::new(code, value.to_be_bytes())
    }

    pub fn ipv4(code: u8, ip: Ipv4Addr) -> Self {
        Self::new(code, ip.octets())
    }

    /// Encode a list of IPv4 addresses, splitting into multiple options if
    /// needed (RFC 3396 concatenation). 63 addresses = 252 bytes per option.
    pub fn ipv4_list(code: u8, ips: &[Ipv4Addr]) -> Vec<Self> {
        const ADDRS_PER_OPTION: usize = 63;
        ips.chunks(ADDRS_PER_OPTION)
            .map(|chunk| {
                let mut data = Vec::with_capacity(chunk.len() * 4);
                for ip in chunk {
                    data.extend_from_slice(&ip.octets());
                }
                Self::new(code, data)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_types_round_trip() {
        for code in 1..=8 {
            let msg = MsgType::from_code(code).unwrap();
            assert_eq!(msg.code(), code);
        }
        assert_eq!(MsgType::from_code(0), None);
        assert_eq!(MsgType::from_code(9), None);
        assert_eq!(MsgType::Discover.to_string(), "DISCOVER");
    }

    #[test]
    fn encodes_ipv4_options() {
        let opt = DhcpOption::ipv4(OPT_SERVER_ID, Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(opt.code, 54);
        assert_eq!(opt.data, [10, 0, 0, 1]);

        let opt = DhcpOption::u32(OPT_LEASE_TIME, 43_200);
        assert_eq!(opt.data, 43_200u32.to_be_bytes());

        let opt = DhcpOption::u8(OPT_MESSAGE_TYPE, 5);
        assert_eq!(opt.data, [5]);
    }

    #[test]
    fn splits_long_ipv4_lists() {
        let ips: Vec<Ipv4Addr> = (0..70).map(|i| Ipv4Addr::new(10, 0, 0, i)).collect();
        let options = DhcpOption::ipv4_list(OPT_DNS, &ips);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].data.len(), 63 * 4);
        assert_eq!(options[1].data.len(), 7 * 4);
        assert_eq!(options[0].code, OPT_DNS);
        assert_eq!(options[1].code, OPT_DNS);
    }
}
