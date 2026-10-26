//! Startup capability check.
//!
//! The tool needs, at minimum:
//! - `CAP_NET_ADMIN`       — netlink link/address changes
//! - `CAP_NET_RAW`         — `SO_BINDTODEVICE`
//! - `CAP_NET_BIND_SERVICE`— bind UDP/67 below 1024
//!
//! File capabilities (`setcap`) are honoured because we inspect `CapEff`.

use std::fs;

use crate::error::{AppError, Result};

const CAP_NET_BIND_SERVICE: u32 = 10;
const CAP_NET_ADMIN: u32 = 12;
const CAP_NET_RAW: u32 = 13;

const REQUIRED: [(u32, &str); 3] = [
    (CAP_NET_ADMIN, "CAP_NET_ADMIN"),
    (CAP_NET_RAW, "CAP_NET_RAW"),
    (CAP_NET_BIND_SERVICE, "CAP_NET_BIND_SERVICE"),
];

/// Verify the process holds the capabilities required by the tool.
///
/// Running as root normally satisfies this; running with `setcap` also does.
/// Missing capabilities produce an `AppError::Permission` (exit code 3).
pub fn check_required() -> Result<()> {
    let cap_eff = effective_capabilities()?;
    let missing: Vec<&str> = REQUIRED
        .iter()
        .filter(|(bit, _)| cap_eff & (1u64 << bit) == 0)
        .map(|(_, name)| *name)
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<path-to-dhcp-mock>".to_string());
    Err(AppError::Permission(format!(
        "missing {}; run as root or grant the capabilities with:\n  \
         sudo setcap cap_net_admin,cap_net_raw,cap_net_bind_service+ep {executable}",
        missing.join(", ")
    )))
}

fn effective_capabilities() -> Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .map(str::trim)
        .ok_or_else(|| AppError::Permission("cannot read CapEff from /proc/self/status".into()))?;
    u64::from_str_radix(value, 16)
        .map_err(|e| AppError::Permission(format!("cannot parse CapEff `{value}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_capabilities_are_in_range() {
        for (bit, _) in REQUIRED {
            assert!(bit < 64);
            assert_ne!(1u64 << bit, 0);
        }
    }
}
