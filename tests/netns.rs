//! End-to-end tests in an isolated two-namespace topology.
//!
//! The outer test re-executes itself once per scenario under
//! `unshare -Urn` (unprivileged user + network namespace). Inside, two more
//! network namespaces are created with `unshare -n` holders and connected by
//! a veth pair; the server under test runs in one, and the test client runs in
//! the other (the test thread switches with `setns(2)`).
//!
//! The test client is a plain UDP socket bound to `0.0.0.0:68`, so it needs an
//! IPv4 address on the client interface to receive broadcasts. `10.99.99.2/24`
//! is only a harness address: the DHCP payload still has `ciaddr = 0`, exactly
//! like a client with no address. Real clients (`udhcpc`, `dhclient`) use raw
//! packet sockets and do not need this; see `scripts/netns-demo.sh`.
//!
//! Requires `unshare`, `nsenter`, `ip` and unprivileged user namespaces.
//! Set `DHCP_MOCK_SKIP_NETNS=1` to skip these tests.

use std::env;
use std::fs::{self, File};
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use dhcp_mock::dhcp::options::{
    DhcpOption, MsgType, OPT_DNS, OPT_MESSAGE_TYPE, OPT_REQUESTED_IP, OPT_ROUTER, OPT_SERVER_ID,
    OPT_SUBNET_MASK,
};
use dhcp_mock::dhcp::packet::{Bootp, MacAddr};

const SERVER_IFACE: &str = "veth-s";
const CLIENT_IFACE: &str = "veth-c";
const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 99, 99, 1);
const HARNESS_IP: &str = "10.99.99.2/24";
const SERVER_CIDR: &str = "10.99.99.1/24";
const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;

const SCENARIOS: &[&str] = &[
    "default_pool",
    "basic",
    "static",
    "exhaustion",
    "decline",
    "cleanup",
    "inform",
    "wrong_server",
];

// ---------------------------------------------------------------------------
// Test entry point / scenario dispatch
// ---------------------------------------------------------------------------

#[test]
fn netns_end_to_end() {
    if let Ok(scenario) = env::var("DHCP_MOCK_NS_SCENARIO") {
        run_scenario(&scenario);
        return;
    }

    if env::var_os("DHCP_MOCK_SKIP_NETNS").is_some() {
        eprintln!("DHCP_MOCK_SKIP_NETNS is set; skipping netns integration tests");
        return;
    }

    let preflight = Command::new("unshare").args(["-Urn", "true"]).status();
    match preflight {
        Ok(status) if status.success() => {}
        other => panic!(
            "cannot create unprivileged user+net namespaces ({other:?}); \
             run as root or set DHCP_MOCK_SKIP_NETNS=1 to skip"
        ),
    }

    let exe = env::current_exe().expect("current_exe");
    for scenario in SCENARIOS {
        eprintln!("=== netns scenario: {scenario} ===");
        let output = Command::new("timeout")
            .args(["120", "unshare", "-Urn", "--fork", "--kill-child"])
            .arg(&exe)
            .args(["--exact", "netns_end_to_end", "--nocapture"])
            .env("DHCP_MOCK_NS_SCENARIO", scenario)
            .output()
            .expect("run scenario under unshare");

        if !output.status.success() {
            eprintln!(
                "--- scenario `{scenario}` failed ({}) ---\n\
                 child stdout:\n{}\nchild stderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            panic!("scenario `{scenario}` failed");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("{stderr}");
        }
    }
}

fn run_scenario(name: &str) {
    let env = NetnsEnv::new();
    match name {
        "default_pool" => scenario_default_pool(&env),
        "basic" => scenario_basic(&env),
        "static" => scenario_static(&env),
        "exhaustion" => scenario_exhaustion(&env),
        "decline" => scenario_decline(&env),
        "cleanup" => scenario_cleanup(&env),
        "inform" => scenario_inform(&env),
        "wrong_server" => scenario_wrong_server(&env),
        other => panic!("unknown scenario `{other}`"),
    }
}

// ---------------------------------------------------------------------------
// Two-namespace environment
// ---------------------------------------------------------------------------

struct NetnsHolder {
    child: Child,
    pid: u32,
}

impl NetnsHolder {
    fn new() -> Self {
        let mut child = Command::new("unshare")
            .args(["-n", "sleep", "600"])
            .spawn()
            .expect("spawn `unshare -n` holder");
        let pid = child.id();
        let own_inode = fs::metadata("/proc/self/ns/net")
            .expect("stat own netns")
            .ino();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::metadata(format!("/proc/{pid}/ns/net")) {
                Ok(metadata) if metadata.ino() != own_inode => break,
                _ => {}
            }
            if let Some(status) = child.try_wait().expect("try_wait holder") {
                panic!("`unshare -n` holder exited early: {status}");
            }
            if Instant::now() > deadline {
                panic!("`unshare -n` holder {pid} did not enter a new netns");
            }
            sleep(Duration::from_millis(10));
        }
        Self { child, pid }
    }
}

