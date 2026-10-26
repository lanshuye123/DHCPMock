//! UDP/67 socket creation with `SO_BINDTODEVICE`.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket as StdUdpSocket};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::dhcp::SERVER_PORT;
use crate::error::{AppError, Result};

/// Create a non-blocking UDP socket bound to `0.0.0.0:67` and to `iface`.
///
/// `SO_BINDTODEVICE` keeps the server from ever seeing or emitting DHCP
/// traffic on other interfaces (Design.md §10.1).
pub fn create_dhcp_socket(iface: &str) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;
    socket.set_broadcast(true)?;
    socket.bind_device(Some(iface.as_bytes())).map_err(|e| {
        if is_permission(&e) {
            AppError::Permission(format!(
                "SO_BINDTODEVICE({iface}) failed: {e}; missing CAP_NET_RAW?"
            ))
        } else {
            AppError::Io(e)
        }
    })?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SERVER_PORT).into())
        .map_err(|e| {
            if is_permission(&e) {
                AppError::Permission(format!(
                    "binding UDP port {SERVER_PORT} failed: {e}; missing CAP_NET_BIND_SERVICE?"
                ))
            } else {
                AppError::Runtime(format!("bind 0.0.0.0:{SERVER_PORT}: {e}"))
            }
        })?;

    socket.set_nonblocking(true)?;
    let std_socket: StdUdpSocket = socket.into();
    UdpSocket::from_std(std_socket)
        .map_err(|e| AppError::Runtime(format!("register UDP socket with tokio: {e}")))
}

fn is_permission(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == libc::EPERM || code == libc::EACCES
    )
}
