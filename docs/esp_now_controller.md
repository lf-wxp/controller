# ESP-NOW 控制端参考实现

本文档提供一份"另一颗 ESP32 如何**主动给手柄发命令**"的可复制样板。
烧到控制端 ESP32 上即可给手柄下发 `LedBlink` / `SetSensitivity` /
`ShowToast` / `SetBatteryMode` 等控制命令，并收到手柄的 Ack / Error 响应。

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

```rust
#![no_std]
#![no_main]

use defmt::{error, info, warn};
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::clock::CpuClock;
use portable_atomic::{AtomicBool, AtomicU32, Ordering};

// 协议层：与手柄 100% 对齐（24 B Command / Response，25 B Frame）
use protocol::{
  auth::{init_session_nonce, KeyId},
  command::{encode_command, Command, CommandBody, COMMAND_LEN},
  frame::FRAME_LEN,
  response::{decode_response, ResponseBody, ResponseDecodeError, RESPONSE_LEN},
};

esp_app_desc!();

// ============================================================
// 全局共享状态
// ============================================================

/// 是否已经收到过至少一次 NonceHello —— 未收到前不发命令
static NONCE_READY: AtomicBool = AtomicBool::new(false);

/// 单调递增的 seq 计数器（每个 key_id 独立，本例只用 KeyId::DEFAULT）
///
/// 生产环境应把这个值持久化到 NVS，重启后从中恢复；否则重启后
/// 首条命令可能被手柄侧的抗重放窗口拒绝。
static NEXT_SEQ: AtomicU32 = AtomicU32::new(1);

/// 收到 Response 的通知（用于匹配请求 seq）
static RESPONSE_SIGNAL: Signal<CriticalSectionRawMutex, protocol::response::CommandResponse> =
  Signal::new();

// ============================================================
// 主入口
// ============================================================

#[esp_rtos::main]
async fn main(spawner: Spawner) {
  let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
  esp_alloc::heap_allocator!(size: 96 * 1024);

  info!("controller-host starting");

  // 1. 初始化 Wi-Fi + ESP-NOW
  let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
  esp_rtos::start(timg0.timer0);

  let wifi_ctrl = esp_radio::init().unwrap();
  let (mut wifi, _) =
    esp_radio::wifi::new(&wifi_ctrl, peripherals.WIFI).expect("wifi init failed");
  let _ = wifi.set_mode(esp_radio::wifi::WifiMode::Sta);
  let _ = wifi.start_async().await;

  let esp_now = esp_radio::esp_now::EspNow::new(&wifi_ctrl, peripherals.WIFI)
    .expect("esp-now init failed");
  let (_manager, sender, receiver) = esp_now.split();

  info!("esp-now up, channel = 1 (default)");

  // 2. 起三个 embassy task
  spawner.must_spawn(nonce_listener_task(receiver));
  spawner.must_spawn(command_sender_task(sender));
  spawner.must_spawn(response_matcher_task());

  // 主 loop 空转（真实业务可以在这里做 UI / 传感器采集）
  loop {
    Timer::after(Duration::from_secs(60)).await;
  }
}

// ============================================================
// Task 1：监听 NonceHello + Response 广播
// ============================================================

#[embassy_executor::task]
async fn nonce_listener_task(mut receiver: esp_radio::esp_now::EspNowReceiver<'static>) {
  loop {
    let data = receiver.receive_async().await;
    let bytes = data.data();

    // Response 是 RESPONSE_LEN（24）字节；其它长度（Frame、干扰帧）静默忽略。
    // 注意：以 `RESPONSE_LEN` 常量为准，不要硬编码具体数字。
    if bytes.len() != RESPONSE_LEN {
      // 顺便可以按 FRAME_LEN 分派手柄状态帧（若也想订阅）
      let _ = FRAME_LEN;
      continue;
    }

    match decode_response(bytes) {
      Ok(resp) => match resp.body {
        // 收到 NonceHello → 装进全局，之后 command_sender_task 才能签命令
        ResponseBody::NonceHello { nonce } => {
          init_session_nonce(nonce);
          if !NONCE_READY.swap(true, Ordering::Relaxed) {
            info!("nonce ready: 0x{:08x}", nonce);
          }
        }
        // 观察空气里的 AnnounceReply（可选，用于诊断）
        //
        // 说明：本 host 是"发命令角色"而非手柄本体，通常不需要维护
        // receiver 目录。若确实想 host 侧也管理 receiver 列表，可在此
        // 则复用手柄的 PeerRegistry 逻辑（`crates/comm/src/peer_registry.rs`）。
        ResponseBody::AnnounceReply {
          mac,
          rssi_dbm,
          role_tag,
        } => {
          info!(
            "observed AnnounceReply mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} \
             rssi={}dBm role={:?}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], rssi_dbm, role_tag
          );
        }
        // 其它响应类型转给 matcher 处理
        _ => RESPONSE_SIGNAL.signal(resp),
      },
      Err(ResponseDecodeError::BadMagic | ResponseDecodeError::BadLength) => {
        // 干扰帧或状态帧，静默忽略
      }
      Err(err) => {
        warn!("bad response: {:?}", err);
      }
    }
  }
}

// ============================================================
// Task 2：每 3 秒发一条示例命令
// ============================================================

#[embassy_executor::task]
async fn command_sender_task(mut sender: esp_radio::esp_now::EspNowSender<'static>) {
  // 广播地址
  const BROADCAST: [u8; 6] = [0xFF; 6];

  // 等到收到至少一次 NonceHello 才开始发
  while !NONCE_READY.load(Ordering::Relaxed) {
    Timer::after(Duration::from_millis(200)).await;
  }
  info!("nonce ready → start sending commands");

  loop {
    // 示例：让手柄 LED 0 闪 3 次，每次 100ms
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let cmd = Command::with_key(
      seq,
      KeyId::DEFAULT,
      CommandBody::LedBlink {
        led_idx: 0,
        count: 3,
        period_ms: 100,
      },
    );

    // wire 长度 = COMMAND_LEN = 24
    let bytes: [u8; COMMAND_LEN] = encode_command(&cmd);
    match sender.send_async(&BROADCAST, &bytes).await {
      Ok(_) => info!("sent LedBlink seq={} ({} B)", seq, COMMAND_LEN),
      Err(e) => error!("send failed: {:?}", e),
    }

    Timer::after(Duration::from_secs(3)).await;
  }
}

// ============================================================
// Task 3：匹配 Ack / Error 响应
// ============================================================

#[embassy_executor::task]
async fn response_matcher_task() {
  loop {
    let resp = RESPONSE_SIGNAL.wait().await;
    match resp.body {
      ResponseBody::Ack => {
        info!("✓ ACK for seq={}", resp.req_seq);
      }
      ResponseBody::Error(code) => {
        warn!("✗ ERR seq={} code={:?}", resp.req_seq, code);
      }
      ResponseBody::BatterySnapshot { percent } => {
        info!("battery: {}%", percent);
      }
      // 下面两种由 listener_task 直接处理，此处不会走到；显式匹配
      // 避免非穷举 warning。
      ResponseBody::NonceHello { .. } | ResponseBody::AnnounceReply { .. } => {}
    }
  }
}
```