impl Drop for NetnsHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct NetnsEnv {
    server_ns: NetnsHolder,
    client_ns: NetnsHolder,
}

impl NetnsEnv {
    fn new() -> Self {
        let server_ns = NetnsHolder::new();
        let client_ns = NetnsHolder::new();

        run_ip(&[
            "link",
            "add",
            SERVER_IFACE,
            "type",
            "veth",
            "peer",
            "name",
            CLIENT_IFACE,
        ]);
        run_ip(&[
            "link",
            "set",
            SERVER_IFACE,
            "netns",
            &server_ns.pid.to_string(),
        ]);
        run_ip(&[
            "link",
            "set",
            CLIENT_IFACE,
            "netns",
            &client_ns.pid.to_string(),
        ]);

        // The server interface is deliberately left down: dhcp-mock must bring
        // it up and add the server address itself (M1).
        run_in_ns(client_ns.pid, &["ip", "link", "set", CLIENT_IFACE, "up"]);
        run_in_ns(
            client_ns.pid,
            &["ip", "addr", "replace", HARNESS_IP, "dev", CLIENT_IFACE],
        );

        Self {
            server_ns,
            client_ns,
        }
    }

    fn start_server(&self, extra_args: &[&str]) -> ServerGuard {
        ServerGuard::start(self.server_ns.pid, extra_args)
    }

    fn ip_in_server_ns(&self, args: &[&str]) -> String {
        let mut command = Command::new("nsenter");
        command.args(["-t", &self.server_ns.pid.to_string(), "-n", "ip"]);
        command.args(args);
        let output = command.output().expect("run ip in server netns");
        assert!(
            output.status.success(),
            "ip {} in server netns failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn server_iface_has_addr(&self, cidr: &str) -> bool {
        self.ip_in_server_ns(&["-o", "addr", "show", "dev", SERVER_IFACE])
            .contains(cidr)
    }

    fn server_iface_is_up(&self) -> bool {
        let output = self.ip_in_server_ns(&["-o", "link", "show", "dev", SERVER_IFACE]);
        output
            .split_once('<')
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(flags, _)| flags.split(',').any(|flag| flag == "UP"))
            .unwrap_or(false)
    }

    /// Run `f` with the calling thread switched into the client netns.
    fn with_client<F: FnOnce()>(&self, f: F) {
        let saved = File::open("/proc/self/ns/net").expect("open own netns");
        let path = format!("/proc/{}/ns/net", self.client_ns.pid);
        let target = File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
        setns(target.as_raw_fd(), libc::CLONE_NEWNET)
            .unwrap_or_else(|e| panic!("setns into client netns: {e}"));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        setns(saved.as_raw_fd(), libc::CLONE_NEWNET)
            .unwrap_or_else(|e| panic!("restore original netns: {e}"));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }
}

