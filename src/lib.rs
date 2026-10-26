//! A minimal DHCPv4 mock server for debugging directly attached devices.
//!
//! The tool configures a single interface (link up + IPv4 address) through
//! rtnetlink, then serves DHCPv4 on UDP/67 bound to that interface with
//! `SO_BINDTODEVICE`. See `Design.md` for the full design.

#[cfg(not(target_os = "linux"))]
compile_error!("dhcp-mock only supports Linux");

pub mod caps;
pub mod config;
pub mod dhcp;
pub mod error;
pub mod netlink;
