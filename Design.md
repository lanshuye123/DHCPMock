# 调试用 DHCP Mock 工具设计方案

> 文档状态：草案 v0.1  
> 目标平台：Linux（内核 ≥ 5.0）  
> 工具定位：面向调试场景的轻量 DHCPv4 服务器 CLI，通过指定网卡为直连设备（开发板、测试机）分配地址

---

## 1. 概述与目标

### 1.1 背景

调试嵌入式开发板、临时测试机或 PXE 引导设备时，常需要一条“即插即用”的 DHCP 服务。现有方案（`dnsmasq`、`isc-dhcp-server`）功能完整但配置繁琐，且默认行为面向生产网络，缺少针对“单网卡、点对点、可反复启停”调试场景的简洁接口。

### 1.2 目标

开发一个 CLI 工具，实现：

1. 指定一块网卡，工具自动确保该网卡具备可用的 IPv4 地址；
2. 在该网卡上启动一个最小化 DHCPv4 服务器，向直连设备分配地址；
3. 作用域严格限定在指定网卡，不干扰宿主机其他网络；
4. 提供适合调试的参数（地址池、租约时长、静态绑定、退出行为）。

### 1.3 非目标

- 不实现 DHCPv6、DHCP 中继（`giaddr` 转发）、PXE 引导扩展；
- 不做租约持久化（进程退出即丢失，符合调试定位）；
- 不追求生产级高可用与并发性能。

---

## 2. 需求分析

### 2.1 功能性需求

| 编号 | 需求 | 说明 |
|---|---|---|
| F1 | 指定网卡 | 通过 `--iface` 指定，工具只在该接口收发 |
| F2 | 自动配 IP | 网卡无 IPv4 地址时，通过 netlink 添加；已有则复用/覆盖 |
| F3 | 拉起链路 | 若接口处于 down 状态，自动置 up |
| F4 | DHCP 服务 | 处理 DISCOVER/REQUEST/RELEASE/DECLINE/INFORM |
| F5 | 地址池 | 支持显式范围或由服务器 IP/前缀自动派生 |
| F6 | 静态绑定 | 按 MAC 分配固定 IP（可选参数，可重复） |
| F7 | 租约时长 | 可配置，默认 12h |
| F8 | 附加选项 | 网关、DNS、子网掩码按配置下发 |
| F9 | 退出策略 | 默认保留添加的地址；`--cleanup-on-exit` 时删除 |
| F10 | 日志 | 打印每次交互（xid、MAC、分配结果），支持 `--log-dhcp` |

### 2.2 非功能性需求

| 编号 | 需求 | 说明 |
|---|---|---|
| N1 | 权限最小化 | 需要 `CAP_NET_ADMIN`（netlink 配址）、`CAP_NET_RAW`（`SO_BINDTODEVICE`）、`CAP_NET_BIND_SERVICE`（67 端口）；启动时检查并给出明确提示 |
| N2 | 单进程单网卡 | 一次运行只服务一块网卡，避免状态耦合 |
| N3 | 信号处理 | `SIGINT`/`SIGTERM` 优雅退出，按 F9 决定是否清理 |
| N4 | 无外部命令依赖 | 不调用 `ip`/`ifconfig`，全部走系统调用 |
| N5 | 可测试性 | 能在 network namespace + veth 环境中完整验证 |

---

## 3. 技术选型与关键决策

### 3.1 语言与库

**推荐 Rust**，理由：内存安全、错误处理显式、生态覆盖本方案全部系统调用。

| 能力 | Rust crate | C 备选 |
|---|---|---|
| CLI 解析 | `clap` (derive) | `getopt_long` |
| netlink 配址/链路 | `rtnetlink` | `libnl-route` |
| UDP socket | `std::net::UdpSocket` + `socket2`（设置 `SO_BINDTODEVICE`） | 原生 `setsockopt` |
| 报文构造/解析 | 手写（DHCP 结构固定，不引入 `pnet` 的额外抽象） | 手写 |
| 日志 | `tracing` / `env_logger` | `syslog` |
| 异步（可选） | `tokio` | 单线程 `poll`/`epoll` |

> 若团队以 C 为主，架构完全等价，`libnl` 的 `rtnl_addr_add` / `rtnl_link_set_flags` 对应下文 netlink 步骤。

