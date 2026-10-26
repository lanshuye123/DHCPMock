//! Link and address management through rtnetlink (no external commands).

use std::net::{IpAddr, Ipv4Addr};

use futures_util::TryStreamExt;
use rtnetlink::packet_route::{AddressFamily, address::AddressAttribute, link::LinkFlags};
use rtnetlink::{AddressMessageBuilder, Handle, LinkUnspec};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::error::{AppError, Result};

pub struct Netlink {
    handle: Handle,
    _connection: JoinHandle<()>,
}

impl Netlink {
    /// Open a netlink route socket and spawn its driver task.
    pub fn new() -> Result<Self> {
        let (connection, handle, _messages) = rtnetlink::new_connection()
            .map_err(|e| AppError::Netlink(format!("open netlink route socket: {e}")))?;
        let task = tokio::spawn(connection);
        Ok(Self {
            handle,
            _connection: task,
        })
    }

    pub async fn ifindex(&self, name: &str) -> Result<u32> {
        let mut links = self
            .handle
            .link()
            .get()
            .match_name(name.to_string())
            .execute();
        match links.try_next().await {
            Ok(Some(link)) => Ok(link.header.index),
            Ok(None) => Err(AppError::Netlink(format!("interface `{name}` not found"))),
            Err(e) => Err(map_netlink(format!("look up interface `{name}`"), e)),
        }
    }

    /// Bring the link up if it is down. Returns whether anything changed.
    pub async fn ensure_link_up(&self, ifindex: u32) -> Result<bool> {
        let mut links = self.handle.link().get().match_index(ifindex).execute();
        let link = match links.try_next().await {
            Ok(Some(link)) => link,
            Ok(None) => {
                return Err(AppError::Netlink(format!(
                    "interface index {ifindex} not found"
                )));
            }
            Err(e) => return Err(map_netlink(format!("look up link index {ifindex}"), e)),
        };

        if link.header.flags.contains(LinkFlags::Up) {
            return Ok(false);
        }

        self.handle
            .link()
            .change(LinkUnspec::new_with_index(ifindex).up().build())
            .execute()
            .await
            .map_err(|e| map_netlink(format!("bring interface index {ifindex} up"), e))?;
        debug!(ifindex, "netlink: link set up");
        Ok(true)
    }

    /// All IPv4 addresses on the interface as `(address, prefix_len)`.
    pub async fn list_ipv4(&self, ifindex: u32) -> Result<Vec<(Ipv4Addr, u8)>> {
        let mut addresses = self
            .handle
            .address()
            .get()
            .set_link_index_filter(ifindex)
            .execute();
        let mut result = Vec::new();
        while let Some(message) = addresses
            .try_next()
            .await
            .map_err(|e| map_netlink(format!("list IPv4 addresses of index {ifindex}"), e))?
        {
            if message.header.family != AddressFamily::Inet {
                continue;
            }
            // Prefer IFA_LOCAL; IFA_ADDRESS is the peer on point-to-point links.
            let local = message
                .attributes
                .iter()
                .find_map(|attribute| match attribute {
                    AddressAttribute::Local(IpAddr::V4(ip)) => Some(*ip),
                    _ => None,
                });
            let address = local.or_else(|| {
                message
                    .attributes
                    .iter()
                    .find_map(|attribute| match attribute {
                        AddressAttribute::Address(IpAddr::V4(ip)) => Some(*ip),
                        _ => None,
                    })
            });
            if let Some(address) = address {
                result.push((address, message.header.prefix_len));
            }
        }
        Ok(result)
    }

    pub async fn add_addr(&self, ifindex: u32, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        self.handle
            .address()
            .add(ifindex, IpAddr::V4(ip), prefix)
            .replace()
            .execute()
            .await
            .map_err(|e| map_netlink(format!("add address {ip}/{prefix} to index {ifindex}"), e))?;
        debug!(ifindex, addr = %format!("{ip}/{prefix}"), "netlink: address added");
        Ok(())
    }

    pub async fn del_addr(&self, ifindex: u32, ip: Ipv4Addr, prefix: u8) -> Result<()> {
        let message = AddressMessageBuilder::<Ipv4Addr>::new()
            .index(ifindex)
            .address(ip, prefix)
            .build();
        match self.handle.address().del(message).execute().await {
            Ok(()) => {
                debug!(ifindex, addr = %format!("{ip}/{prefix}"), "netlink: address deleted");
                Ok(())
            }
            Err(e) if is_missing(&e) => {
                debug!(ifindex, addr = %format!("{ip}/{prefix}"), "netlink: address already absent");
                Ok(())
            }
            Err(e) => Err(map_netlink(
                format!("delete address {ip}/{prefix} from index {ifindex}"),
                e,
            )),
        }
    }
}

fn netlink_errno(error: &rtnetlink::Error) -> Option<i32> {
    match error {
        rtnetlink::Error::NetlinkError(message) => message.to_io().raw_os_error(),
        _ => None,
    }
}

fn map_netlink(context: String, error: rtnetlink::Error) -> AppError {
    match netlink_errno(&error) {
        Some(code) if code == libc::EPERM || code == libc::EACCES => {
            AppError::Permission(format!("{context}: {error} (missing CAP_NET_ADMIN?)"))
        }
        _ => AppError::Netlink(format!("{context}: {error}")),
    }
}

fn is_missing(error: &rtnetlink::Error) -> bool {
    matches!(
        netlink_errno(error),
        Some(code) if code == libc::ENOENT || code == libc::EADDRNOTAVAIL
    )
}
