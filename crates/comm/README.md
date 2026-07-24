# comm

[![crates.io](https://img.shields.io/crates/v/comm.svg)](https://crates.io/crates/comm)
[![docs.rs](https://docs.rs/comm/badge.svg)](https://docs.rs/comm)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](../../LICENSE)

> A reusable **bidirectional communication orchestration** crate — sender + receiver façades, peer discovery, key management, anti-replay. Built on the embassy async runtime, targeting **no_std** by default.

`comm` 把"**发送 / 接收 / 发现 / 回复 / peer 列表 / receiver 选择 / 密钥管理 / 抗重放**"这些流程统一封装成 **`Notifier`**（`Coordinator`）和 **`Receiver`**（`Endpoint`）两个门面。使用者只需实现一个 [`CommLink`] trait，就能在任意物理链路（ESP-NOW、UART、TCP、内存回环…）上跑起完整的控制协议。

---

## 目录

- [comm](#comm)
  - [目录](#目录)
  - [核心概念](#核心概念)
    - [角色定位](#角色定位)
    - [三种帧](#三种帧)
  - [快速开始](#快速开始)
    - [添加依赖](#添加依赖)
    - [Coordinator 端（controller / notifier）](#coordinator-端controller--notifier)
    - [Endpoint 端（led / motor / srv…）](#endpoint-端led--motor--srv)
  - [API 全景](#api-全景)
    - [门面](#门面)
    - [Notifier 方法](#notifier-方法)
    - [Receiver 方法](#receiver-方法)
    - [后台 loop](#后台-loop)
    - [Prelude](#prelude)
  - [Feature 矩阵](#feature-矩阵)
  - [双身份 Notifier](#双身份-notifier)
  - [命令寻址：广播 vs 单播](#命令寻址广播-vs-单播)
  - [Endpoint 主动上报](#endpoint-主动上报)
    - [主动 `send_command` — 危险 API](#主动-send_command--危险-api)
  - [替换物理链路](#替换物理链路)
  - [Host 端集成测试](#host-端集成测试)
  - [错误处理与可观测性](#错误处理与可观测性)
    - [错误类型](#错误类型)
    - [`defmt` 日志埋点](#defmt-日志埋点)
  - [设计原则](#设计原则)
  - [版本兼容性](#版本兼容性)
  - [许可](#许可)

---

## 核心概念

### 角色定位

`Notifier` 与 `Receiver` 在**消息能力**层面已完全对称（都支持发送/接收 Frame / Command / Response），但**职责**是不对称的：

| 维度 | `Notifier`（**Coordinator**） | `Receiver`（**Endpoint**） |
|---|---|---|
| 拥有 `PeerRegistry` | ✅ 目录权威方 | ❌ 无目录 |
| 可选持有 `Selector` | ✅ UI 侧目标位图助手（不自动作用于发送）| ❌ |
| 主动 `discover()` | ✅ 发起会话 | ❌ 只能被动响应 |
| 首次遇到新 peer 时回 `AssignId` | ✅ 自动 | ❌ 只接收 `AssignId` |
| 主动 `send_frame` / `report` | ✅ | ✅ |
| 主动 `send_command` | ✅ | ⚠️ 需 opt-in feature |
| 处理入站 `Command` | ✅（双身份可选）| ✅（必填 `command_handler`）|
| 处理入站 `Response`（含 receiver 上报） | ✅（可选 `response_handler`）| —— |

> **一句话记忆**：`Notifier` 主导会话与拓扑；`Receiver` 是被协调的叶子节点，即便它也能主动上报数据。

为方便新代码选择更精确的命名，crate 提供两个 zero-cost 类型别名：

```rust
pub type Coordinator = Notifier;
pub type Endpoint = Receiver;
```

新老写法完全互通。门面**不含 link 泛型**——见下方"门面 = link 无关句柄"。

### 三种帧

| 类型 | 方向 | 频率 | 语义 |
|---|---|---|---|
| `Frame` | 主流 Coordinator → Endpoint | 高频状态流 | `GamepadState` + `dest_mask` 位图寻址；覆盖式，无 seq，无 Ack |
| `Command` | 主流 Coordinator → Endpoint | 低频控制流 | 带 HMAC + `seq`，触发 anti-replay；自动回 `Ack` / `Err` |
| `Response` | Endpoint → Coordinator | 应答 + 主动上报 | `AnnounceReply` / `Ack` / `BatterySnapshot` / … |

---

## 快速开始

### 添加依赖

```toml
[dependencies]
comm = { version = "0.2", default-features = false }
```

### 门面 = link 无关句柄

门面（`Notifier` / `Receiver`）**不持有 link**：真实设备把一个 `CommLink` 拆成
**send / recv 两个端**（各归一个 task），门面只收拢 `&'static` 共享状态。`build()`
一次后 `&'static` 化，门面**既跑后台 loop 又当生产者句柄**：

- 后台 loop：`facade.run_broadcast_loop(send_link)` / `facade.run_receive_loop(recv_link)`
- 主循环生产：`notifier.discover()` / `send_command()` / `send_frame()`；`receiver.report()`

（需要把 send / recv 拆到不同 task 又不想过门面的高级用户，仍可直接调
`notifier::run_broadcast_loop` 等自由函数 + `NotifierRecvConfig` / `ReceiverRecvConfig`。）

### Coordinator 端（controller / notifier）

```rust,ignore
use comm::prelude::*;
use comm::notifier::signals::{FrameSignal, CommandOutChannel, ResponseChannel};

// 1. 用户 crate 里声明 static 状态
static KEYRING: Keyring = Keyring::new();
static PEERS: PeerRegistry = PeerRegistry::new();
static REPLAY: ReplayGuard = ReplayGuard::new();
static FRAME_SIG: FrameSignal = FrameSignal::new();
static CMD_SIG: CommandOutChannel = CommandOutChannel::new();
static RESP_SIG: ResponseChannel = ResponseChannel::new();
static NOTIFIER: static_cell::StaticCell<Notifier> = static_cell::StaticCell::new();

// 2. 用 builder 组装（不含 link），`&'static` 化
//    selector 可选：它只是 UI 侧的目标位图容器，不会自动作用于发送，故此处省略。
let notifier: &'static Notifier = NOTIFIER.init(
    Notifier::builder()
        .keyring(&KEYRING)
        .peers(&PEERS)
        .replay(&REPLAY)
        .frame_signal(&FRAME_SIG)
        .command_signal(&CMD_SIG)
        .response_signal(&RESP_SIG)
        .build(),
);

// 3. 两条后台 loop 各喂一个 link 端（send / recv 分属两个 task）
#[embassy_executor::task]
async fn bcast(n: &'static Notifier, link: MySendLink) -> ! { n.run_broadcast_loop(link).await }
#[embassy_executor::task]
async fn recv(n: &'static Notifier, link: MyRecvLink) -> ! { n.run_receive_loop(link).await }

// 4. 主循环 API
notifier.discover();                    // 主动发起一次发现
for peer in notifier.peers() { /* ... */ } // peers() 直接返回 PeerInfo 快照 Vec
// 寻址取自帧自带的 dest_mask（选中 receiver 0 + 1）；单目标会透明升级为单播。
notifier.send_frame(&Frame::with_dest(seq, state, 0b0000_0011));
```

### Endpoint 端（led / motor / srv…）

```rust,ignore
use core::sync::atomic::AtomicU8;
use comm::prelude::*;
use comm::notifier::signals::{FrameSignal, CommandOutChannel, ResponseChannel};

static KEYRING: Keyring = Keyring::new();
static REPLAY: ReplayGuard = ReplayGuard::new();
static FRAME_SIG: FrameSignal = FrameSignal::new();
static CMD_SIG: CommandOutChannel = CommandOutChannel::new();
static RESP_SIG: ResponseChannel = ResponseChannel::new();
static MY_ID: AtomicU8 = AtomicU8::new(u8::MAX); // UNASSIGNED_ID
static RECEIVER: static_cell::StaticCell<Receiver> = static_cell::StaticCell::new();

fn handle_command(_src: CommandSource, cmd: &Command) -> CommandOutcome {
    match cmd.kind {
        CommandBody::LedBlink { .. } => {
            turn_on_led();
            CommandOutcome::Ok
        }
        _ => CommandOutcome::Err(ErrorCode::Unsupported),
    }
}

let receiver: &'static Receiver = RECEIVER.init(
    Receiver::builder()
        .keyring(&KEYRING)
        .replay(&REPLAY)
        .response_signal(&RESP_SIG)
        .frame_signal(&FRAME_SIG)
        .command_signal(&CMD_SIG)
        .role_tag(*b"led")
        .mac(MY_MAC)
        .my_id(&MY_ID)
        .command_handler(handle_command)
        .build(),
);

// 两条后台 loop：receiver.run_broadcast_loop(send) + receiver.run_receive_loop(recv)
```

> AnnounceReply / Ack / HMAC / anti-replay 全部由 crate 自动完成，业务只关心 `handle_command`。

---

## API 全景

### 门面

| 类型 | 作用 |
|---|---|
| [`Notifier`] / [`Coordinator`] | 发送端门面（link 无关句柄），持有 `PeerRegistry`，可选持有 `Selector` |
| [`Receiver`] / [`Endpoint`] | 接收端门面（link 无关句柄），持有 `command_handler` |
| [`CommLink`] | **唯一**硬件抽象 trait |

### Notifier 方法

| 类别 | 方法 |
|---|---|
| 主动出站（广播） | `send_frame(&Frame)` / `send_command(CommandBody)` |
| 主动出站（**单播**） | `send_command_to(receiver_id, CommandBody)` / `send_command_to_mac(mac, CommandBody)` |
| 发现 | `discover()` — 广播一次 `Announce` |
| Peer 目录 | `peers()` — 返回 `PeerInfo` 只读快照 `Vec`（非借出 registry） |
| 目标选择（可选 `Selector` 容器；不自动作用于发送，寻址取自 `Frame::dest_mask`） | `select_targets(mask)` / `selector()` |
| 会话 | `rotate_key(KeyId)` / `init_session(&mut entropy)` |

> 广播与单播的语义差异、可靠性、以及使用注意，见下文 [命令寻址：广播 vs 单播](#命令寻址广播-vs-单播)。

### Receiver 方法

| 类别 | 方法 |
|---|---|
| 主动出站 | `report(ResponseBody)` / `send_frame(&Frame)` / `send_command(_)` ⚠️ |
| 状态 | `assigned_id()` — 已分配的 `receiver_id`（`u8::MAX` 表未分配） |

### 后台 loop

推荐直接用门面方法 `notifier.run_broadcast_loop(link)` / `run_receive_loop(link)`
（`Receiver` 同名）——它们只是下面自由函数的糖，自动从门面字段拼参数。自由函数
仍公开，供不过门面的高级场景使用。

| 函数 | 作用 |
|---|---|
| `notifier::run_broadcast_loop` | 三路 select（Frame + Command + Response） → `CommLink::send`；入参 `peers: Option<&PeerRegistry>`，`Some(&PEERS)` 时 **Frame 按 `dest_mask` 自动单播/广播**（见下文） |
| `notifier::run_receive_loop` | `CommLink::recv` → 派发（Response upsert peers / 可选 Command 双身份）；参数打包成 `NotifierRecvConfig` |
| `receiver::run_broadcast_loop` | 复用 notifier 侧同名实现（一份代码两处用）；Endpoint 无目录，wrapper 恒传 `peers = None` → Frame 恒广播，签名保持 4 参不变 |
| `receiver::run_receive_loop` | `CommLink::recv` → 派发（Announce 自动回 / AssignId 写 `my_id` / 业务 handler）；参数打包成 `ReceiverRecvConfig`（同时充当内部 `DispatchCtx`） |
| `notifier::run_nonce_broadcast_loop` | 周期广播 session nonce（K3 密钥派生的组成部分）；门面糖为 `notifier.run_nonce_broadcast_loop(interval)`，直接复用门面持有的 `response_signal`，无需手抄 `&'static` 引用 |

### 可观测性：出站队列丢弃计数（`comm::metrics`）

Command / Response 走深度 `OUTBOUND_QUEUE_DEPTH` 的有界 FIFO，队列满时
`try_send` **静默丢弃当前这条**。`comm::metrics` 用两枚进程级 `AtomicU32`
把丢弃暴露出来，供健康巡检：

| API | 作用 |
|---|---|
| `metrics::dropped_commands()` / `dropped_responses()` | 读累计丢弃计数 |
| `metrics::snapshot() -> DropCounts` | 一次性取两枚计数（`DropCounts::is_clean()` 判无丢弃） |
| `metrics::reset()` | 清零（巡检窗口切换 / 测试隔离） |

只在真正丢弃时 `fetch_add(Relaxed)`，正常路径零开销。

### Prelude

```rust,ignore
use comm::prelude::*;
// 一站式 re-export：Notifier / Receiver / CommLink / Keyring / PeerRegistry /
// Selector / ReplayGuard / Frame / Command / CommandResponse / …
```

---

## Feature 矩阵

| Feature | 默认 | 作用 | 触发条件 |
|---|:---:|---|---|
| *（无 feature）* | ✅ | 纯 `no_std`，可在 esp32 / wasm32 / host 上编译 | — |
| `defmt` | ❌ | 全类型 & dispatch 静默丢弃点接入 `defmt` 日志 | field debug / logic-analyzer 场景 |
| `serde` | ❌ | 与 `protocol/serde` 联动，`PeerInfo` 等可 (de)serialize | Dashboard / gRPC bridge |
| `loopback` | ❌ | 内置一个 `std::sync::mpsc` 版 `CommLink`（`LoopbackLink::pair`） | host 端集成测试 / 演示 |
| `test-utils` | ❌ | 暴露 `DummyLink` 等测试 fixture | 集成测试 / 依赖侧单元测试 |
| `endpoint-initiated-command` | ❌ | ⚠️ 开启 `Receiver::send_command` — Endpoint 主动发 Command | 特殊拓扑（多 Coordinator / 对等发现） |

启用示例：

```toml
[dependencies]
comm = { version = "0.2", features = ["defmt"] }

[dev-dependencies]
comm = { version = "0.2", features = ["loopback", "test-utils"] }
```

---

## 双身份 Notifier

手柄类设备既需要**主动广播 Frame**（Notifier 本职），又需要**接收下行 Command**（LedBlink / QueryReceivers / …）。启用 `with_command_handler` 让同一个 `Notifier` 在同一 `CommLink` 上同时干两件事：

```rust,ignore
let notifier = Notifier::builder()
    // ... 常规 8 个必填字段 ...
    .with_command_handler(
        handle_command,          // fn(CommandSource, &Command) -> CommandOutcome
        *b"hst",                 // role_tag
        MY_MAC,                  // 本机 MAC
        &MY_ID,                  // AssignId 目标
        CommandSource::EspNow,   // 命令来源标签
    )
    .with_frame_handler(handle_upstream_frame) // 可选：同时订阅入站 Frame
    .build();
```

> 若同时接 BLE 和 ESP-NOW 两条链路，请分别搭 2 套 Notifier。

---

## 命令寻址：广播 vs 单播

Coordinator 下发的 `Command` 可以选择**广播给全网**或**单播给某一台** receiver。寻址信息由 `CommandOutChannel` 的载荷 [`OutboundCommand`] 携带（`dest: CommandDest { Broadcast | Unicast([u8; 6]) }` + 已编码字节），由 `run_broadcast_loop` 在出站时解释。

| 方法 | 目标 | 底层寻址 | 可靠性 |
|---|---|---|---|
| `send_command(body)` | 全网所有 receiver | `CommandDest::Broadcast` → 链路广播地址 | fire-and-forget，**无 ACK** |
| `send_command_to(id, body)` | `receiver_id == id` 的那台 | 反查 `PeerRegistry` 得 MAC → `CommandDest::Unicast` | ESP-NOW MAC 层 ACK + 有界重试 |
| `send_command_to_mac(mac, body)` | 指定 MAC（跳过反查） | 直接 `CommandDest::Unicast` | 同上 |

```rust,ignore
// 广播：发给所有 receiver（比如全体闪灯）
notifier.send_command(CommandBody::LedBlink { led_idx: 0, count: 3, period_ms: 100 });

// 单播：只发给 receiver_id == 2 的那台；未发现该 id 时返回 NotifierError::NoTarget
notifier.send_command_to(2, CommandBody::LedBlink { led_idx: 0, count: 3, period_ms: 100 })?;

// 已持有 MAC（例如来自一次 peers() 快照）时，直发省一次反查
notifier.send_command_to_mac(peer.mac, CommandBody::ShowToast { len, bytes });
```

**可靠性**：单播走 ESP-NOW 单播地址，能拿到 MAC 层 ACK；`run_broadcast_loop` 在 `send` 返回 `Err` 时再做 `MAX_UNICAST_SEND_RETRIES` 次有界补发。首次单播到某个未登记 MAC 时，链路侧（如 ESP-NOW）需惰性 `add_peer`——这属于 `CommLink` 实现细节，`comm` 只负责传目标地址。

> **有界队列，可组播，但一轮突发别超过队列深度**
> `CommandOutChannel` 是深度 `OUTBOUND_QUEUE_DEPTH`（默认 4）的 embassy `Channel`（FIFO）。连续 `send_command_to(a)` → `send_command_to(b)` 会**逐条排队出站**，不再像旧版覆盖式 `Signal` 那样只剩最后一条。唯一约束是**单轮突发别超过队列深度**：超出的那几条 `try_send` 会被丢弃。要发给很多台请分帧节流，或在自己 crate 里声明更深的 `Channel` 静态实例。

> **接收端配套**：单播只解决"送到哪台"。receiver 侧仍需在自己的 `command_handler` 里对相应 `CommandBody` 分支写执行逻辑——`comm` 只自动处理 `Announce` / `AssignId`，业务命令默认不执行（返回 `NoReply` 即被忽略）。

### Frame 寻址：由 `dest_mask` 自动派生的单播 / 广播

`Command` 的寻址是**显式**的（`CommandDest`）；而高频状态流 `Frame` 的寻址是**自动**的——寻址完全取自**帧自带的 `dest_mask`**，`run_broadcast_loop`（Notifier 侧传入 `Some(&PEERS)`）会在每帧出站前依据它决策（`select_targets` 只维护可选 `Selector` 容器的值，**不**会改写待发送的帧）：

| `dest_mask` 情况 | 出站方式 | 理由 |
|---|---|---|
| **恰好选中单个** receiver（1 个 bit）且其 MAC 已在 `PeerRegistry` | **单播**到该 MAC（复用 `MAX_UNICAST_SEND_RETRIES` 有界重试） | ESP-NOW 单播带 MAC 层 ACK + 重传，单点下发更可靠 |
| 单个 bit 但该 `receiver_id` 的 MAC 未知 | 广播 | 无法反查目标地址，退回广播 |
| 0 个 bit / ≥2 个 bit / 全选（`u32::MAX`） | 广播 | `Frame` 是高频流，**绝不** fan-out 成 N 条单播（会拖垮出站节拍） |

```rust,ignore
// 单目标：帧的 dest_mask 只选中 receiver_id=2 → send_frame 透明升级为单播（若该 id 的 MAC 已知）
notifier.send_frame(&Frame::with_dest(seq, state, 1 << 2));

// 多目标 / 全选 → 保持广播
notifier.send_frame(&Frame::with_dest(seq, state, comm::selector::DEST_MASK_ALL));
```

**要点**：
- 决策由**帧自带的** `dest_mask` 派生，**不是**新的显式 API，也不是全局开关——`bit-i ↔ receiver_id == i`，与 `Frame::is_addressed_to` / `Selector` 位映射一致。
- 单播帧**仍携带原 `dest_mask`**，故接收端的 `dest_mask` 过滤（被选中的那台 bit 已置位）照常放行，收发行为不变。
- Endpoint（Receiver）侧无 `PeerRegistry`，`receiver::run_broadcast_loop` 恒传 `peers = None`，Frame **一律广播**。

---

## Endpoint 主动上报

Receiver 侧的**上报通道** = `Receiver::report(ResponseBody)`：

```rust,ignore
// 每 30s 上报一次电量
receiver.report(ResponseBody::BatterySnapshot { percent: 85 });
```

**语义**：
- `req_seq = 0`，表示"非请求触发"
- `key_id` 使用 keyring 当前 active key
- 有界队列：上报进 `ResponseChannel`（深度 `OUTBOUND_QUEUE_DEPTH`）排队出站，不再互相覆盖；仅当队列满时才丢弃本次上报

Notifier 侧通过 `NotifierBuilder::with_response_handler(...)` 订阅（详见 `notifier::builder` doc）。

### 主动 `send_command` — 危险 API

```rust,ignore
// 需在 Cargo.toml 里 opt-in:
// comm = { version = "0.2", features = ["endpoint-initiated-command"] }
receiver.send_command(CommandBody::LedBlink { /* ... */ });
```

**⚠️ Correctness**：90% 场景下 Command 只走 Coordinator → Endpoint 方向；Endpoint 主动发 Command 会占用共享 keyring 的 seq 计数器、触发对端 anti-replay 状态迁移。仅在**明确**做多 Coordinator / 对等发现等特殊拓扑，且理解上述后果时才开启 feature。**优先用 `report` 上报数据**。

---

## 替换物理链路

任何"能发广播帧 + 能收广播帧"的物理链路都可以实现 `CommLink`：

```rust,ignore
use core::future::Future;
use comm::link::{CommLink, Packet};

pub struct MyEspNowLink { /* ... */ }

impl CommLink for MyEspNowLink {
    const MAX_FRAME_LEN: usize = 250;
    type SendError = MyErr;
    type RecvError = MyErr;
    type Addr = [u8; 6];
    const BROADCAST: Self::Addr = [0xFF; 6];

    async fn send(&mut self, dst: [u8; 6], bytes: &[u8]) -> Result<(), MyErr> {
        // 调用底层 esp-radio API
    }

    async fn recv(&mut self) -> Result<Packet<'_, [u8; 6]>, MyErr> {
        // 返回借用 impl 内部缓冲的 Packet
    }
}
```

**约定**：
- `send` 应"入队即返回"，不长时间 `.await`（embassy 单线程执行器不喜欢阻塞）
- `recv` 返回的切片指向 impl 内部缓冲；下一次 `recv()` 后失效

---

## Host 端集成测试

启用 `loopback` + `test-utils` feature 即可在 host 上跑端到端测试：

```rust,ignore
#![cfg(all(feature = "loopback", feature = "test-utils"))]

use comm::loopback::pair;

let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);
// a_send + a_recv 组成 notifier 端；b_send + b_recv 组成 receiver 端
```

需要**只测 Receiver `&self` API**（`report` / `send_frame` / …）时，门面已
link 无关，直接用 `Receiver::builder()` 拼一个实体即可（无需持有物理 link）：

```rust,ignore
let receiver: comm::Receiver = comm::Receiver::builder()
    .keyring(&KEYRING)
    .replay(&REPLAY)
    .response_signal(&RESP_SIG)
    .frame_signal(&FRAME_SIG)
    .command_signal(&CMD_SIG)
    .role_tag(*b"led")
    .mac(MAC_B)
    .my_id(&MY_ID)
    .command_handler(handle_command)
    .src(comm::CommandSource::Local)
    .build();
receiver.report(ResponseBody::BatterySnapshot { percent: 85 });
```

参考 `tests/integration.rs` 里的端到端场景。

---

## 错误处理与可观测性

### 错误类型

| 错误 | 来源 |
|---|---|
| `NotifierError` | Notifier 侧的顶层错误（`#[non_exhaustive]`） |
| `ReceiverError` | Receiver 侧的顶层错误（`#[non_exhaustive]`） |
| `LinkError<S, R>` | `CommLink` 的双向错误包装 |
| `KeyringError` / `ReplayCheckError` | key/anti-replay 相关 |

### `defmt` 日志埋点

开启 `defmt` feature 后，dispatch 路径的**静默丢弃点**会输出日志便于 field debug：

| 事件 | 级别 |
|---|---|
| 过短 packet（`< 2` bytes） | `trace` |
| 未知 magic / 长度不匹配 | `trace` |
| Command decode 失败（bad magic / length） | `trace` |
| Command decode 失败（HMAC / 版本） | `warn` |
| Anti-replay 拒绝 | `warn` |
| Frame decode 失败 | `trace` |
| `dest_mask` 过滤（未寻址本机） | `trace` |

---

## 设计原则

- **`no_std` by default** — 直接依赖 embassy 家族（`embassy-sync` / `embassy-time` / `embassy-futures`）；不做运行时无关抽象
- **`CommLink` 是唯一的可插拔点** — ESP-NOW / UART / loopback 各自实现
- **协议逻辑复用 `protocol`** — 本 crate 只负责编排
- **零 heap 分配** — 所有集合走 `heapless::Vec<T, N>`
- **编译期尺寸护栏** — `Frame` / `Command` / `CommandResponse` / `PeerInfo` 有 `size_of` 断言，防止意外膨胀
- **实用主义 builder** — 必填字段用 `Option` + `expect`，而非 typestate 展开 `2^n` impl
- **危险 API 加 feature 门控** — `endpoint-initiated-command` 是 opt-in 的显式选择

---

## 版本兼容性

- Rust：**1.88+**（`async fn` in trait + 若干 `edition2024` 特性）
- embassy：`embassy-sync 0.7` / `embassy-futures 0.1` / `embassy-time 0.5`
- MSRV 变更遵循 SemVer：0.x → 只在 major 变更时提升 MSRV；未来 1.0 后需 patch-safe

---

## 许可

MIT。详见 [`LICENSE`](../../LICENSE)。

[`CommLink`]: https://docs.rs/comm/latest/comm/trait.CommLink.html
[`Notifier`]: https://docs.rs/comm/latest/comm/struct.Notifier.html
[`Receiver`]: https://docs.rs/comm/latest/comm/struct.Receiver.html
[`OutboundCommand`]: https://docs.rs/comm/latest/comm/notifier/signals/struct.OutboundCommand.html
[`Coordinator`]: https://docs.rs/comm/latest/comm/type.Coordinator.html
[`Endpoint`]: https://docs.rs/comm/latest/comm/type.Endpoint.html