### 3.2 关键决策：不使用 `AF_PACKET`

| 方案 | 前提 | 结论 |
|---|---|---|
| `AF_PACKET` + `SOCK_DGRAM` raw socket | 服务器网卡无 IP，需在内核 IP 栈前拦截 | **不采用** |
| UDP 67 + `SO_BINDTODEVICE` | 服务器网卡有 IP | **采用** |

**理由**：本工具会先通过 netlink 为网卡配置 IP（F2），满足内核向 UDP socket 递交广播包的条件。`AF_PACKET` 引入的手工 IP/UDP 封包、校验和计算、防火墙绕过等复杂度在本场景无收益。Kea 使用 raw socket 是为了服务“尚未有地址的客户端”这一通用场景，与本工具的“先配服务器地址”设计正交。

### 3.3 关键决策：配置驱动的地址池派生

用户只需给出服务器地址（含前缀），地址池默认由网段派生：

```
server = 10.99.99.1/24
network = 10.99.99.0
broadcast = 10.99.99.255
default_pool = [10.99.99.2 .. 10.99.99.254]   # 排除 network/broadcast/server
```

显式 `--pool` 覆盖默认派生，但工具会校验池不与服务器地址、network、broadcast 重叠。

---

## 4. 总体架构

```
┌─────────────────────────────────────────────────────────┐
│                        CLI (clap)                       │
│  --iface --server-ip --pool --lease --static --cleanup  │
└───────────────────────────┬─────────────────────────────┘
                            │ Config
                            ▼
┌─────────────────────────────────────────────────────────┐
│                    Orchestrator (main)                  │
│  1. 权限检查  2. 链路管理  3. 地址管理  4. 启动 DHCP 循环 │
└───────┬───────────────────────────────┬─────────────────┘
        │                               │
        ▼                               ▼
┌───────────────────┐         ┌───────────────────────────┐
│  Netlink Module   │         │      DHCP Server Module   │
│  - link up        │         │  - UDP socket (67)        │
│  - addr add/del   │         │  - SO_BINDTODEVICE        │
│  - 查询已有地址    │         │  - 报文解析 / 构造         │
└───────────────────┘         │  - 地址池 / 租约表         │
                              │  - 状态机                 │
                              └───────────────────────────┘
                                        │
                                        ▼
                              ┌───────────────────────────┐
                              │      Logging (tracing)    │
                              └───────────────────────────┘
```

---

## 5. CLI 接口设计

### 5.1 命令形式

```bash
dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 [OPTIONS]
```

### 5.2 参数表

| 参数 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `--iface <NAME>` | 是 | — | 目标网卡名，如 `eth1`、`enp0s20u1` |
| `--server-ip <CIDR>` | 是 | — | 服务器地址与前缀，如 `10.99.99.1/24` |
| `--pool <START-END>` | 否 | 由 CIDR 派生 | 地址池，如 `10.99.99.100-10.99.99.200` |
| `--lease <DURATION>` | 否 | `12h` | 租约时长，支持 `30m`/`12h`/`1d` |
| `--gateway <IP>` | 否 | 服务器地址 | 下发的 option 3 |
| `--dns <IP[,IP]>` | 否 | 无 | 下发的 option 6 |
| `--static <MAC=IP>` | 否 | 无 | 静态绑定，可重复 |
| `--cleanup-on-exit` | 否 | false | 退出时删除工具添加的地址 |
| `--no-config-addr` | 否 | false | 跳过配址（网卡已有 IP 时使用） |
| `--log-dhcp` | 否 | false | 打印每次 DHCP 交互详情 |
| `--foreground` | 否 | true | 保留前台运行；预留 daemon 化扩展 |

### 5.3 使用示例

```bash
# 最简：给 eth1 配 10.99.99.1/24，池自动派生为 .2-.254
sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 --log-dhcp

# 指定池 + 静态绑定 + 退出清理
sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 \
  --pool 10.99.99.100-10.99.99.150 \
  --static aa:bb:cc:dd:ee:ff=10.99.99.100 \
  --lease 2h --dns 1.1.1.1 --cleanup-on-exit
```

---

## 6. 核心流程

### 6.1 启动流程