fn setns(fd: i32, nstype: i32) -> Result<(), std::io::Error> {
    let rc = unsafe { libc::setns(fd, nstype) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn run_ip(args: &[&str]) {
    let output = Command::new("ip").args(args).output().expect("run ip");
    assert!(
        output.status.success(),
        "ip {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_in_ns(pid: u32, args: &[&str]) {
    let output = Command::new("nsenter")
        .args(["-t", &pid.to_string(), "-n"])
        .args(args)
        .output()
        .expect("run nsenter");
    assert!(
        output.status.success(),
        "`{}` in netns {pid} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Server process under test
// ---------------------------------------------------------------------------

struct ServerGuard {
    child: Child,
    log_path: PathBuf,
}

impl ServerGuard {
    fn start(netns_pid: u32, extra_args: &[&str]) -> Self {
        let log_path = temp_path("server", "log");
        let log = File::create(&log_path).expect("create server log file");

        let child = Command::new("nsenter")
            .args(["-t", &netns_pid.to_string(), "-n"])
            .arg(env!("CARGO_BIN_EXE_dhcp-mock"))
            .args(["--iface", SERVER_IFACE])
            .args(extra_args)
            .stdout(Stdio::from(log.try_clone().expect("clone log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn dhcp-mock");

        let mut guard = Self { child, log_path };
        guard.wait_ready();
        guard
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let log = self.log();
            if log.contains("listening on") {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("try_wait server") {
                panic!("dhcp-mock exited before listening ({status}); log:\n{log}");
            }
            if Instant::now() > deadline {
                panic!("dhcp-mock did not become ready; log:\n{log}");
            }
            sleep(Duration::from_millis(20));
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    fn wait_log_contains(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let log = self.log();
            if log.contains(needle) {
                return log;
            }
            if Instant::now() > deadline {
                panic!("log did not contain `{needle}`; log:\n{log}");
            }
            sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, signal: i32) {
        let rc = unsafe { libc::kill(self.child.id() as i32, signal) };
        assert_eq!(
            rc,
            0,
            "kill({signal}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    fn terminate(mut self) -> (ExitStatus, String) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let status = wait_timeout(&mut self.child, Duration::from_secs(5));
        (status, self.log())
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn wait_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            return child.wait().expect("wait after kill");
        }
        sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

fn scenario_default_pool(env: &NetnsEnv) {
    // Deliberately without --log-dhcp: exchanges must still be visible.
    let guard = env.start_server(&["--server-ip", SERVER_CIDR]);

    assert!(
        env.server_iface_is_up(),
        "dhcp-mock did not bring {SERVER_IFACE} up"
    );
    assert!(
        env.server_iface_has_addr(SERVER_CIDR),
        "dhcp-mock did not add {SERVER_CIDR} to {SERVER_IFACE}"
    );

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);
        let mac = mac("02:00:00:00:00:01");
        let offer = discover(&socket, mac, 0x1001);
        assert_eq!(offer.op, 2);
        assert_eq!(offer.msg_type(), Some(MsgType::Offer));
        // Default pool is derived from 10.99.99.1/24 and excludes the server.
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 99, 99, 2));
        assert_eq!(offer.ipv4_option(OPT_SERVER_ID), Some(SERVER_IP));
        assert_eq!(
            offer.ipv4_option(OPT_SUBNET_MASK),
            Some(Ipv4Addr::new(255, 255, 255, 0))
        );
        assert_eq!(offer.ipv4_option(OPT_ROUTER), Some(SERVER_IP));
        assert_eq!(offer.lease_time(), Some(12 * 3_600));

        let ack = request_selecting(&socket, mac, 0x1002, offer.yiaddr);
        assert_eq!(ack.yiaddr, offer.yiaddr);
        assert_eq!(ack.msg_type(), Some(MsgType::Ack));
    });

    // Without --lease-file, SIGUSR1 must dump the JSON snapshot to the log.
    guard.signal(libc::SIGUSR1);
    guard.wait_log_contains("schema_version", Duration::from_secs(3));

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(log.contains("listening on"), "log:\n{log}");
    assert!(
        log.contains("\"leases\""),
        "SIGUSR1 snapshot missing; log:\n{log}"
    );
    assert!(
        log.contains("[DHCP] 02:00:00:00:00:01"),
        "default console echo is missing the client MAC; log:\n{log}"
    );
    assert!(
        log.contains("DISCOVER -> OFFER") && log.contains("10.99.99.2"),
        "default console echo is missing DISCOVER/OFFER; log:\n{log}"
    );
    assert!(
        log.contains("REQUEST  -> ACK"),
        "default console echo is missing the ACK; log:\n{log}"
    );
}

fn scenario_basic(env: &NetnsEnv) {
    let lease_file = temp_path("leases", "json");
    let lease_arg = lease_file.to_string_lossy().into_owned();

    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.110",
        "--lease",
        "2h",
        "--dns",
        "1.1.1.1,8.8.8.8",
        "--lease-file",
        &lease_arg,
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);
        let mac = mac("02:00:00:00:00:10");

        let offer = discover(&socket, mac, 0x2001);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 99, 99, 100));
        assert_eq!(offer.lease_time(), Some(2 * 3_600));
        assert_eq!(
            offer.ipv4_list_option(OPT_DNS),
            vec![Ipv4Addr::new(1, 1, 1, 1), Ipv4Addr::new(8, 8, 8, 8)]
        );
        wait_lease(&lease_file, mac, "offered");

        let ack = request_selecting(&socket, mac, 0x2002, offer.yiaddr);
        assert_eq!(ack.msg_type(), Some(MsgType::Ack));
        assert_eq!(ack.yiaddr, offer.yiaddr);
        wait_lease(&lease_file, mac, "bound");

        // SIGUSR1 must rewrite the snapshot even if the file disappeared.
        fs::remove_file(&lease_file).expect("remove lease snapshot");
        guard.signal(libc::SIGUSR1);
        wait_for_file(&lease_file);
        wait_lease(&lease_file, mac, "bound");

        // The client "configures" the leased address, then renews via unicast.
        drop(socket);
        run_ip(&["addr", "replace", "10.99.99.100/24", "dev", CLIENT_IFACE]);
        let renew_socket = client_socket(Ipv4Addr::new(10, 99, 99, 100));
        let renewed = request_renew(&renew_socket, mac, 0x2003, Ipv4Addr::new(10, 99, 99, 100));
        assert_eq!(renewed.msg_type(), Some(MsgType::Ack));
        assert_eq!(renewed.yiaddr, Ipv4Addr::new(10, 99, 99, 100));

        // M3 acceptance: the leased address is usable on the link.
        let ping = Command::new("ping")
            .args(["-c", "1", "-W", "1", "-I", "10.99.99.100", "10.99.99.1"])
            .output()
            .expect("run ping");
        assert!(
            ping.status.success(),
            "ping from the leased address failed:\n{}\n{}",
            String::from_utf8_lossy(&ping.stdout),
            String::from_utf8_lossy(&ping.stderr)
        );

        release(&renew_socket, mac, 0x2004, Ipv4Addr::new(10, 99, 99, 100));
        wait_lease_empty(&lease_file);
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("[DHCP] 02:00:00:00:00:10"),
        "console echo is missing the client MAC; log:\n{log}"
    );
    assert!(
        log.contains("REQUEST  -> ACK") && log.contains("10.99.99.100"),
        "console echo is missing the ACK; log:\n{log}"
    );
    assert!(
        log.contains("RELEASE") && log.contains("10.99.99.100"),
        "console echo is missing the RELEASE; log:\n{log}"
    );
}

fn scenario_static(env: &NetnsEnv) {
    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.110",
        "--static",
        "02:00:00:00:00:99=10.99.99.100",
        "--static",
        "02:00:00:00:00:98=10.99.99.150",
        "--static",
        "02:00:00:00:00:97=10.99.99.151",
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);

        // Static address inside the dynamic range.
        let static_mac = mac("02:00:00:00:00:99");
        let offer = discover(&socket, static_mac, 0x3001);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 99, 99, 100));
        let ack = request_selecting(&socket, static_mac, 0x3002, offer.yiaddr);
        assert_eq!(ack.yiaddr, Ipv4Addr::new(10, 99, 99, 100));

        // A dynamic client must skip the statically reserved .100.
        let dynamic_mac = mac("02:00:00:00:00:01");
        let dynamic_offer = discover(&socket, dynamic_mac, 0x3003);
        assert_eq!(dynamic_offer.yiaddr, Ipv4Addr::new(10, 99, 99, 101));

        // Static address outside the dynamic range.
        let outside_mac = mac("02:00:00:00:00:98");
        let outside_offer = discover(&socket, outside_mac, 0x3004);
        assert_eq!(outside_offer.yiaddr, Ipv4Addr::new(10, 99, 99, 150));

        // INIT-REBOOT: a static client may REQUEST without a prior DISCOVER.
        let reboot_mac = mac("02:00:00:00:00:97");
        let reboot_ack =
            request_selecting(&socket, reboot_mac, 0x3005, Ipv4Addr::new(10, 99, 99, 151));
        assert_eq!(reboot_ack.yiaddr, Ipv4Addr::new(10, 99, 99, 151));
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("[DHCP] 02:00:00:00:00:99")
            && log.contains("DISCOVER -> OFFER")
            && log.contains("10.99.99.100"),
        "console echo is missing the static binding; log:\n{log}"
    );
}

