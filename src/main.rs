//! CLI entry point: validate config, prepare the interface, serve DHCP.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use dhcp_mock::caps;
use dhcp_mock::config::{Cli, Config, format_duration};
use dhcp_mock::dhcp::SERVER_PORT;
use dhcp_mock::dhcp::server::{Server, Signals};
use dhcp_mock::dhcp::socket::create_dhcp_socket;
use dhcp_mock::error::Result;
use dhcp_mock::netlink::Netlink;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.log_dhcp);

    let config = match Config::from_cli(cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("dhcp-mock: {error}");
            return ExitCode::from(error.exit_code());
        }
    };

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("dhcp-mock: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn init_logging(log_dhcp: bool) {
    // netlink-packet-route warns about newer kernel attributes we do not use;
    // keep that noise out of the tool's logs.
    let default_filter = if log_dhcp {
        "warn,netlink_packet_route=error,dhcp_mock=debug,dhcp=debug,lease=info"
    } else {
        "warn,netlink_packet_route=error,dhcp_mock=info,dhcp=info,lease=info"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

async fn run(config: Config) -> Result<()> {
    caps::check_required()?;
    let signals = Signals::new()?;

    let netlink = Netlink::new()?;
    let ifindex = netlink.ifindex(&config.iface).await?;

    let link_changed = netlink.ensure_link_up(ifindex).await?;
    let gateway = config
        .gateway
        .map(|gateway| gateway.to_string())
        .unwrap_or_else(|| "-".to_string());
    info!(
        iface = %config.iface,
        ifindex,
        server = %config.server_cidr(),
        pool = %config.pool_display(),
        lease = %format_duration(config.lease_secs as u64),
        gateway = %gateway,
        dns = ?config.dns,
        statics = config.statics.len(),
        "starting"
    );
    if link_changed {
        info!(iface = %config.iface, "link was down, brought it up");
    }

    let existing = netlink.list_ipv4(ifindex).await?;
    let exact = existing
        .iter()
        .any(|(ip, prefix)| *ip == config.server_ip && *prefix == config.prefix);
    let mut addr_added = false;

    if config.no_config_addr {
        if !exact {
            warn!(
                iface = %config.iface,
                server = %config.server_cidr(),
                addresses = ?existing.iter().map(|(ip, prefix)| format!("{ip}/{prefix}")).collect::<Vec<_>>(),
                "--no-config-addr is set but the server address is not present on the interface"
            );
        }
    } else if exact {
        info!(addr = %config.server_cidr(), "server address already present, reusing it");
    } else {
        netlink
            .add_addr(ifindex, config.server_ip, config.prefix)
            .await?;
        addr_added = true;
        info!(addr = %config.server_cidr(), "address added to interface");
    }

    let started_at = Instant::now();
    let mut server = Server::new(config.clone(), ifindex, started_at);

    if let Some(path) = config.lease_file.as_ref() {
        if let Err(error) = server.write_snapshot(path) {
            rollback_address(&netlink, &config, ifindex, addr_added).await;
            return Err(error);
        }
        info!(path = %path.display(), "lease snapshot enabled");
    }

    let socket = match create_dhcp_socket(&config.iface) {
        Ok(socket) => socket,
        Err(error) => {
            rollback_address(&netlink, &config, ifindex, addr_added).await;
            return Err(error);
        }
    };

    info!(
        target: "dhcp",
        "[DHCP] listening on {}:{}  server={}  pool={}  lease={}  gateway={}",
        config.iface,
        SERVER_PORT,
        config.server_cidr(),
        config.pool_display(),
        format_duration(u64::from(config.lease_secs)),
        gateway
    );

    let result = server.run(socket, signals).await;

    if config.cleanup_on_exit && addr_added {
        match netlink
            .del_addr(ifindex, config.server_ip, config.prefix)
            .await
        {
            Ok(()) => info!(addr = %config.server_cidr(), "address removed on exit"),
            Err(error) => warn!(
                addr = %config.server_cidr(),
                error = %error,
                "failed to remove address on exit"
            ),
        }
    } else if addr_added {
        info!(
            addr = %config.server_cidr(),
            "address kept after exit (use --cleanup-on-exit to remove it)"
        );
    }

    result
}

/// If startup fails after we added the address, leave the system as we found
/// it. (A normal exit keeps it unless `--cleanup-on-exit` was given.)
async fn rollback_address(netlink: &Netlink, config: &Config, ifindex: u32, added: bool) {
    if !added {
        return;
    }
    match netlink
        .del_addr(ifindex, config.server_ip, config.prefix)
        .await
    {
        Ok(()) => info!(
            addr = %config.server_cidr(),
            "startup failed; removed the address added by this tool"
        ),
        Err(error) => warn!(
            addr = %config.server_cidr(),
            error = %error,
            "startup failed; could not remove the address added by this tool"
        ),
    }
}