```
main()
 ├─ 1. 解析 CLI → Config
 ├─ 2. 权限检查（euid==0 或具备所需 capability）
 ├─ 3. 解析 iface → ifindex（if_nametoindex）
 ├─ 4. Netlink: 查询链路状态
 │     └─ 若 down → 置 IFF_UP
 ├─ 5. Netlink: 查询接口现有 IPv4 地址
 │     ├─ 已有 == server-ip/prefix → 跳过
 │     ├─ 已有其他地址 → 默认追加（NLM_F_CREATE），记录 added=true
 │     └─ 无地址 → 添加（NLM_F_CREATE|NLM_F_REPLACE），记录 added=true
 ├─ 6. 构造地址池（显式或派生）并校验
 ├─ 7. 创建 UDP socket:
 │     - socket(AF_INET, SOCK_DGRAM, 0)
 │     - setsockopt(SO_BINDTODEVICE, iface)
 │     - setsockopt(SO_REUSEADDR, 1)
 │     - setsockopt(SO_BROADCAST, 1)
 │     - bind(0.0.0.0:67)
 ├─ 8. 注册 SIGINT/SIGTERM 处理
 └─ 9. 进入收包循环
```

### 6.2 收包与响应循环

```
loop:
  (pkt, src_addr, ifindex) = recvfrom(sock, buf)
  │  # SO_BINDTODEVICE 已限制接口，ifindex 校验可选
  ├─ 解析 BOOTP 头（236B）+ magic cookie(0x63825363) + options
  ├─ 提取: op(1=request), xid, chaddr(MAC), flags, ciaddr, option 53(msg type)
  ├─ match msg_type:
  │    DISCOVER → 选地址 → 构造 OFFER
  │    REQUEST  → 校验/确认 → ACK 或 NAK
  │    RELEASE  → 释放租约（无回复）
  │    DECLINE  → 标记地址冲突，冷却
  │    INFORM   → 回 ACK（不含 yiaddr，仅配置项）
  └─ 发送响应:
       - 若 flags.broadcast 置位 或 ciaddr==0 → 广播 255.255.255.255:68
       - 否则尝试单播 yiaddr:68（调试默认仍倾向广播，见 7.3）
```

### 6.3 退出流程

```
signal(SIGINT/SIGTERM)
 ├─ 停止收包循环
 ├─ flush 日志
 ├─ if cleanup_on_exit && addr_added_by_us:
 │     └─ Netlink: RTM_DELADDR 删除 server-ip
 └─ exit(0)
```

---

## 7. 模块详细设计

### 7.1 Netlink 模块（`netlink.rs`）

**职责**：链路状态与地址管理，不依赖外部命令。

**接口**：

```rust
pub struct Netlink { handle: rtnetlink::Handle }

impl Netlink {
    pub async fn new() -> Result<Self>;
    pub async fn ifindex(&self, name: &str) -> Result<u32>;
    pub async fn ensure_link_up(&self, ifindex: u32) -> Result<bool>;      // 返回是否变更
    pub async fn list_ipv4(&self, ifindex: u32) -> Result<Vec<(Ipv4Addr,u8)>>;
    pub async fn add_addr(&self, ifindex: u32, ip: Ipv4Addr, prefix: u8) -> Result<()>;
    pub async fn del_addr(&self, ifindex: u32, ip: Ipv4Addr, prefix: u8) -> Result<()>;
}
```

**实现要点**：

- `ensure_link_up`：`RTM_NEWLINK` + `ifi_flags |= IFF_UP`，仅当当前未 up 时执行；
- `add_addr`：`RTM_NEWADDR`，`ifa_index=ifindex`、`ifa_family=AF_INET`、`ifa_prefixlen=prefix`，挂载 `IFA_LOCAL`（地址）与 `IFA_BROADCAST`（`ip | ~mask`），flags 使用 `NLM_F_CREATE | NLM_F_REPLACE`；
- `del_addr`：`RTM_DELADDR`，同样填 `ifa_index/ifa_family/ifa_prefixlen/IFA_LOCAL`；
- 所有调用需 `CAP_NET_ADMIN`。

**C 备选**：`libnl-route` 的 `rtnl_addr_add()` / `rtnl_addr_delete()` / `rtnl_link_change()`，语义一一对应。

### 7.2 DHCP 服务模块（`dhcp/`）

拆为四个子模块：

