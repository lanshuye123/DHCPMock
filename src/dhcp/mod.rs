//! DHCPv4 server modules.

pub mod options;
pub mod packet;
pub mod pool;
pub mod report;
pub mod server;
pub mod socket;

/// DHCP server port.
pub const SERVER_PORT: u16 = 67;
/// DHCP client port.
pub const CLIENT_PORT: u16 = 68;
