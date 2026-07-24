# ESP-NOW 控制端参考实现

本文档提供一份"另一颗 ESP32 如何**主动给手柄发命令**"的可复制样板。
烧到控制端 ESP32 上即可给手柄下发 `LedBlink` / `SetSensitivity` /
`ShowToast` / `SetBatteryMode` 等控制命令，并收到手柄的 Ack / Error 响应。

> 🆕 **本样板已升级为使用 [`comm`](../crates/comm/) 门面**：不再手写
> nonce 监听 / seq 计数 / 收发派发，而是复用与手柄同源的 `comm::Notifier`
> 编排句柄——你只需实现一个 `comm::CommLink` 适配层（把 esp-radio 的
> ESP-NOW 收发两半包一下），其余交给门面的两条后台 loop。这与仓库里手柄侧的
> [`crates/controller/src/transport/esp_now/`](../crates/controller/src/transport/esp_now/)
> 是**同一套**用法。若你想了解不借助 `comm`、直接操作 wire 字节的底层做法，
> 见 [`crates/examples/controller-host-demo/`](../crates/examples/controller-host-demo/)。

> 👉 如果你只想**订阅手柄状态**（读按键 / 摇杆 / 电量），请参考
> [esp_now_receiver.md](./esp_now_receiver.md)。

> 🆕 **协议**（当前）：`Command` / `Response`  **24 B**（10 B payload）；
> `Frame`  **25 B**（4 B `dest_mask` 多接收方位图寻址）；
> 新增 `Announce` / `AssignId` 命令与 `AnnounceReply` 响应，支持接收方动态发现。
> 详见 [`protocol_air.md`](./protocol_air.md)。

## 📑 目录