#### 7.2.1 `socket.rs`

```rust
pub fn create_dhcp_socket(iface: &str) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    sock.set_reuse_address(true)?;
    sock.set_broadcast(true)?;
    sock.bind_to_device(iface)?;                    // SO_BINDTODEVICE
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 67).into())?;
    Ok(sock.into())
}
```

需要 `CAP_NET_RAW`（`SO_BINDTODEVICE`）+ `CAP_NET_BIND_SERVICE`（端口 67）。

#### 7.2.2 `packet.rs`（报文解析/构造）

DHCP 报文布局：

| 偏移 | 长度 | 字段 |
|---|---|---|
| 0 | 1 | `op`（1=BOOTREQUEST, 2=BOOTREPLY） |
| 1 | 1 | `htype`（1=Ethernet） |
| 2 | 1 | `hlen`（6） |
| 3 | 1 | `hops` |
| 4 | 4 | `xid` |
| 8 | 2 | `secs` |
| 10 | 2 | `flags`（bit15 = broadcast） |
| 12 | 4 | `ciaddr` |
| 16 | 4 | `yiaddr` |
| 20 | 4 | `siaddr` |
| 24 | 4 | `giaddr` |
| 28 | 16 | `chaddr`（MAC 在 [28..34]） |
| 44 | 192 | `sname` + `file`（本工具不填充） |
| 236 | 4 | magic cookie = `0x63 0x82 0x53 0x63` |
| 240 | var | options（TLV，以 `255` 结束） |

```rust
pub struct Bootp {
    pub op: u8, pub xid: u32, pub flags: u16,
    pub ciaddr: Ipv4Addr, pub yiaddr: Ipv4Addr,
    pub siaddr: Ipv4Addr, pub giaddr: Ipv4Addr,
    pub chaddr: [u8; 16],
    pub options: Vec<Option_>,   // 类型化 TLV
}

impl Bootp {
    pub fn parse(buf: &[u8]) -> Result<Self>;
    pub fn serialize(&self, out: &mut Vec<u8>);
    pub fn msg_type(&self) -> Option<MsgType>;
    pub fn mac(&self) -> MacAddr;
    pub fn wants_broadcast(&self) -> bool { self.flags & 0x8000 != 0 }
}
```

**响应选项构造**：

| Option | 值 | 条件 |
|---|---|---|
| 53 | OFFER(2) / ACK(5) / NAK(6) | 必填 |
| 54 | server-ip | 必填 |
| 51 | lease_seconds | OFFER/ACK |
| 1 | subnet mask | 由 prefix 计算 |
| 3 | gateway | 若配置 |
| 6 | DNS | 若配置 |
| 58 / 59 | T1 = lease/2, T2 = lease*7/8 | 可选 |
| 255 | end | 必填 |

`op=2`、`htype=1`、`hlen=6`、`xid` 与请求一致、`yiaddr` 填分配地址、`chaddr` 回填、`flags` 复制自请求。

#### 7.2.3 `pool.rs`（地址池）

```rust
pub struct Pool {
    range: (Ipv4Addr, Ipv4Addr),
    reserved: HashSet<Ipv4Addr>,         // server-ip / network / broadcast / gateway
    static_map: HashMap<MacAddr, Ipv4Addr>,
    leases: HashMap<MacAddr, Lease>,     // MAC → 分配结果
    assigned: HashMap<Ipv4Addr, MacAddr>,
}

pub struct Lease {
    pub ip: Ipv4Addr,
    pub expires_at: Instant,
    pub state: LeaseState,   // Offered / Bound / Declined
}
```

**分配算法**：

1. 若 `static_map` 命中请求 MAC → 直接返回该地址（并检查是否被他人占用，若冲突则 NAK）；
2. 若 `leases` 中该 MAC 存在未过期租约 → 续约返回同一地址；
3. 否则在 `range` 内线性/轮转查找第一个未分配、未保留的地址；
4. 池耗尽 → 记录日志，不回复（或回 NAK，调试场景建议不回复以便观察）。

**offer 暂存**：DISCOVER 阶段分配但未 REQUEST 的地址记为 `Offered`，设置短超时（如 60s）后回收，防止恶意 DISCOVER 耗尽池。

#### 7.2.4 `state_machine.rs`

