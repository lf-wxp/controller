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
| 拥有 `Selector` | ✅ 决定下发目标 | ❌ 无决策权 |
| 主动 `discover()` | ✅ 发起会话 | ❌ 只能被动响应 |
| 首次遇到新 peer 时回 `AssignId` | ✅ 自动 | ❌ 只接收 `AssignId` |
| 主动 `send_frame` / `report` | ✅ | ✅ |
| 主动 `send_command` | ✅ | ⚠️ 需 opt-in feature |
| 处理入站 `Command` | ✅（双身份可选）| ✅（必填 `command_handler`）|
| 处理入站 `Response`（含 receiver 上报） | ✅（可选 `response_handler`）| —— |

> **一句话记忆**：`Notifier` 主导会话与拓扑；`Receiver` 是被协调的叶子节点，即便它也能主动上报数据。

为方便新代码选择更精确的命名，crate 提供两个 zero-cost 类型别名：

```rust
pub type Coordinator<L> = Notifier<L>;
pub type Endpoint<L> = Receiver<L>;
```

新老写法完全互通。

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

### Coordinator 端（controller / notifier）

```rust,ignore
use core::sync::atomic::AtomicU8;
use comm::prelude::*;
use comm::notifier::signals::{FrameSignal, CommandOutSignal, ResponseSignal};

// 1. 用户 crate 里声明 static 状态
static KEYRING: Keyring = Keyring::new();
static PEERS: PeerRegistry = PeerRegistry::new();
static REPLAY: ReplayGuard = ReplayGuard::new();
static SELECTOR: Selector = Selector::new();
static FRAME_SIG: FrameSignal = FrameSignal::new();
static CMD_SIG: CommandOutSignal = CommandOutSignal::new();
static RESP_SIG: ResponseSignal = ResponseSignal::new();

// 2. 用 builder 组装
let notifier = Notifier::builder()
    .link(my_link)                 // 必填：自定义 CommLink 实现
    .keyring(&KEYRING)
    .peers(&PEERS)
    .replay(&REPLAY)
    .selector(&SELECTOR)
    .frame_signal(&FRAME_SIG)
    .command_signal(&CMD_SIG)
    .response_signal(&RESP_SIG)
    .build();

// 3. spawn 两个后台任务（broadcast + receive）
// 具体 spawn 代码见 `notifier::run_broadcast_loop` / `notifier::run_receive_loop`

// 4. 主循环 API
notifier.discover();                    // 主动发起一次发现
for peer in notifier.peers() { /* ... */ } // peers() 直接返回 PeerInfo 快照 Vec
notifier.select_targets(0b0000_0011);   // 选择 receiver 0 + 1
notifier.send_frame(&frame);            // 广播状态帧
```

### Endpoint 端（led / motor / srv…）

```rust,ignore
use core::sync::atomic::AtomicU8;
use comm::prelude::*;
use comm::notifier::signals::{FrameSignal, CommandOutSignal, ResponseSignal};

static KEYRING: Keyring = Keyring::new();
static REPLAY: ReplayGuard = ReplayGuard::new();
static FRAME_SIG: FrameSignal = FrameSignal::new();
static CMD_SIG: CommandOutSignal = CommandOutSignal::new();
static RESP_SIG: ResponseSignal = ResponseSignal::new();
static MY_ID: AtomicU8 = AtomicU8::new(u8::MAX); // UNASSIGNED_ID

fn handle_command(_src: CommandSource, cmd: &Command) -> CommandOutcome {
    match cmd.kind {
        CommandBody::LedBlink { .. } => {
            turn_on_led();
            CommandOutcome::Ok
        }
        _ => CommandOutcome::Err(ErrorCode::Unsupported),
    }
}

let receiver = Receiver::builder()
    .link(my_link)
    .keyring(&KEYRING)
    .replay(&REPLAY)
    .response_signal(&RESP_SIG)
    .frame_signal(&FRAME_SIG)
    .command_signal(&CMD_SIG)
    .role_tag(*b"led")
    .mac(MY_MAC)
    .my_id(&MY_ID)
    .command_handler(handle_command)
    .build();

// spawn `receiver::run_broadcast_loop` + `receiver::run_receive_loop`
```

> AnnounceReply / Ack / HMAC / anti-replay 全部由 crate 自动完成，业务只关心 `handle_command`。

---

## API 全景

### 门面