fn scenario_exhaustion(env: &NetnsEnv) {
    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.101",
        "--lease",
        "2h",
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);
        assert_eq!(
            discover(&socket, mac("02:00:00:00:00:01"), 0x4001).yiaddr,
            Ipv4Addr::new(10, 99, 99, 100)
        );
        assert_eq!(
            discover(&socket, mac("02:00:00:00:00:02"), 0x4002).yiaddr,
            Ipv4Addr::new(10, 99, 99, 101)
        );
        send_discover(&socket, mac("02:00:00:00:00:03"), 0x4003);
        expect_no_reply(
            &socket,
            mac("02:00:00:00:00:03"),
            0x4003,
            Duration::from_millis(800),
        );
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("address pool exhausted") && log.contains("DISCOVER -> no offer"),
        "expected exhaustion warning on the console; log:\n{log}"
    );
    assert!(
        log.contains("[DHCP] 02:00:00:00:00:03"),
        "exhaustion warning is missing the MAC; log:\n{log}"
    );
}

fn scenario_decline(env: &NetnsEnv) {
    let lease_file = temp_path("decline", "json");
    let lease_arg = lease_file.to_string_lossy().into_owned();
    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.100",
        "--lease-file",
        &lease_arg,
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);
        let client_mac = mac("02:00:00:00:00:20");
        let offer = discover(&socket, client_mac, 0x5001);
        assert_eq!(offer.yiaddr, Ipv4Addr::new(10, 99, 99, 100));

        decline(&socket, client_mac, 0x5002, offer.yiaddr);
        wait_declined(&lease_file, offer.yiaddr);

        // The only address in the pool is cooling down, so no OFFER is sent.
        send_discover(&socket, mac("02:00:00:00:00:21"), 0x5003);
        expect_no_reply(
            &socket,
            mac("02:00:00:00:00:21"),
            0x5003,
            Duration::from_millis(800),
        );
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("DECLINE") && log.contains("cooldown"),
        "expected decline log; log:\n{log}"
    );
    assert!(
        log.contains("[DHCP] 02:00:00:00:00:20"),
        "decline log is missing the MAC; log:\n{log}"
    );
}