```rust
match (msg_type, pool.lookup(mac), request.ciaddr) {
    (Discover, _, _)          => handle_discover(),
    (Request, Some(lease), _) => handle_request_selecting_or_renew(),
    (Request, None, _)        => NAK,
    (Release, _, _)           => pool.release(mac),
    (Decline, _, _)           => pool.mark_declined(mac),
    (Inform, _, _)            => handle_inform(),
    _                         => drop,
}
```

**REQUEST 的两种形态**（调试工具必须都处理）：

- Selecting 态：带 option 50（requested IP）+ option 54（server id）；
- Renewing 态：`ciaddr` 非零、无 option 54。

校验规则：requested IP 必须属于本池、server id（若有）必须等于本服务器、且该 IP 未被他人占用；否则回 NAK。

### 7.3 单播 vs 广播策略

| 请求特征 | 回复方式 |
|---|---|
| `flags.broadcast == true` | 广播 `255.255.255.255:68` |
| `ciaddr != 0`（Renewing） | 单播 `ciaddr:68`（内核有 ARP 表项，通常可达） |
| `ciaddr == 0` 且无 broadcast flag | 优先广播（调试场景更稳），日志提示 |

**决策**：默认“客户端未持有 IP 时用广播”，因为单播到 `yiaddr` 依赖内核 ARP 解析，而客户端此时可能尚未响应 ARP。提供 `--strict-unicast` 保留规范行为。

### 7.4 日志模块

- 默认 `INFO`：启动配置、地址添加结果、监听就绪、退出清理；
- `--log-dhcp` 时 `DEBUG`：每次收包的 `xid / mac / msg_type / 分配结果 / 回复方式`；
- 格式示例：

```
INFO  iface=eth1 ifindex=3 link_up=true added_addr=10.99.99.1/24
INFO  pool=10.99.99.2-10.99.99.254 lease=12h server_id=10.99.99.1
INFO  listening on eth1:67
DEBUG dhcp xid=0x1a2b3c4d mac=aa:bb:cc:dd:ee:ff type=DISCOVER -> OFFER 10.99.99.2
DEBUG dhcp xid=0x1a2b3c4d mac=aa:bb:cc:dd:ee:ff type=REQUEST  -> ACK   10.99.99.2
```

---

## 8. 数据结构与配置

```rust
pub struct Config {
    pub iface: String,
    pub server_ip: Ipv4Addr,
    pub prefix: u8,
    pub pool: Option<(Ipv4Addr, Ipv4Addr)>,
    pub lease: Duration,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub statics: Vec<(MacAddr, Ipv4Addr)>,
    pub cleanup_on_exit: bool,
    pub no_config_addr: bool,
    pub log_dhcp: bool,
}

pub struct RuntimeState {
    pub ifindex: u32,
    pub addr_added_by_us: bool,
    pub link_brought_up: bool,
    pub pool: Pool,
    pub started_at: Instant,
}
```

---

## 9. 错误处理与信号

### 9.1 错误分类

| 类别 | 示例 | 处理 |
|---|---|---|
| 配置错误 | pool 与 server 重叠、MAC 格式错 | 启动即退出，退出码 2 |
| 权限错误 | 缺 `CAP_NET_RAW` | 退出码 3，提示所需 capability |
| netlink 失败 | 接口不存在、地址已存在 | 退出码 4，附 errno |
| 运行时收包错误 | `EINTR` | 忽略（信号中断） |
| 运行时收包错误 | 其他 errno | 记录并继续，连续 N 次失败后退出 |
| 报文解析失败 | 非 DHCP 流量（67 端口上的杂包） | 静默丢弃，DEBUG 记录 |

### 9.2 信号

```rust
let (tx, rx) = signal_channel();
// SIGINT / SIGTERM → tx.send(Shutdown)
// 主循环 select { recvfrom, rx }，收到 Shutdown 后走 6.3 退出流程
```

---

## 10. 安全与隔离

1. **接口级隔离**：`SO_BINDTODEVICE` 保证 socket 只在该网卡收发，是防止误响应其他网络的第一道锁。
2. **子网隔离建议**：文档推荐使用 `10.99.99.0/24` 等冷门网段，降低与宿主机其他网络冲突的概率。
3. **池校验**：`--pool` 与 `--server-ip`、network、broadcast、gateway 的重叠在启动时硬性拒绝。
4. **权限最小化**：文档给出 capability 拆分，允许以 `setcap` 方式运行而非全程 root：