- [ESP-NOW 控制端参考实现](#esp-now-控制端参考实现)
  - [📑 目录](#-目录)
  - [特性](#特性)
  - [⚠️ 生产环境部署提示](#️-生产环境部署提示)
  - [硬件要求](#硬件要求)
  - [控制端 Cargo.toml 参考](#控制端-cargotoml-参考)
  - [.cargo/config.toml / rust-toolchain.toml](#cargoconfigtoml--rust-toolchaintoml)
  - [完整示例 main.rs](#完整示例-mainrs)
  - [ESP-NOW → CommLink 适配层](#esp-now--commlink-适配层)
  - [关键实现细节](#关键实现细节)
    - [1. NonceHello 握手时机](#1-noncehello-握手时机)
    - [2. seq 严格递增（抗重放）](#2-seq-严格递增抗重放)
    - [3. Response 按 req\_seq 匹配](#3-response-按-req_seq-匹配)
    - [4. 多接收方寻址（dest\_mask）](#4-多接收方寻址dest_mask)
    - [5. Announce / AssignId 动态发现](#5-announce--assignid-动态发现)
  - [📦 Command 命令字典](#-command-命令字典)
  - [🚫 Response 错误码字典](#-response-错误码字典)
  - [🔄 密钥轮换（进阶）](#-密钥轮换进阶)
  - [烧录 \& 验证](#烧录--验证)
  - [FAQ](#faq)
    - [Q1: 手柄侧一直不响应，日志显示 `AuthFailed`](#q1-手柄侧一直不响应日志显示-authfailed)
    - [Q2: `NonceHello` 一直收不到](#q2-noncehello-一直收不到)
    - [Q3: 收到 Ack 但 LED 没闪](#q3-收到-ack-但-led-没闪)
    - [Q4: 广播的 Frame 部分 receiver 收不到](#q4-广播的-frame-部分-receiver-收不到)
  - [参考资料](#参考资料)

## 特性

- **HMAC-SHA256 鉴权**：命令帧携带 4 字节截断 tag，防止未授权设备发指令
- **Session Nonce 反重放**：手柄每次上电生成随机 nonce，控制端必须
  监听到 `NonceHello` 广播后才能签出合法命令
- **抗重放窗口**：手柄侧维护 64 位滑动窗口，seq 必须严格递增
- **密钥轮换**：支持 4 个并存的 `key_id`（可运维平滑切换）
- **命令 / 响应对齐**：控制端按 `req_seq == command.seq` 匹配响应，无阻塞
- **10 字节 payload**：Command / Response payload 为 10 B，
  能承载 MAC-48 等复杂载荷
- **Announce / AssignId**：controller 侧可通过 `Announce` 广播发现
  在线 receiver，并单播 `AssignId` 下发逻辑 `receiver_id`
- **`dest_mask` 位图寻址**：Frame 携带 32 位位图，可选择性下发到
  子集 receiver（bit-i = 1 ↔ `receiver_id == i` 处理该帧）

## ⚠️ 生产环境部署提示

本文档的示例代码直接引用了协议 crate 里**明文写死**的
[`SECRET_V1`](../crates/protocol/src/config.rs)：

```rust
pub const SECRET_V1: &[u8; SECRET_LEN] = b"esp32-controller-shared-key-v1!\0";
```

**这仅用于开发调试**。生产部署时必须：

1. **替换默认密钥**：通过 `build.rs` 从环境变量 / 外部密钥管理器读入
2. **控制固件分发渠道**：确保密钥不落地版本控制系统
3. **使用密钥轮换**：定期切换 `key_id`（见文末"密钥轮换"）

## 硬件要求

与接收端一致，见
[esp_now_receiver.md § 接收端硬件要求](./esp_now_receiver.md#接收端硬件要求)。

## 控制端 Cargo.toml 参考

新建 `esp32-controller-host` 项目：

```toml
[package]
name = "esp32-controller-host"
edition = "2024"
version = "0.1.0"

[dependencies]
# 复用本仓库的协议 crate（纯 no_std，无 esp-hal / embassy 等重依赖；
# 保证与手柄侧 Frame / Command / Response 布局 100% 一致）
#
# 引用方式二选一：
#   1) 本地 path（同一 monorepo / 二次开发调试）
#   2) git tag（推荐生产使用，锁定协议版本）
protocol = { path = "../controller/crates/protocol", default-features = false, features = ["defmt"] }
# 或：
# protocol = { git = "https://github.com/lf-wxp/controller", tag = "protocol-v0.2.0", default-features = false, features = ["defmt"] }

# 通信编排门面：Notifier / Receiver / CommLink / nonce 广播等（no_std，依赖 embassy）
# 与手柄侧同源，保证收发 / 抗重放 / 密钥管理逻辑 100% 一致。
comm = { path = "../controller/crates/comm", default-features = false, features = ["defmt"] }
# 或：
# comm = { git = "https://github.com/lf-wxp/controller", tag = "comm-v0.2.0", default-features = false, features = ["defmt"] }

esp-hal = { version = "~1.1.0", features = ["defmt", "esp32", "unstable"] }
esp-rtos = { version = "0.3.0", features = [
  "defmt", "embassy", "esp-alloc", "esp-radio", "esp32",
] }
esp-radio = { version = "0.18.0", features = [
  "defmt", "esp-alloc", "esp-now", "esp32", "unstable", "wifi",
] }
esp-alloc = { version = "0.10.0", features = ["defmt"] }
esp-bootloader-esp-idf = { version = "0.5.0", features = ["defmt", "esp32"] }

embassy-executor = { version = "0.10.0", features = ["defmt"] }
embassy-time = { version = "0.5.0", features = ["defmt"] }
embassy-sync = { version = "0.7.2", features = ["defmt"] }
defmt = "1.0.1"
panic-rtt-target = { version = "0.2.0", features = ["defmt"] }
rtt-target = { version = "0.6.2", features = ["defmt"] }
static_cell = "2.1.1"
portable-atomic = { version = "1", features = ["require-cas"] }
```

## .cargo/config.toml / rust-toolchain.toml

与接收端完全一致，见
[esp_now_receiver.md § .cargo/config.toml](./esp_now_receiver.md#cargoconfigtoml)。

## 完整示例 main.rs

整个收发编排交给 `comm::Notifier` 门面：`builder()...build()` 一次拿到
`&'static` 句柄后，两条后台 loop（`run_broadcast_loop` / `run_receive_loop`）
各吃一个 ESP-NOW link 端，主循环用 `send_command` 下发命令。手柄的
`NonceHello` 由门面的 Response 回调 `on_response` 采纳，Ack / Error 也在同一
回调里观察——**无需再手写 seq 计数、nonce 监听、收发派发**。

> host 只**发命令 + 观察 Response**，不作为可被发现的 receiver，因此**不**调用
> `with_command_handler`（那是"双身份"设备才需要的）。`AnnounceReply` 会被
> `comm` 内部消费（`upsert` + 回 `AssignId`），不会进 `on_response`。

```rust
#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use portable_atomic::{AtomicBool, Ordering};
use static_cell::StaticCell;

// comm 门面 + 出站信号类型
use comm::notifier::signals::{CommandOutChannel, FrameSignal, ResponseChannel};
use comm::{CommandBody, CommandResponse, Keyring, Notifier, PeerRegistry, ReplayGuard, ResponseBody};
use protocol::auth::init_session_nonce;

// ESP-NOW ↔ comm::CommLink 适配层（见下一节「ESP-NOW → CommLink 适配层」）
mod link;
use link::{EspNowRecvLink, EspNowSendLink};

esp_app_desc!();

// ============================================================
// 全局共享状态（fn 指针 handler 无法捕获环境 → 用 static 承载）
// ============================================================

/// 是否已采纳至少一次 NonceHello —— 未就绪前不发命令
static NONCE_READY: AtomicBool = AtomicBool::new(false);

// comm 门面所需的运行时状态（均为 const fn，可直接 static 初始化）：
static KEYRING: Keyring = Keyring::new(); // 出站 Command 的 key_id + seq 计数器
static PEERS: PeerRegistry = PeerRegistry::new(); // host 一般不发现 receiver，但 builder 必填
static REPLAY: ReplayGuard = ReplayGuard::new(); // host 不收业务命令，但 builder 必填
static FRAME_SIG: FrameSignal = FrameSignal::new(); // 出站 Frame（host 不发帧，占位）
static CMD_SIG: CommandOutChannel = CommandOutChannel::new(); // 出站 Command 有界队列
static RESP_SIG: ResponseChannel = ResponseChannel::new(); // 出站 Response 有界队列（占位）

/// Notifier 门面单例（`build()` 后 `init` 进来，两条 loop + 主循环共享）
static NOTIFIER: StaticCell<Notifier> = StaticCell::new();

// ============================================================
// Response 回调：门面收到非 AnnounceReply 的 Response 时调用
// ============================================================

/// 采纳手柄 nonce + 观察 Ack / Error / 电量。
///
/// 关键点：手柄是 `NonceHello` 的**发布方**，本 host 必须采纳同一个 nonce，
/// 之后 `send_command` 才能签出手柄认可的 HMAC。`comm` 的 Coordinator 接收
/// 路径**不会**自动采纳他人 nonce（避免 Coordinator 误采），因此这里在
/// 回调里手动 `init_session_nonce`。
fn on_response(resp: &CommandResponse) {
  match resp.body {
    ResponseBody::NonceHello { nonce } => {
      init_session_nonce(nonce);
      if !NONCE_READY.swap(true, Ordering::Relaxed) {
        info!("nonce ready: 0x{:08x}", nonce);
      }
    }
    ResponseBody::Ack => info!("✓ ACK for seq={}", resp.req_seq),
    ResponseBody::Error(code) => warn!("✗ ERR seq={} code={:?}", resp.req_seq, code),
    ResponseBody::BatterySnapshot { percent } => info!("battery: {}%", percent),
    // AnnounceReply 已由 comm 内部消费（upsert + 回 AssignId），不会走到这里；
    // 显式列出避免非穷举 warning。
    ResponseBody::AnnounceReply { .. } => {}
  }
}

// ============================================================
// 主入口
// ============================================================

#[esp_rtos::main]
async fn main(spawner: Spawner) {
  let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
  esp_alloc::heap_allocator!(size: 96 * 1024);
  info!("controller-host (comm-backed) starting");

  // 1. 初始化 Wi-Fi + ESP-NOW
  let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
  esp_rtos::start(timg0.timer0);
  let wifi_ctrl = esp_radio::init().unwrap();
  let (mut wifi, _) = esp_radio::wifi::new(&wifi_ctrl, peripherals.WIFI).expect("wifi init failed");
  let _ = wifi.set_mode(esp_radio::wifi::WifiMode::Sta);
  let _ = wifi.start_async().await;
  let esp_now =
    esp_radio::esp_now::EspNow::new(&wifi_ctrl, peripherals.WIFI).expect("esp-now init failed");
  let (manager, sender, receiver) = esp_now.split();
  info!("esp-now up, channel = 1 (default)");

  // manager 需 'static：send-link 在单播时惰性 add_peer（host 一般只广播，但接口需要它）
  static MANAGER: StaticCell<esp_radio::esp_now::EspNowManager<'static>> = StaticCell::new();
  let manager: &'static _ = MANAGER.init(manager);

  // 2. 把 esp-now 的收发两半包成 comm::CommLink（send / recv 分属两个 task）
  let send_link = EspNowSendLink::new(sender, manager);
  let recv_link = EspNowRecvLink::new(receiver);

  // 3. 组装 Notifier 门面（link 无关）；host 只发命令 + 观察 Response，
  //    故不设 with_command_handler（不作为可被发现的 receiver）。
  let notifier: &'static Notifier = NOTIFIER.init(
    Notifier::builder()
      .keyring(&KEYRING)
      .peers(&PEERS)
      .replay(&REPLAY)
      .frame_signal(&FRAME_SIG)
      .command_signal(&CMD_SIG)
      .response_signal(&RESP_SIG)
      .with_response_handler(on_response)
      .build(),
  );

  // 4. 两条后台 loop：send / recv 端各喂一个 link
  spawner.must_spawn(bcast_task(notifier, send_link));
  spawner.must_spawn(recv_task(notifier, recv_link));

  // 5. 命令发送 loop：等 nonce 就绪后周期下发 LedBlink
  while !NONCE_READY.load(Ordering::Relaxed) {
    Timer::after(Duration::from_millis(200)).await;
  }
  info!("nonce ready → start sending commands");
  loop {
    // send_command 自动分配 seq（keyring）+ 用当前 session nonce 签 HMAC + 入队
    notifier.send_command(CommandBody::LedBlink {
      led_idx: 0,
      count: 3,
      period_ms: 100,
    });
    info!("sent LedBlink (broadcast)");
    Timer::after(Duration::from_secs(3)).await;
  }
}

// embassy #[task] 宏不能吃泛型 async fn，故各包一层具体签名。
#[embassy_executor::task]
async fn bcast_task(n: &'static Notifier, link: EspNowSendLink) -> ! {
  n.run_broadcast_loop(link).await
}

#[embassy_executor::task]
async fn recv_task(n: &'static Notifier, link: EspNowRecvLink) -> ! {
  n.run_receive_loop(link).await
}
```

## ESP-NOW → CommLink 适配层

`comm::CommLink` 是 `comm` 唯一的硬件抽象点。下面把 esp-radio 的 ESP-NOW
收发两半各包成一个 link 端（`send` / `recv` 都要 `&mut self`，不能被两个 task
同时借用，故拆两半）。这份适配层与仓库手柄侧的
[`crates/controller/src/transport/esp_now/link.rs`](../crates/controller/src/transport/esp_now/link.rs)
**同款**，可直接复制为本项目的 `src/link.rs`：

```rust
use comm::{CommLink, Packet};
use esp_radio::esp_now::{
  BROADCAST_ADDRESS, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface, PeerInfo,
};

/// ESP-NOW MTU（250 B payload）；真实帧 ≤ 26 B，此上限只是硬边界保护。
const ESP_NOW_MTU: usize = 250;

/// send-only 端的发送错误
#[derive(Debug, defmt::Format)]
pub enum SendError {
  /// 底层 esp-radio 发送失败
  Radio,
  /// 单播目标 add_peer 失败（ESP-NOW peer 表满，上限约 20）
  PeerFull,
}

/// 被误用方向时的错误（send-only 端被 recv / recv-only 端被 send）
#[derive(Debug, defmt::Format)]
pub enum WrongDirection {
  /// 该端不支持此方向
  Unsupported,
}

// ---- send-only：EspNowSendLink（喂给 run_broadcast_loop）----

pub struct EspNowSendLink {
  sender: EspNowSender<'static>,
  manager: &'static EspNowManager<'static>,
}

impl EspNowSendLink {
  #[must_use]
  pub const fn new(
    sender: EspNowSender<'static>,
    manager: &'static EspNowManager<'static>,
  ) -> Self {
    Self { sender, manager }
  }

  /// 确保单播目标已在 peer 表中（幂等）；广播地址由 esp-radio 自动登记。
  fn ensure_peer(&self, dst: &[u8; 6]) -> Result<(), SendError> {
    if *dst == BROADCAST_ADDRESS || self.manager.peer_exists(dst) {
      return Ok(());
    }
    self
      .manager
      .add_peer(PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: *dst,
        lmk: None,
        channel: None,
        encrypt: false,
      })
      .map_err(|_| SendError::PeerFull)
  }
}

impl CommLink for EspNowSendLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = SendError;
  type RecvError = WrongDirection;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST_ADDRESS;

  async fn send(&mut self, dst: Self::Addr, bytes: &[u8]) -> Result<(), Self::SendError> {
    self.ensure_peer(&dst)?;
    self
      .sender
      .send_async(&dst, bytes)
      .await
      .map_err(|_| SendError::Radio)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // send-only：broadcast_loop 不会调用；若被误调，loop 会忽略 Err 并 continue
    Err(WrongDirection::Unsupported)
  }
}

// ---- recv-only：EspNowRecvLink（喂给 run_receive_loop）----

pub struct EspNowRecvLink {
  receiver: EspNowReceiver<'static>,
  scratch: [u8; ESP_NOW_MTU],
  scratch_len: usize,
  scratch_src: [u8; 6],
}

impl EspNowRecvLink {
  #[must_use]
  pub const fn new(receiver: EspNowReceiver<'static>) -> Self {
    Self {
      receiver,
      scratch: [0; ESP_NOW_MTU],
      scratch_len: 0,
      scratch_src: [0; 6],
    }
  }
}

impl CommLink for EspNowRecvLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = WrongDirection;
  type RecvError = WrongDirection;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST_ADDRESS;

  async fn send(&mut self, _dst: Self::Addr, _bytes: &[u8]) -> Result<(), Self::SendError> {
    Err(WrongDirection::Unsupported)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // ReceivedData::data() 的切片生命周期与 pkt 绑定；copy 到 self.scratch 后
    // 让借用挂到 self 上，调用方（run_receive_loop）才能跨 .await 安全持有。
    let pkt = self.receiver.receive_async().await;
    let data = pkt.data();
    let len = data.len().min(ESP_NOW_MTU);
    self.scratch[..len].copy_from_slice(&data[..len]);
    self.scratch_len = len;
    self.scratch_src = pkt.info.src_address;
    Ok(Packet {
      src: self.scratch_src,
      data: &self.scratch[..self.scratch_len],
    })
  }
}
```

## 关键实现细节

### 1. NonceHello 握手时机

手柄每 5 秒广播一次 `NonceHello`（[esp_now/mod.rs](../crates/controller/src/transport/esp_now/mod.rs)
的 `nonce_broadcast_task`）。控制端上电后**最长等待约 5 秒**才能发出第一条
合法命令 —— 采纳动作发生在门面的 Response 回调 `on_response` 里
（`ResponseBody::NonceHello { nonce } → init_session_nonce(nonce)`），主循环用
`NONCE_READY` AtomicBool 兜底、就绪后才开始发命令：

```rust
while !NONCE_READY.load(Ordering::Relaxed) {
  Timer::after(Duration::from_millis(200)).await;
}
```

> ⚠️ `comm` 的 Coordinator（`Notifier`）接收路径**不会**自动采纳他人 nonce
> （避免"nonce 发布方误采别人的 nonce"）。本 host 虽用 `Notifier` 门面，但角色
> 是"采纳手柄 nonce 的命令方"，因此必须在 `on_response` 里**手动**采纳。

### 2. seq 严格递增（抗重放）

手柄侧 [`AntiReplayWindow`](../crates/protocol/src/replay.rs) 对每个
`key_id` 独立维护 64 位滑动窗口。控制端 seq 必须**严格单调递增**。门面已把
seq 管理收拢进 [`comm::Keyring`](../crates/comm/src/keyring.rs)：`send_command`
内部 `keyring.next_seq()` 原子 `fetch_add`，恒返回 `>= 1` 且绕回时自动跳过保留的 0：

- ✅ `1, 2, 3, 4, ...` 全部接受
- ✅ `1, 2, 5, 3, 4`（乱序但落在窗口内）全部接受
- ❌ 重启后从 1 重新发 → **老 seq 被拒绝**（AuthFailed 或 Replay）

**生产环境**建议把 `Keyring` 的 tx_counter 定期持久化到 NVS，重启后
`peek_counter` / 自定义流程恢复。参考 [hal/persist.rs](../crates/controller/src/hal/persist.rs)
的 `PersistentConfig`（含 `replay_windows` 字段）——控制端可模仿此实现。

### 3. Response 按 req_seq 匹配

`CommandResponse.req_seq` 携带对应请求 Command 的 seq。门面把所有非
`AnnounceReply` 的 Response 都交给 `on_response` 回调。若需要"请求-响应对齐"
（例如超时重试），可在回调里维护一个 `pending_seqs` 表（`req_seq → Deadline`），
匹配到 Ack / Error 后移除。

本例简化处理：直接打印 Ack/Err，不做超时重试。

### 4. 多接收方寻址（dest\_mask）

Frame  `dest_mask: u32`，位图指定"哪些 `receiver_id` 应处理该帧"：

| `dest_mask`        | 语义                                              |
| ------------------ | ------------------------------------------------- |
| `0xFFFF_FFFF`      | **广播**（默认；等价老版本行为）                   |
| `1 << id`          | 单播到 `receiver_id == id`                        |
| `mask_a \| mask_b` | 多播到 `receiver_id` 属于 mask 中 bit=1 的子集    |
| `0`                | 静默丢弃（用于"暂停下发"）                        |

**构造示例**（假设 host 想同时给 `receiver_id=1` 与 `receiver_id=5` 下发）：

```rust
use protocol::frame::{Frame, BROADCAST_DEST_MASK};
use protocol::state::GamepadState;

let mask = (1_u32 << 1) | (1_u32 << 5);
let frame = Frame::with_dest(seq, GamepadState::EMPTY, mask);

// receiver 侧过滤（伪代码）：
// if !frame.is_addressed_to(MY_RECEIVER_ID) { continue; }
```

**注意**：`dest_mask` 是 Frame 新增的 32-bit 位图寻址字段（帧总长 25 字节），与 Command 无关。
Command 的多播由**目标 MAC** + `CommandBody::AssignId.mac` 字段实现。

### 5. Announce / AssignId 动态发现

起手柄侧维护 [`PeerRegistry`](../crates/comm/src/peer_registry.rs)（全局单例位于
[`crates/controller/src/lib.rs`](../crates/controller/src/lib.rs) 的 `pub static REGISTRY`），通过 Announce
广播动态发现 receiver、通过 AssignId 下发逻辑 `receiver_id`：

```text
手柄 ─► Announce (broadcast) ────────────────► 所有 receiver
   ◄─── AnnounceReply(mac, rssi, role) ────── receiver A
   ─── AssignId(mac_A, receiver_id=0) (bcast, receiver A 自匹配) ─► receiver A
```

如果 host（本文档角色）也想充当"receiver 发现方"（不是典型场景），**用门面几乎零成本**：
`comm::Notifier` 已内置整套发现编排——

1. `notifier.discover()` 广播一次 `Announce`
2. 收到的 `AnnounceReply` 由门面**自动** `PEERS.upsert(...)` 并回单播 `AssignId`（无需你写）
3. 用 `notifier.peers()` 拿 `PeerInfo` 快照渲染 / 选择，`notifier.send_command_to(id, ..)` 定向下发
4. （可选）发现前 `PEERS.prune(now, ttl)` 淘汰长时间未上报的 receiver

**典型 host 场景无需实现**：这个环节主要由手柄侧完成；host 侧只需
发命令给手柄本体（MAC 固定，无 receiver_id 概念）。

## 📦 Command 命令字典

> 💡 完整的 Command **帧结构 / 字节布局**（含 magic / version_byte / hmac 偏移）
> 见 [`protocol_air.md § Command`](./protocol_air.md#2-command控制命令24-b)。
> 本节仅列出**面向开发者的类型字典**，方便在编写命令时快速查字段名。

对应 [`CommandBody`](../crates/protocol/src/command.rs) 各变体（10 B payload）：

| 变体              | 载荷字段                                                 | 用途                              |
| ----------------- | -------------------------------------------------------- | --------------------------------- |
| `Nop`             | ―                                                        | 心跳 / 连接性检查                 |
| `LedBlink`        | `led_idx: u8, count: u8, period_ms: u16`                 | 让 LED 闪烁 N 次                  |
| `SetSensitivity`  | `joy_scale: u16, knob_scale: u16` (0..=1000)             | 修改摇杆 / 旋钮灵敏度             |
| `ShowToast`       | `len: u8, bytes: [u8; 5]` (ASCII)                        | OLED 底部短提示（≤5 字节）        |
| `SetBatteryMode`  | `simulate: bool`                                         | 切换电池模拟 / 真实模式           |
| **🆕 `Announce`** | ―（payload 全 0 保留）                                   | 广播邀请所有 receiver 回 Reply    |
| **🆕 `AssignId`** | `mac: [u8; 6], receiver_id: u8`                          | 下发逻辑 ID（receiver 自匹配 MAC）|

**构造示例**：

```rust
// 灵敏度设为 80%
CommandBody::SetSensitivity {
  joy_scale: 800,
  knob_scale: 1000,
}

// 显示 "HI!"
CommandBody::ShowToast {
  len: 3,
  bytes: *b"HI!\0\0",
}

// 切到真实电池测量
CommandBody::SetBatteryMode { simulate: false }

// 广播 Peer 发现
CommandBody::Announce

// 给 MAC 为 aa:bb:cc:dd:ee:ff 的 receiver 分配逻辑 ID = 3
CommandBody::AssignId {
  mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
  receiver_id: 3,
}
```

## 🚫 Response 错误码字典

对应 [`ErrorCode`](../crates/protocol/src/response.rs)：

| 错误码             | 触发场景                                   |
| ------------------ | ------------------------------------------ |
| `InvalidArgument`  | 参数越界（如 `LedBlink { led_idx: 99 }`）  |
| `Unsupported`      | 手柄不支持该命令（例如老固件）             |
| `Busy`             | 手柄内部忙（如 LED 特效队列已满）          |

**Response 变体**：

| 变体              | 载荷字段                                                  | 用途                              |
| ----------------- | --------------------------------------------------------- | --------------------------------- |
| `Ack` / `Error`   | ―（原有）                                                 | 命令执行反馈                      |
| `BatterySnapshot` | `percent: u8`                                             | 电量快照                          |
| `NonceHello`      | `nonce: u32`                                              | Session nonce 广播                |
| **🆕 `AnnounceReply`** | `mac: [u8; 6], rssi_dbm: i8, role_tag: [u8; 3]`      | receiver 上报身份 + RSSI          |

## 🔄 密钥轮换（进阶）

手柄侧密钥环
[`SHARED_SECRETS`](../crates/protocol/src/config.rs) 支持 4 个并存 slot：

```rust
pub const SHARED_SECRETS: [Option<&'static [u8; 32]>; KEY_SLOTS] =
  [Some(SECRET_V1), Some(SECRET_V2), None, None];
```

**平滑切换步骤**：

1. **Day 0**：控制端全部用 `KeyId::DEFAULT`（key_id=0，对应 SECRET_V1）
2. **Day 15**：控制端灰度切到 `KeyId::new(1).unwrap()`（对应 SECRET_V2）；
   手柄侧同时接受两个 key 的命令
3. **Day 30**：控制端全量切到 key_id=1；手柄侧下发配置把 slot 0 改为 `None`
4. 老密钥彻底停用

**构造示例**：

```rust
let key_id = KeyId::new(1).expect("key_id 1 within KEY_SLOTS");
let cmd = Command::with_key(seq, key_id, CommandBody::Nop);
```

## 烧录 & 验证

1. 烧好控制端 ESP32，打开 RTT 观察 defmt 日志
2. 烧好手柄，上电后每 5 秒会广播一次 `NonceHello`
3. 控制端约 5 秒内应打印 `nonce ready: 0x...`
4. 每 3 秒发一条 `LedBlink` → 手柄 LED 应闪烁 3 次
5. 手柄侧返回 Ack → 控制端打印 `✓ ACK for seq=N`（由 `on_response` 回调打印）

## FAQ

### Q1: 手柄侧一直不响应，日志显示 `AuthFailed`

**可能原因**：
- 控制端与手柄的 **HMAC 密钥不一致** → 检查两侧 `SECRET_V1` / `SECRET_V2`
- 控制端**尚未收到 NonceHello** 就发了命令 → 检查 `NONCE_READY` 逻辑
- 控制端 seq 回退（例如重启后从 1 重新开始）→ 从 NVS 恢复 seq 或换 key_id
- 🆕 **协议版本不一致**：两端 `protocol` 依赖版本不一致会得到
  `UnsupportedVersion` 而不是 `AuthFailed`；确保两端依赖同一版本

### Q2: `NonceHello` 一直收不到

**可能原因**：
- 手柄未开机 / 未初始化 ESP-NOW
- Wi-Fi 频道不一致 → 参考
  [esp_now_receiver.md § 频道对齐](./esp_now_receiver.md#频道对齐)
- 手柄侧 `nonce_broadcast_task` 未启动 → 检查
  [transport/esp_now/mod.rs](../crates/controller/src/transport/esp_now/mod.rs)

### Q3: 收到 Ack 但 LED 没闪

**可能原因**：
- 手柄 LED 特效队列已满 → 应收到 `Error(Busy)` 而不是 Ack；检查是否
  真的是 Ack
- `led_idx` 越界 → 应收到 `Error(InvalidArgument)`；本手柄目前只有 1 个
  LED（idx=0），传 1 会失败

### Q4: 广播的 Frame 部分 receiver 收不到

**可能原因**：
- 🆕 手柄进入 Selecting 模式后**只广播给选中的 receiver**（`dest_mask` 非全 1）
  → 未被选中的 receiver 会静默丢弃该帧；这是**预期行为**
- receiver 侧 `RECEIVER_ID` 未和手柄侧 AssignId 分配的一致
  → 长按 Switch 重新触发 Announce/AssignId 握手
- receiver 侧 `dest_mask` 过滤逻辑写错 → 参考
  [esp_now_receiver.md § dest_mask 过滤](./esp_now_receiver.md#dest_mask-过滤)

## 参考资料

- **空中协议对照** → [`protocol_air.md`](./protocol_air.md)（3 种帧 / 字节布局 / 安全模型 / 时间表）
- 空中协议 3 种 magic → [protocol_air.md § 空气中的 3 种帧](./protocol_air.md#空气中的-3-种帧)
- 协议 crate 源码 → [crates/protocol/](../crates/protocol/)
- 协议 crate 使用指南 → [crates/protocol/USAGE.md](../crates/protocol/USAGE.md)
- 手柄侧命令分发 → [crates/controller/src/transport/control.rs](../crates/controller/src/transport/control.rs)
- 🆕 通信编排门面（`Notifier` / `Receiver` / `CommLink`）→ [crates/comm/](../crates/comm/)（用法总览见 [crates/comm/README.md](../crates/comm/README.md)）
- 🆕 手柄侧 Peer 目录管理 → [crates/comm/src/peer_registry.rs](../crates/comm/src/peer_registry.rs)（全局单例在 [crates/controller/src/lib.rs](../crates/controller/src/lib.rs) 的 `REGISTRY`）
- 🆕 手柄侧门面接线（`init_notifier` / `discover` / 两条 loop task）→ [crates/controller/src/transport/esp_now/mod.rs](../crates/controller/src/transport/esp_now/mod.rs)
- 🆕 ESP-NOW → `CommLink` 适配层（本文档「适配层」章节同款）→ [crates/controller/src/transport/esp_now/link.rs](../crates/controller/src/transport/esp_now/link.rs)
- Dashboard 参考实现（BLE 版）→ [crates/dashboard/src/bluetooth.rs](../crates/dashboard/src/bluetooth.rs)
- **纯 host 侧协议交互 demo（不借助 comm，直接操作 wire 字节）** → [crates/examples/controller-host-demo/](../crates/examples/controller-host-demo/)