fn scenario_cleanup(env: &NetnsEnv) {
    // With --cleanup-on-exit the address must be removed.
    let guard = env.start_server(&["--server-ip", SERVER_CIDR, "--cleanup-on-exit"]);
    assert!(
        env.server_iface_has_addr(SERVER_CIDR),
        "address missing before cleanup test"
    );
    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(log.contains("address removed on exit"), "log:\n{log}");
    assert!(
        !env.server_iface_has_addr(SERVER_CIDR),
        "address should have been removed; log:\n{log}"
    );

    // Without the flag the address is kept.
    let guard = env.start_server(&["--server-ip", SERVER_CIDR]);
    assert!(
        env.server_iface_has_addr(SERVER_CIDR),
        "address missing before keep test"
    );
    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        env.server_iface_has_addr(SERVER_CIDR),
        "address should have been kept; log:\n{log}"
    );
}

fn scenario_inform(env: &NetnsEnv) {
    let lease_file = temp_path("inform", "json");
    let lease_arg = lease_file.to_string_lossy().into_owned();
    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.110",
        "--dns",
        "9.9.9.9",
        "--lease-file",
        &lease_arg,
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let client_ip = Ipv4Addr::new(10, 99, 99, 77);
        run_ip(&["addr", "replace", "10.99.99.77/24", "dev", CLIENT_IFACE]);
        let socket = client_socket(client_ip);
        let mac = mac("02:00:00:00:00:30");
        let ack = inform(&socket, mac, 0x6001, client_ip);

        assert_eq!(ack.msg_type(), Some(MsgType::Ack));
        assert_eq!(ack.yiaddr, Ipv4Addr::UNSPECIFIED);
        assert_eq!(ack.lease_time(), None);
        assert_eq!(
            ack.ipv4_option(OPT_SUBNET_MASK),
            Some(Ipv4Addr::new(255, 255, 255, 0))
        );
        assert_eq!(
            ack.ipv4_list_option(OPT_DNS),
            vec![Ipv4Addr::new(9, 9, 9, 9)]
        );

        // INFORM must not create a lease.
        wait_lease_empty(&lease_file);
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("INFORM") && log.contains("(no lease)"),
        "console echo is missing the INFORM; log:\n{log}"
    );
}

