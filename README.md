# dhcp-mock

[![CI](https://github.com/lanshuye123/DHCPMock/actions/workflows/ci.yml/badge.svg)](https://github.com/lanshuye123/DHCPMock/actions/workflows/ci.yml)

调试用最小 DHCPv4 服务器。给一块直连网卡自动配好 IPv4 地址与链路，然后只在该网卡上为开发板、测试机、PXE 设备分配地址。

实现对应 [`Design.md`](Design.md) 的 **M1–M5**，并额外实现了设计文档中未决的 `--strict-unicast` 与租约 JSON 导出。

```bash
sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 --log-dhcp
```

## 目录

- [状态](#状态)
- [功能](#功能)
- [获取源码](#获取源码)
- [构建](#构建)
- [权限](#权限)
- [快速开始](#快速开始)
- [参数](#参数)
- [行为说明](#行为说明)
- [退出码](#退出码)
- [测试](#测试)
- [与设计文档的差异](#与设计文档的差异)
- [故障排查](#故障排查)
- [目录结构](#目录结构)
- [文档与许可](#文档与许可)

---

## 状态

| 项 | 状态 |
|---|---|
| M1 CLI + netlink 配址/链路 up | ✅ 集成测试覆盖 |
| M2 UDP socket + DISCOVER/OFFER | ✅ 集成测试覆盖 |
| M3 完整四步 + 地址池 + 租约 + 连通性 | ✅ 集成测试 + `ping` 验证 |
| M4 静态绑定、续约、RELEASE/DECLINE | ✅ 集成测试覆盖 |
| M5 信号、`--cleanup-on-exit`、日志、租约 JSON | ✅ 集成测试覆盖 |
| M6 兼容性矩阵 | ⚠️ 已验证 ISC `dhclient 4.4.3-P1`；Windows / ESP32 lwIP 未在本机验证 |

测试：51 个单元测试 + 8 个双 netns 端到端场景，`cargo test` 全绿。

---

## 功能

- 指定单块网卡，工具只在该接口收发 DHCP（`SO_BINDTODEVICE`）；
- 自动把链路置 up、自动添加 `--server-ip`（已存在则复用）；
- 处理 DISCOVER / REQUEST / RELEASE / DECLINE / INFORM；
- 地址池支持显式范围或由 CIDR 派生；
- 按 MAC 静态绑定；
- 可配置租约时长、网关、DNS、T1/T2；
- 默认广播回复，`--strict-unicast` 切换为 RFC 2131 单播语义；
- 退出时按需删除本工具添加的地址；
- 每次交互默认在控制台打印 `MAC / 类型 / 分配结果`（`--log-dhcp` 额外补充 xid / 目的地址 / hostname）；
- 租约表可实时导出为 JSON（`--lease-file` / `SIGUSR1`）。

### 非目标

- DHCPv6、DHCP relay（`giaddr != 0` 的包直接忽略）、PXE 引导扩展；
- 租约持久化（进程退出即丢失）；
- 生产级高可用、并发性能、审计。

---

## 获取源码

```bash
git clone https://github.com/lanshuye123/DHCPMock.git
cd DHCPMock
```

## 构建

要求 Linux（内核 ≥ 5.0）、Rust 1.85+。

```bash
cargo build --release
# 可执行文件：target/release/dhcp-mock
```

无外部命令依赖（不调用 `ip` / `ifconfig`）；netlink、socket 全部走系统调用。

---

## 权限

需要以下 capability：

| capability | 用途 |
|---|---|
| `CAP_NET_ADMIN` | netlink 链路 / 地址修改 |
| `CAP_NET_RAW` | `SO_BINDTODEVICE` |
| `CAP_NET_BIND_SERVICE` | 绑定 UDP/67 |

两种运行方式：

```bash
# 1) root
sudo ./target/release/dhcp-mock --iface eth1 --server-ip 10.99.99.1/24

# 2) 文件 capability（无需全程 root）
sudo setcap cap_net_admin,cap_net_raw,cap_net_bind_service+ep ./target/release/dhcp-mock
./target/release/dhcp-mock --iface eth1 --server-ip 10.99.99.1/24
```

启动时读取 `/proc/self/status` 的 `CapEff` 检查；缺失 capability 时退出码 3 并打印所需项与 `setcap` 命令。某些发行版会因挂载选项（`nosuid`）忽略文件 capability，此时请直接用 root。

---

## 快速开始

```bash
# 最简：为 eth1 配 10.99.99.1/24，池自动派生为 10.99.99.2-10.99.99.254
sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 --log-dhcp

# 指定池 + 静态绑定 + DNS + 退出清理 + 租约快照
sudo dhcp-mock --iface eth1 --server-ip 10.99.99.1/24 \
  --pool 10.99.99.100-10.99.99.150 \
  --static aa:bb:cc:dd:ee:ff=10.99.99.100 \
  --lease 2h --dns 1.1.1.1,8.8.8.8 \
  --lease-file /tmp/dhcp-leases.json \
  --cleanup-on-exit
```

直连设备侧的地址应在 `10.99.99.0/24` 内；建议使用 `10.99.99.0/24` 这类冷门网段，避免与宿主机其他网络冲突。

---

## 参数

| 参数 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `--iface <NAME>` | 是 | — | 目标网卡名，如 `eth1`、`enp0s20u1` |
| `--server-ip <CIDR>` | 是 | — | 服务器地址与前缀，如 `10.99.99.1/24`；前缀需 `/1`–`/30` |
| `--pool <START-END>` | 否 | 由 CIDR 派生 | 地址池，如 `10.99.99.100-10.99.99.150`；单地址可写 `IP-IP` |
| `--lease <DURATION>` | 否 | `12h` | 支持 `90s` / `30m` / `12h` / `1d` / `1h30m`，裸数字视为秒 |
| `--gateway <IP>` | 否 | 服务器地址 | 下发的 option 3；必须在同一子网 |
| `--dns <IP[,IP]>` | 否 | 无 | 下发的 option 6；可逗号分隔或重复传参 |
| `--static <MAC=IP>` | 否 | 无 | 静态绑定，可重复；MAC 支持 `:` / `-` / 无分隔 |
| `--cleanup-on-exit` | 否 | false | 退出时删除**本次运行**添加的地址 |
| `--no-config-addr` | 否 | false | 跳过添加地址（仍会把链路置 up） |
| `--log-dhcp` | 否 | false | 额外打印 xid / 目的地址 / hostname 等 DEBUG 细节（交互行默认已可见） |
| `--strict-unicast` | 否 | false | 严格按 RFC 2131 单播；默认在客户端无 IP 时广播 |
| `--lease-file <PATH>` | 否 | 无 | 租约表 JSON 快照路径；每次租约变化与退出时原子更新 |
| `--foreground` | 否 | true | 接受但无实际作用；当前总是前台运行，未实现 daemon 化 |

`--help` / `--version` 由 clap 提供。

---

## 行为说明

### 网卡与地址

1. `if_nametoindex` 解析网卡（不存在 → 退出码 4）；
2. 若链路 down，通过 rtnetlink 置 `IFF_UP`；
3. 查询接口现有 IPv4 地址：
   - 已有完全一致的 `server-ip/prefix` → 复用，不记录为“本次添加”；
   - 否则用 `RTM_NEWADDR`（`NLM_F_CREATE | NLM_F_REPLACE`）添加，并记录 `added=true`；
4. `--no-config-addr` 时跳过第 3 步；若接口上没有该地址，会打印 warning（没有 IPv4 地址的接口可能收不到广播）。

启动时若在加地址之后失败（例如租约文件不可写、socket 创建失败），会**回滚**本次添加的地址，让系统恢复原状。正常退出时是否删除地址只由 `--cleanup-on-exit` 决定。

### 地址池派生

服务器地址 `10.99.99.1/24` 时：

```
network   = 10.99.99.0
broadcast = 10.99.99.255
pool      = 10.99.99.2 - 10.99.99.254
```

派生规则：`[network+1, broadcast-1]`，再从两端裁掉保留地址（服务器、网关、同子网 DNS）。分配时还会跳过：network、broadcast、服务器、网关、同子网 DNS、静态绑定地址、已分配地址、DECLINE 冷却中的地址。

显式 `--pool` 时**不会**自动裁剪；但启动时硬性校验池不与服务器地址、网关、network、broadcast 重叠（配置错误 → 退出码 2）。池内可以包含静态绑定地址，动态分配会自动跳过。

池耗尽时不回复（便于观察），并打印 `address pool exhausted` 警告。

### 静态绑定

- `--static MAC=IP` 可重复；MAC 必须是单播、非全零；IP 必须在本子网内且不能是 network / broadcast / 服务器 / 网关；
- 静态 IP 可以在动态池内，也可以在池外；池内时会被动态分配跳过；
- 静态客户端即使没有先 DISCOVER，直接 REQUEST（INIT-REBOOT）也会被 ACK；
- 静态地址被其他客户端占用时回 NAK；
- 同一 MAC / IP 重复绑定在启动时报配置错误。

### 租约与状态机

| 报文 | 行为 |
|---|---|
| DISCOVER | 静态命中 → 该地址；已有租约/OFFER → 复用；否则轮转找空闲地址；返回 OFFER，地址记为 `offered`，60s 未 REQUEST 自动回收 |
| REQUEST（SELECTING，option 50 + 54） | 校验 server-id、地址归属、是否被占用；通过则 ACK 并记 `bound`，否则 NAK |
| REQUEST（RENEWING，`ciaddr != 0`） | 有对应租约则续约 ACK；否则按设计回 NAK（进程重启后租约表为空，客户端会重新走 DORA） |
| RELEASE | 释放该 MAC 的租约，不回复 |
| DECLINE | 将该地址移出轮转并冷却 300s，不回复 |
| INFORM | 回 ACK（`yiaddr=0`，不含 option 51/58/59），不创建租约 |

NAK 携带 option 53/54/56（原因文本）。静态绑定的 INIT-REBOOT 是唯一允许“无租约直接 ACK”的情况。

### 回复方式

| 请求特征 | 默认 | `--strict-unicast` |
|---|---|---|
| `flags` 广播位 = 1 | 广播 `255.255.255.255:68` | 同左 |
| `ciaddr != 0` | 单播 `ciaddr:68` | 同左 |
| `ciaddr == 0` 且无广播位 | **广播**（调试更稳） | 单播 `yiaddr:68` |

默认广播的原因：客户端此时还没有地址，单播到 `yiaddr` 依赖内核 ARP，而客户端可能不响应 ARP。NAK 在 `ciaddr == 0` 时无论何种模式都只能广播（不能单播到 `0.0.0.0`）。

### 退出与清理

`SIGINT` / `SIGTERM` → 停止收包 → 刷新租约快照 → 打印统计 → 按 `--cleanup-on-exit` 删除**本次添加**的地址 → 退出 0。

- 默认保留地址（符合调试定位）；
- 如果地址是启动时已存在的，不会删除；
- 运行期致命错误（连续 10 次收包失败）退出码 1；若指定了 `--cleanup-on-exit` 同样会清理地址。

### 日志

默认 INFO 就把每次 DHCP 交互打到 stderr（带时间戳），嵌入式调试时可以直接看到“哪个 MAC 拿到了哪个 IP”：

```
INFO [DHCP] listening on eth1:67  server=10.99.99.1/24  pool=10.99.99.2-10.99.99.254  lease=12h  gateway=10.99.99.1
INFO [DHCP] aa:bb:cc:dd:ee:ff  DISCOVER -> OFFER  10.99.99.100  (lease 12h)
INFO [DHCP] aa:bb:cc:dd:ee:ff  REQUEST  -> ACK    10.99.99.100  (lease 12h)
INFO [DHCP] aa:bb:cc:dd:ee:ff  RELEASE             10.99.99.100
INFO [DHCP] aa:bb:cc:dd:ee:ff  DECLINE             10.99.99.101  (client reported conflict, cooldown 5m)
INFO [DHCP] aa:bb:cc:dd:ee:ff  REQUEST  -> NAK    10.99.99.105  (no offer or lease exists for this client)
INFO [DHCP] aa:bb:cc:dd:ee:ff  INFORM   -> ACK    -  (no lease)
WARN [DHCP] aa:bb:cc:dd:ee:ff  DISCOVER -> no offer  (address pool exhausted)
```

`--log-dhcp` 额外打开 DEBUG 细节（xid、目的地址、hostname），不改变上面的 INFO 行：

```
DEBUG [DHCP] xid=0x00007001 mac=aa:bb:cc:dd:ee:ff kind=DISCOVER reply=OFFER yiaddr=10.99.99.100 dest=255.255.255.255:68 host=-
```

可用 `RUST_LOG` 覆盖默认过滤器，例如 `RUST_LOG=debug`。`netlink-packet-route` 针对新内核属性的 WARN 已被默认过滤掉。

### 租约 JSON

`--lease-file PATH`：启动时写一次，此后每次租约变化、SIGUSR1、优雅退出时原子更新（同目录临时文件 + `rename`）。

```json
{
  "schema_version": 1,
  "iface": "eth1",
  "ifindex": 3,
  "server_ip": "10.99.99.1",
  "prefix": 24,
  "network": "10.99.99.0",
  "broadcast": "10.99.99.255",
  "pool_start": "10.99.99.2",
  "pool_end": "10.99.99.254",
  "lease_seconds": 43200,
  "uptime_seconds": 12,
  "generated_at_unix": 1761480000,
  "leases": [
    {
      "mac": "aa:bb:cc:dd:ee:ff",
      "ip": "10.99.99.100",
      "state": "bound",
      "expires_in_secs": 7188
    }
  ],
  "declined": [
    { "ip": "10.99.99.101", "cooldown_secs": 240 }
  ],
  "counters": {
    "received": 4, "discover": 2, "offer": 2, "request": 1, "ack": 1,
    "nak": 0, "release": 0, "decline": 1, "inform": 0, "dropped": 0,
    "parse_errors": 0, "send_errors": 0, "sent": 3
  }
}
```

说明：

- `state` 为 `offered` 或 `bound`；
- 被 DECLINE 的地址在 `declined[]` 中按 IP 记录冷却剩余秒数（不占 `leases[]` 的 MAC 键）；
- `expires_in_secs` 来自单调时钟，`generated_at_unix` 是 wall clock；
- 不指定 `--lease-file` 时，`SIGUSR1` 直接把同样的 JSON 打印到日志（INFO，target `lease`）。

JSON Schema：[docs/lease-snapshot.schema.json](docs/lease-snapshot.schema.json)；可直接参考的示例：[docs/lease-snapshot.example.json](docs/lease-snapshot.example.json)。单元测试会校验示例与 schema 的必填字段，以及 schema 与实际序列化字段的一致性。

### 信号

| 信号 | 行为 |
|---|---|
| `SIGINT` / `SIGTERM` | 优雅退出，按 `--cleanup-on-exit` 处理地址 |
| `SIGUSR1` | 立即导出/打印租约 JSON |

---

## 退出码

| 码 | 含义 |
|---|---|
| 0 | 正常退出（含收到 SIGINT/SIGTERM） |
| 1 | 运行期错误（I/O、连续收包失败等） |
| 2 | 配置错误（池重叠、前缀非法、MAC 格式错、clap 用法错误） |
| 3 | 权限不足（缺 capability / `SO_BINDTODEVICE` / 绑定 67 失败） |
| 4 | netlink 错误（网卡不存在、地址操作失败） |

---

## 测试

### 单元测试

```bash
cargo test --lib
```

覆盖：CIDR / 地址池 / 租约时长解析、配置校验、默认池派生、DHCP 编解码与 malformed 包、选项拼接、地址池分配/续约/耗尽/静态/冲突/超时/冷却、回复目的地址策略、租约 JSON 原子写、退出码映射。

### 端到端测试（无需 root）

```bash
cargo test --test netns
```

每个场景在 `unshare -Urn` 创建的用户+网络 namespace 中运行；内部再创建两个网络 namespace（server / client）并用 veth 对连接，服务器在 server ns，测试客户端在 client ns（测试线程用 `setns(2)` 切进去）。覆盖：

1. `default_pool`：链路自动 up、地址自动添加、默认池首个地址、SIGUSR1 JSON 输出；
2. `basic`：OFFER/ACK、租约时长、DNS、租约文件状态、SIGUSR1、续约、RELEASE、用租约地址 `ping` 通服务器；
3. `static`：池内/池外静态绑定、动态客户端跳过静态地址、静态 INIT-REBOOT；
4. `exhaustion`：池耗尽不回复；
5. `decline`：DECLINE 冷却、冷却期间不再分配；
6. `cleanup`：`--cleanup-on-exit` 删除地址 / 默认保留地址；
7. `inform`：INFORM ACK 不带租约且不创建租约；
8. `wrong_server`：REQUEST 选择其他 server-id 时不回 NAK，且服务器继续工作。

其中 `default_pool` 场景**刻意不加 `--log-dhcp`**，并断言默认日志中出现 `[DHCP] <mac>  DISCOVER -> OFFER <ip>` 与 `REQUEST -> ACK` 行，确保“控制台默认可见 MAC→IP”这一行为不会回归。

依赖 `unshare`、`nsenter`、`ip`、`ping`，以及允许非特权 user namespace 的内核。CI 只运行 fmt / clippy / 单元测试 / release 构建；netns 套件需要本地或带特权的 runner 执行。没有这些条件时：

```bash
DHCP_MOCK_SKIP_NETNS=1 cargo test
```

> 测试客户端用普通 UDP socket（不是 `AF_PACKET`），所以需要在 client veth 上预置 `10.99.99.2/24` 才能收到广播；DHCP 报文中的 `ciaddr` 仍为 0，语义与无地址客户端一致。真实客户端（`dhclient` / `udhcpc`）走 raw socket，不需要这个测试地址。

### 真实客户端验证

已用 ISC `dhclient 4.4.3-P1` 在双 netns 中验证：

- `DHCPDISCOVER` → `DHCPOFFER`（10.99.99.100）→ `DHCPREQUEST` → `DHCPACK`；
- 客户端接口成功配置 `10.99.99.100/24`；
- 租约文件中的 subnet-mask / routers / lease-time / renewal / rebinding / domain-name-servers / server-identifier 均正确；
- 服务器日志可见完整 exchange 行。

`udhcpc`（BusyBox）与 Windows / ESP32 lwIP 未在本机验证；`scripts/netns-demo.sh` 提供了 root 环境下的 `udhcpc` 优先、`dhclient` 兜底的演示。

### 手工演示（root）

```bash
sudo scripts/netns-demo.sh
```

脚本创建两个独立 netns + veth，启动 `dhcp-mock`，运行 `udhcpc` 或 `dhclient`，最后打印客户端地址并清理。只在 netns 内操作，不影响宿主机网络。

---

## 与设计文档的差异

1. **REQUEST 选择其他 server-id 时忽略而非 NAK。** 设计文档 §7.2.4 写的是 server-id 不匹配回 NAK；本实现遵循 RFC 2131 §4.3.2（不应回复），同时释放本服务器的 OFFER。这样在网段里意外存在另一台 DHCP 服务器时不会主动破坏客户端的选择；调试场景更安全。日志会记录 `[DHCP] <mac>  REQUEST ignored (client selected server <id>)`。
2. **默认池显示范围会在两端裁掉保留地址。** 如 `--gateway 10.99.99.254` 时，默认池显示为 `.2-.253`；功能上等价（分配本来就会跳过保留地址），但日志/JSON 更直观。显式 `--pool` 不裁剪，仍按设计硬性拒绝与服务器/网关/network/broadcast 重叠。
3. **新增 `--strict-unicast`。** 对应 §14 风险缓解项。
4. **新增 `--lease-file` / SIGUSR1 租约 JSON。** 对应 §14 未决项；JSON 中 DECLINE 地址放在 `declined[]`，而不是 `leases[]` 的 `declined` 状态（按 IP 冷却更自然）。
5. **`--foreground` 仅为兼容保留。** 当前总是前台运行，未实现 `--daemonize`。
6. **目录位于仓库根目录**，而非设计文档 §12 建议的 `dhcp-mock/` 子目录；模块划分保持一致。
7. **启动失败会回滚本次添加的地址**；正常退出才按 F9 决定保留/删除。
8. **DHCP 交互日志默认 INFO。** 设计文档 §7.4 把 exchange 放在 DEBUG（仅 `--log-dhcp` 可见）；本实现按嵌入式调试需求默认打印 MAC→IP，`--log-dhcp` 仅补充 xid、目的地址、hostname 等细节。

---

## 故障排查

| 现象 | 处理 |
|---|---|
| `permission denied: missing CAP_...` | 用 root 或 `setcap`（提示里会打印当前二进制的绝对路径） |
| `netlink error: ... No such device` | `--iface` 拼写错误或网卡不在当前 netns；`ip link` 确认 |
| `binding UDP port 67 failed` | 端口被 dnsmasq/dhcpd 占用；停掉或换 netns |
| 客户端收不到 OFFER | 默认日志会打印 DISCOVER/OFFER；若没有，确认客户端与服务器在同一二层网段、没有其他 DHCP 服务器抢先；`--log-dhcp` 可看 xid/目的地址细节 |
| 客户端拿到地址但 ping 不通 | 检查是否被分配了保留地址（正常不会）、客户端防火墙、`--gateway` 是否合理 |
| 地址被 NetworkManager / systemd-networkd 移除 | 调试网卡上禁用对应管理服务，或在 netns 中运行（设计文档 §14 风险） |
| 有地址但收不到广播 | 不要用 `--no-config-addr` 跳过配址；确认接口已 up 且有 IPv4 地址 |
| `--lease-file` 写入失败 | 检查父目录存在且可写；启动时失败会回滚地址并退出 |
| 集成测试跳过/失败 | 需要非特权 user namespace；`unshare -Urn true` 自检，或 `DHCP_MOCK_SKIP_NETNS=1` 跳过 |
| 严格单播下客户端收不到 OFFER | 预期：客户端尚无地址，ARP 可能失败；调试默认请用广播模式 |

---

## 目录结构

```
.
├── .github/workflows/ci.yml
├── Cargo.toml
├── Cargo.lock
├── CHANGELOG.md
├── CONTRIBUTING.md
├── Design.md
├── LICENSE-APACHE
├── LICENSE-MIT
├── README.md
├── docs/
│   ├── lease-snapshot.example.json
│   └── lease-snapshot.schema.json
├── scripts/
│   └── netns-demo.sh
├── src/
│   ├── main.rs            # 编排：解析 → 权限 → netlink → 服务循环 → 清理
│   ├── lib.rs
│   ├── caps.rs            # CapEff 检查
│   ├── config.rs          # clap + 校验 + 池派生
│   ├── error.rs           # 统一错误 + 退出码
│   ├── netlink.rs         # 链路 / 地址管理
│   └── dhcp/
│       ├── mod.rs
│       ├── options.rs     # option 常量 / TLV
│       ├── packet.rs      # BOOTP/DHCP 编解码
│       ├── pool.rs        # 地址池 / 租约 / 冷却
│       ├── report.rs      # 租约 JSON / 计数器
│       ├── server.rs      # 状态机 + 主循环
│       └── socket.rs      # UDP/67 + SO_BINDTODEVICE
└── tests/
    └── netns.rs           # 双 netns 端到端测试
```

## 文档与许可

- 设计文档：[Design.md](Design.md)
- 变更记录：[CHANGELOG.md](CHANGELOG.md)
- 贡献指南：[CONTRIBUTING.md](CONTRIBUTING.md)
- 租约 JSON：[Schema](docs/lease-snapshot.schema.json) / [示例](docs/lease-snapshot.example.json)
- CI：[.github/workflows/ci.yml](.github/workflows/ci.yml)（fmt / clippy / 单元测试 / release；netns 套件本地跑）
- 许可证：[MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE)