| 类型 | 作用 |
|---|---|
| [`Notifier<L>`] / [`Coordinator<L>`] | 发送端门面，持有 `PeerRegistry` / `Selector` |
| [`Receiver<L>`] / [`Endpoint<L>`] | 接收端门面，持有 `command_handler` |
| [`CommLink`] | **唯一**硬件抽象 trait |

### Notifier 方法

| 类别 | 方法 |
|---|---|
| 主动出站 | `send_frame(&Frame)` / `send_command(CommandBody)` / `report(ResponseBody)` |
| 发现 | `discover()` — 广播一次 `Announce` |
| Peer 目录 | `peers()` — 返回 `PeerInfo` 只读快照 `Vec`（非借出 registry） |
| 目标选择 | `select_targets(mask)` / `selector()` |
| Getter | `response_signal()` / `keyring()` |

### Receiver 方法

| 类别 | 方法 |
|---|---|
| 主动出站 | `report(ResponseBody)` / `send_frame(&Frame)` / `send_command(_)` ⚠️ |
| 状态 | `assigned_id()` — 已分配的 `receiver_id`（`u8::MAX` 表未分配） |
| Getter | `response_signal()` / `keyring()` / `my_mac()` / `role_tag()` |

### 后台 loop

| 函数 | 作用 |
|---|---|
| `notifier::run_broadcast_loop` | 三路 select（Frame + Command + Response） → `CommLink::send` |
| `notifier::run_receive_loop` | `CommLink::recv` → 派发（Response upsert peers / 可选 Command 双身份） |
| `receiver::run_broadcast_loop` | 复用 notifier 侧同名实现（一份代码两处用） |
| `receiver::run_receive_loop` | `CommLink::recv` → 派发（Announce 自动回 / AssignId 写 `my_id` / 业务 handler） |
| `notifier::run_nonce_broadcast_loop` | 周期广播 session nonce（K3 密钥派生的组成部分） |

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
| `serde` | ❌ | 与 `controller-protocol/serde` 联动，`PeerInfo` 等可 (de)serialize | Dashboard / gRPC bridge |
| `loopback` | ❌ | 内置一个 `std::sync::mpsc` 版 `CommLink`（`LoopbackLink::pair`） | host 端集成测试 / 演示 |
| `test-utils` | ❌ | 暴露 `DummyLink` + `test_receiver_from_parts` 等测试 fixture | 集成测试 / 依赖侧单元测试 |
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

## Endpoint 主动上报

Receiver 侧的**上报通道** = `Receiver::report(ResponseBody)`：

```rust,ignore
// 每 30s 上报一次电量
receiver.report(ResponseBody::BatterySnapshot { percent: 85 });
```

**语义**：
- `req_seq = 0`，表示"非请求触发"
- `key_id` 使用 keyring 当前 active key
- 覆盖式：若上一条上报还没被 `run_broadcast_loop` 消费，本次会覆盖它

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

需要**只测 Receiver `&self` API**（`report` / `send_frame` / …）而不消费真实 link 时，用 `test_receiver_from_parts`：

```rust,ignore
let receiver: comm::Receiver<comm::link::DummyLink> =
    comm::receiver::test_receiver_from_parts(
        &KEYRING, &REPLAY, &RESP_SIG, &FRAME_SIG, &CMD_SIG,
        *b"led", MAC_B, &MY_ID, handle_command,
    );
receiver.report(ResponseBody::BatterySnapshot { percent: 85 });
```

参考 `tests/integration.rs` 里的 8 个端到端场景。

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
- **协议逻辑复用 `controller-protocol`** — 本 crate 只负责编排
- **零 heap 分配** — 所有集合走 `heapless::Vec<T, N>`
- **编译期尺寸护栏** — `Frame` / `Command` / `CommandResponse` / `PeerInfo` 有 `size_of` 断言，防止意外膨胀
- **实用主义 builder** — 8 个必填字段用 `Option` + `expect`，而非 typestate 展开 `2^8` impl
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
[`Notifier<L>`]: https://docs.rs/comm/latest/comm/struct.Notifier.html
[`Receiver<L>`]: https://docs.rs/comm/latest/comm/struct.Receiver.html
[`Coordinator<L>`]: https://docs.rs/comm/latest/comm/type.Coordinator.html
[`Endpoint<L>`]: https://docs.rs/comm/latest/comm/type.Endpoint.html