fn scenario_wrong_server(env: &NetnsEnv) {
    let guard = env.start_server(&[
        "--server-ip",
        SERVER_CIDR,
        "--pool",
        "10.99.99.100-10.99.99.110",
        "--log-dhcp",
    ]);

    env.with_client(|| {
        let socket = client_socket(Ipv4Addr::UNSPECIFIED);
        let mac = mac("02:00:00:00:00:40");
        let offer = discover(&socket, mac, 0x7001);
        assert_eq!(offer.msg_type(), Some(MsgType::Offer));

        // A REQUEST that selects a different server must be ignored, not NAKed.
        let mut wrong = Bootp::new_request(mac, 0x7002);
        wrong.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Request.code()));
        wrong.push_option(DhcpOption::ipv4(OPT_REQUESTED_IP, offer.yiaddr));
        wrong.push_option(DhcpOption::ipv4(
            OPT_SERVER_ID,
            Ipv4Addr::new(10, 99, 99, 254),
        ));
        send_to(
            &socket,
            &wrong,
            SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT),
        );
        expect_no_reply(&socket, mac, 0x7002, Duration::from_millis(800));

        // The server must still be alive and offering addresses.
        let next = discover(&socket, mac, 0x7003);
        assert_eq!(next.msg_type(), Some(MsgType::Offer));
        assert!(
            (100..=110).contains(&next.yiaddr.octets()[3]),
            "unexpected offer {}",
            next.yiaddr
        );
    });

    let (status, log) = guard.terminate();
    assert!(status.success(), "server exit status {status}; log:\n{log}");
    assert!(
        log.contains("ignored (client selected server 10.99.99.254)"),
        "console echo is missing the ignored REQUEST; log:\n{log}"
    );
}