```bash
sudo setcap cap_net_admin,cap_net_raw,cap_net_bind_service+ep ./dhcp-mock
```

5. **不修改全局状态**：除指定网卡的地址和链路状态外，不触碰路由表、`sysctl`、防火墙。

---

## 11. 测试方案

### 11.1 单元测试

- `packet.rs`：用抓包样本（`tcpdump -w` 保存的 DISCOVER/REQUEST）验证解析与序列化往返；
- `pool.rs`：分配/续约/耗尽/静态绑定/冲突场景；
- 配置派生：各种 CIDR 下的默认池边界。

### 11.2 集成测试（network namespace）

```bash
# 环境准备
ip netns add srv
ip netns add cli
ip link add veth-s type veth peer name veth-c
ip link set veth-s netns srv
ip link set veth-c netns cli
ip -n srv link set veth-s up
ip -n cli link set veth-c up

# 运行工具
ip netns exec srv ./dhcp-mock --iface veth-s --server-ip 10.99.99.1/24 --log-dhcp &

# 客户端验证
ip netns exec cli udhcpc -i veth-c -f -q
ip netns exec cli ip addr show veth-c   # 应看到 10.99.99.x/24
```

覆盖用例：正常四步、续约（`udhcpc -R` 或手动触发）、静态绑定、池耗尽、`--cleanup-on-exit` 后地址确实被删除。

### 11.3 兼容性矩阵

用 `dhclient`、`udhcpc`（BusyBox）、Windows 客户端、典型开发板（如树莓派、ESP32 的 lwIP DHCP）各跑一次，确认 option 集合兼容。

---

## 12. 目录结构建议（Rust）

```
dhcp-mock/
├── Cargo.toml
├── src/
│   ├── main.rs               # 编排：解析→权限→netlink→server loop
│   ├── config.rs             # clap 定义 + 校验
│   ├── netlink.rs            # 链路/地址管理
│   ├── error.rs              # 统一错误类型 + 退出码映射
│   └── dhcp/
│       ├── mod.rs
│       ├── socket.rs
│       ├── packet.rs         # BOOTP/DHCP 编解码
│       ├── options.rs        # option 常量与 TLV
│       ├── pool.rs
│       └── server.rs         # 状态机 + 主循环
├── tests/
│   └── netns.rs              # 集成测试（调用 ip netns 脚本）
└── README.md
```

---

## 13. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 | CLI + netlink 配址/链路 up | `--iface` 指定网卡后 `ip addr` 可见地址 |
| M2 | UDP socket + DISCOVER/OFFER 往返 | `udhcpc` 能收到 OFFER（日志可见） |
| M3 | 完整四步 + 地址池 + 租约 | `udhcpc` 成功拿到地址并可 ping 通 |
| M4 | 静态绑定、续约、RELEASE/DECLINE | 集成测试全绿 |
| M5 | 信号处理 + `--cleanup-on-exit` + 日志完善 | 优雅退出、地址按预期清理 |
| M6 | 兼容性矩阵 + 文档 | 四类客户端全部通过 |

---

## 14. 风险与未决问题

| 风险 | 影响 | 缓解 |
|---|---|---|
| 客户端依赖单播回复 | 未启用 broadcast flag 的客户端可能收不到 | 默认广播回复；提供 `--strict-unicast` 切换 |
| 网卡上已有其他 DHCP 服务器 | 客户端可能选中对方 | 调试场景可接受；文档提示用冷门网段 |
| netlink 添加地址与 NetworkManager/systemd-networkd 竞争 | 地址被管理进程移除 | 文档建议在调试用网卡上 disable 对应管理服务，或在 netns 中运行 |
| 大量 DISCOVER 耗尽池 | 无法继续分配 | `Offered` 状态 60s 超时回收 |
| capability 组合在某些发行版 `setcap` 行为差异 | 启动失败 | 提供 `sudo` 直接运行作为兜底，README 说明 |

**未决**：

- 是否需要 `--daemonize`（当前保留 `--foreground` 语义，默认前台）；
- 是否需要把租约表导出为 JSON 供外部工具查询（调试增强，非首版）。