## 关键实现细节

### 1. NonceHello 握手时机

手柄每 5 秒广播一次 `NonceHello`（[esp_now/mod.rs](../crates/controller/src/transport/esp_now/mod.rs)
的 `nonce_broadcast_task`）。控制端上电后**最长等待约 5 秒**才能
发出第一条合法命令 —— 本例用 `NONCE_READY` AtomicBool 兜底：

```rust
while !NONCE_READY.load(Ordering::Relaxed) {
  Timer::after(Duration::from_millis(200)).await;
}
```

### 2. seq 严格递增（抗重放）

手柄侧 [`AntiReplayWindow`](../crates/protocol/src/replay.rs) 对每个
`key_id` 独立维护 64 位滑动窗口。控制端 seq 必须**严格单调递增**：

- ✅ `1, 2, 3, 4, ...` 全部接受
- ✅ `1, 2, 5, 3, 4`（乱序但落在窗口内）全部接受
- ❌ 重启后从 1 重新发 → **老 seq 被拒绝**（AuthFailed 或 Replay）

**生产环境**建议把 `NEXT_SEQ` 值定期持久化到 NVS。参考
[hal/persist.rs](../crates/controller/src/hal/persist.rs) 的 `PersistentConfig` 里已经
包含 `replay_windows` 字段 —— 控制端可以模仿此实现。

### 3. Response 按 req_seq 匹配

`CommandResponse.req_seq` 携带对应请求 Command 的 seq。控制端如果
需要"请求-响应对齐"（例如超时重试），可以维护一个 `pending_seqs:
HashMap<u32, Deadline>`，在 matcher 里匹配后移除。

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

如果 host（本文档角色）也想充当"receiver 发现方"（不是典型场景），可以：

1. 定期广播 `CommandBody::Announce`（等同手柄进入 Selecting 时的行为）
2. 监听 `ResponseBody::AnnounceReply` 并维护本地目录
3. 对新发现的 receiver 单播 `CommandBody::AssignId { mac, receiver_id }`

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
5. 手柄侧返回 Ack → 控制端打印 `✓ ACK for seq=N`
6. 🆕 若同一空气中有 receiver 在线，长按手柄 Switch 键会触发 Announce
   广播 → controller-host 日志中会打印 `observed AnnounceReply mac=... rssi=...`
   （诊断用）

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
- 🆕 手柄侧 Peer 目录管理 → [crates/comm/src/peer_registry.rs](../crates/comm/src/peer_registry.rs)（全局单例在 [crates/controller/src/lib.rs](../crates/controller/src/lib.rs)）
- 🆕 手柄侧 Announce 广播 → [crates/controller/src/transport/esp_now/mod.rs](../crates/controller/src/transport/esp_now/mod.rs)（`broadcast_announce` / `esp_now_receive_task`）
- Dashboard 参考实现（BLE 版）→ [crates/dashboard/src/bluetooth.rs](../crates/dashboard/src/bluetooth.rs)
- **纯 host 侧协议交互 demo** → [crates/examples/controller-host-demo/](../crates/examples/controller-host-demo/)