// ---------------------------------------------------------------------------
// DHCP client helpers (run inside the client netns)
// ---------------------------------------------------------------------------

fn client_socket(bind_ip: Ipv4Addr) -> UdpSocket {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .expect("create client socket");
    socket.set_reuse_address(true).expect("SO_REUSEADDR");
    socket.set_broadcast(true).expect("SO_BROADCAST");
    socket
        .bind_device(Some(CLIENT_IFACE.as_bytes()))
        .expect("SO_BINDTODEVICE");
    socket
        .bind(&SocketAddr::from((bind_ip, CLIENT_PORT)).into())
        .expect("bind client socket");
    socket
        .set_read_timeout(Some(Duration::from_millis(2_000)))
        .expect("set read timeout");
    socket.into()
}

fn send_discover(socket: &UdpSocket, mac: MacAddr, xid: u32) {
    let mut packet = Bootp::new_request(mac, xid);
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Discover.code()));
    send_to(
        socket,
        &packet,
        SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT),
    );
}

fn discover(socket: &UdpSocket, mac: MacAddr, xid: u32) -> Bootp {
    send_discover(socket, mac, xid);
    recv_reply(socket, xid, mac, MsgType::Offer)
}

fn request_selecting(socket: &UdpSocket, mac: MacAddr, xid: u32, ip: Ipv4Addr) -> Bootp {
    let mut packet = Bootp::new_request(mac, xid);
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Request.code()));
    packet.push_option(DhcpOption::ipv4(OPT_REQUESTED_IP, ip));
    packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, SERVER_IP));
    send_to(
        socket,
        &packet,
        SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT),
    );
    recv_reply(socket, xid, mac, MsgType::Ack)
}

fn request_renew(socket: &UdpSocket, mac: MacAddr, xid: u32, ip: Ipv4Addr) -> Bootp {
    let mut packet = Bootp::new_request(mac, xid);
    packet.ciaddr = ip;
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Request.code()));
    send_to(socket, &packet, SocketAddrV4::new(SERVER_IP, SERVER_PORT));
    recv_reply(socket, xid, mac, MsgType::Ack)
}

fn release(socket: &UdpSocket, mac: MacAddr, xid: u32, ip: Ipv4Addr) {
    let mut packet = Bootp::new_request(mac, xid);
    packet.ciaddr = ip;
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Release.code()));
    packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, SERVER_IP));
    send_to(socket, &packet, SocketAddrV4::new(SERVER_IP, SERVER_PORT));
}

fn decline(socket: &UdpSocket, mac: MacAddr, xid: u32, ip: Ipv4Addr) {
    let mut packet = Bootp::new_request(mac, xid);
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Decline.code()));
    packet.push_option(DhcpOption::ipv4(OPT_REQUESTED_IP, ip));
    packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, SERVER_IP));
    send_to(
        socket,
        &packet,
        SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT),
    );
}

fn inform(socket: &UdpSocket, mac: MacAddr, xid: u32, ip: Ipv4Addr) -> Bootp {
    let mut packet = Bootp::new_request(mac, xid);
    packet.ciaddr = ip;
    packet.push_option(DhcpOption::u8(OPT_MESSAGE_TYPE, MsgType::Inform.code()));
    packet.push_option(DhcpOption::ipv4(OPT_SERVER_ID, SERVER_IP));
    send_to(socket, &packet, SocketAddrV4::new(SERVER_IP, SERVER_PORT));
    recv_reply(socket, xid, mac, MsgType::Ack)
}

fn send_to(socket: &UdpSocket, packet: &Bootp, dest: SocketAddrV4) {
    let bytes = packet.serialize();
    socket
        .send_to(&bytes, dest)
        .unwrap_or_else(|e| panic!("send DHCP packet to {dest}: {e}"));
}

fn recv_reply(socket: &UdpSocket, xid: u32, mac: MacAddr, expected: MsgType) -> Bootp {
    let mut buffer = [0u8; 2048];
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((len, _source)) => {
                let Ok(reply) = Bootp::parse(&buffer[..len]) else {
                    continue;
                };
                if reply.xid == xid && reply.mac() == mac && reply.msg_type() == Some(expected) {
                    return reply;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                panic!("timed out waiting for {expected} (xid 0x{xid:08x}, mac {mac})");
            }
            Err(e) => panic!("recv_from failed: {e}"),
        }
    }
}

fn expect_no_reply(socket: &UdpSocket, mac: MacAddr, xid: u32, wait: Duration) {
    socket
        .set_read_timeout(Some(wait))
        .expect("set read timeout");
    let mut buffer = [0u8; 2048];
    let deadline = Instant::now() + wait + Duration::from_millis(250);
    loop {
        match socket.recv_from(&mut buffer) {
            Ok((len, _source)) => {
                if let Ok(reply) = Bootp::parse(&buffer[..len]) {
                    if reply.xid == xid && reply.mac() == mac {
                        panic!(
                            "expected no reply, got {:?} for xid 0x{xid:08x}",
                            reply.msg_type()
                        );
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
            Err(e) => panic!("recv_from failed: {e}"),
        }
        if Instant::now() > deadline {
            break;
        }
    }
    socket
        .set_read_timeout(Some(Duration::from_millis(2_000)))
        .expect("restore read timeout");
}

// ---------------------------------------------------------------------------
// Lease snapshot helpers
// ---------------------------------------------------------------------------

fn read_snapshot(path: &Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn wait_lease(path: &Path, mac: MacAddr, state: &str) {
    let mac_text = mac.to_string();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(snapshot) = read_snapshot(path) {
            let found = snapshot["leases"]
                .as_array()
                .map(|leases| {
                    leases.iter().any(|lease| {
                        lease["mac"].as_str() == Some(mac_text.as_str())
                            && lease["state"].as_str() == Some(state)
                    })
                })
                .unwrap_or(false);
            if found {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "lease {mac_text} in state `{state}` not found in {}; snapshot: {:?}",
                path.display(),
                fs::read_to_string(path).ok()
            );
        }
        sleep(Duration::from_millis(20));
    }
}

fn wait_lease_empty(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(snapshot) = read_snapshot(path) {
            if snapshot["leases"]
                .as_array()
                .map(|leases| leases.is_empty())
                .unwrap_or(false)
            {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "lease table did not become empty in {}; snapshot: {:?}",
                path.display(),
                fs::read_to_string(path).ok()
            );
        }
        sleep(Duration::from_millis(20));
    }
}

fn wait_declined(path: &Path, ip: Ipv4Addr) {
    let ip_text = ip.to_string();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(snapshot) = read_snapshot(path) {
            let found = snapshot["declined"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .any(|row| row["ip"].as_str() == Some(ip_text.as_str()))
                })
                .unwrap_or(false);
            if found {
                return;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "declined address {ip_text} not found in {}; snapshot: {:?}",
                path.display(),
                fs::read_to_string(path).ok()
            );
        }
        sleep(Duration::from_millis(20));
    }
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        if Instant::now() > deadline {
            panic!("file {} was not created", path.display());
        }
        sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn mac(text: &str) -> MacAddr {
    MacAddr::from_str(text).expect("valid MAC in test")
}

fn temp_path(tag: &str, extension: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    env::temp_dir().join(format!(
        "dhcp-mock-test-{}-{}-{}.{}",
        std::process::id(),
        tag,
        COUNTER.fetch_add(1, Ordering::Relaxed),
        extension
    ))
}
