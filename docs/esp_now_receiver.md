# ESP-NOW 接收端参考实现

本文档提供一份"另一颗 ESP32 如何接收本手柄广播帧"的可复制样板。烧到接收端 ESP32
上即可实时打印手柄状态。

## 📑 目录

- [ESP-NOW 接收端参考实现](#esp-now-接收端参考实现)
  - [📑 目录](#-目录)
  - [特性](#特性)
  - [接收端硬件要求](#接收端硬件要求)
  - [接收端 Cargo.toml 参考](#接收端-cargotoml-参考)
  - [接收端完整代码](#接收端完整代码)
  - [关键点说明](#关键点说明)
    - [1. 无需 `add_peer` 就能接收](#1-无需-add_peer-就能接收)
    - [2. 频道对齐](#2-频道对齐)
    - [3. 空气中的干扰帧](#3-空气中的干扰帧)
    - [4. `dest_mask` 位图寻址](#4-dest_mask-位图寻址)
    - [5. 延迟表现](#5-延迟表现)
    - [6. 1 对 N 广播 \& 多接收方选择](#6-1-对-n-广播--多接收方选择)
    - [7. 与 BLE 双轨并存](#7-与-ble-双轨并存)
  - [进阶：响应 Announce \& 接受 AssignId（推荐）](#进阶响应-announce--接受-assignid推荐)
  - [烧录 \& 验证](#烧录--验证)
  - [还没有硬件？先在本机跑一遍协议](#还没有硬件先在本机跑一遍协议)
  - [下一步：想让接收端回发命令？](#下一步想让接收端回发命令)

## 特性

- **零配置**：接收端无需知道发送端 MAC，无需配对，上电即收
- **1 对 N**：多个接收端同时开机，都能收到同一份手柄状态
- **多接收方选择**：手柄可通过 `dest_mask` 位图寻址到某一台/某一组接收方
- **兼容任意 ESP32**：ESP32 / C3 / S3 / C6 / H2 / S2 均可
- **低延迟**：端到端 <10 ms（典型 <5 ms）
- **状态帧无鉴权**：单向广播 25 B 明文帧（Frame 含 4 B `dest_mask`），接收端零密钥零配置即可解码；
  Command / Response 双向控制帧才带 HMAC-SHA256（见文末"进阶：接收端
  回发命令"）

## 接收端硬件要求

- 任意支持 ESP-NOW 的 ESP32 系列芯片
- 串口/JTAG 用于观察 `defmt` 日志（可选）

## 接收端 Cargo.toml 参考

新建 `esp32-controller-receiver` 项目：

```toml
[package]
name = "esp32-controller-receiver"
edition = "2024"
version = "0.1.0"

[dependencies]
# 只依赖协议 crate（纯 no_std，无 esp-hal / embassy 等重依赖）
# 通过 path 复用本仓库的协议层，保证两端 Frame 布局 100% 一致
# 若想脱离本仓库使用：把 crates/protocol 复制过去或发布到 crates.io 即可
controller-protocol = { path = "../controller/crates/protocol", default-features = false, features = ["defmt"] }

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
defmt = "1.0.1"
panic-rtt-target = { version = "0.2.0", features = ["defmt"] }
rtt-target = { version = "0.6.2", features = ["defmt"] }
static_cell = "2.1.1"
```

> 如果接收端换成 C3/S3 等其它芯片，把 `esp32` feature 换成对应芯片名即可
> （`esp32c3` / `esp32s3` / ...），同时改 `.cargo/config.toml` 的 target。

## 接收端完整代码

`src/bin/main.rs`：

```rust
#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::clock::CpuClock;
use esp_hal::timer::timg::TimerGroup;
use panic_rtt_target as _;
use static_cell::StaticCell;

// 复用发送端的协议模块：Frame、ButtonBits、decode_frame 等
// 现在协议已经独立为 controller-protocol crate（纯 no_std，跨端复用）
use controller_protocol::{decode_frame, ButtonBits, DecodeError};

esp_bootloader_esp_idf::esp_app_desc!();

/// 本机 receiver 的逻辑 ID（0..=31），由手柄通过 AssignId 命令下发；
/// 首次上电前可以用一个占位值（例如 0），收到 AssignId 后再持久化到 NVS。
const MY_RECEIVER_ID: u8 = 0;

#[esp_rtos::main]
async fn main(_spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);
    esp_alloc::heap_allocator!(size: 64 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = esp_hal::interrupt::software::SoftwareInterruptControl::new(
        peripherals.SW_INTERRUPT,
    );
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    info!("[RX] Booting ESP-NOW receiver (id={})", MY_RECEIVER_ID);

    // 初始化 Wi-Fi（ESP-NOW 借用 Wi-Fi 硬件）
    let (wifi_controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default())
            .expect("wifi init failed");

    // 长期保留 wifi_controller，避免 drop 关闭协处理器
    static WIFI_CTRL: StaticCell<esp_radio::wifi::WifiController<'static>> =
        StaticCell::new();
    let _ = WIFI_CTRL.init(wifi_controller);

    // 只要 receiver；manager/sender 也保活防止 drop
    let (mgr, sender, mut receiver) = interfaces.esp_now.split();
    static ESP_NOW_MGR: StaticCell<esp_radio::esp_now::EspNowManager<'static>> =
        StaticCell::new();
    let _ = ESP_NOW_MGR.init(mgr);
    static ESP_NOW_SENDER: StaticCell<esp_radio::esp_now::EspNowSender<'static>> =
        StaticCell::new();
    let _ = ESP_NOW_SENDER.init(sender);

    // ---- 收数循环 ----
    let mut last_seq: Option<u32> = None;
    loop {
        let pkt = receiver.receive_async().await;
        let data = pkt.data();

        match decode_frame(data) {
            Ok(frame) => {
                // 🎯 dest_mask 过滤：只处理寻址到自己的帧
                //
                // Frame 携带 4 B dest_mask 位图；bit-i = 1 表示 receiver_id==i
                // 的接收方应处理该帧；`0xFFFF_FFFF` = 广播（默认）。
                if !frame.is_addressed_to(MY_RECEIVER_ID) {
                    // 不是发给我的：静默丢弃（不用日志，避免刷屏）
                    continue;
                }

                // 丢包检测：seq 应连续递增
                if let Some(prev) = last_seq {
                    let expected = prev.wrapping_add(1);
                    if frame.header.seq != expected {
                        warn!(
                            "[RX] seq gap: expected {}, got {} (dropped {})",
                            expected,
                            frame.header.seq,
                            frame.header.seq.wrapping_sub(expected)
                        );
                    }
                }
                last_seq = Some(frame.header.seq);

                let s = &frame.payload;
                info!(
                    "[RX] seq={} dest=0x{:08x} joy=({},{}) knob=({},{}) btn=0b{=u8:08b} sw={}",
                    frame.header.seq,
                    frame.dest_mask,
                    s.joy_x, s.joy_y,
                    s.knob_1, s.knob_2,
                    s.buttons,
                    s.is_pressed(ButtonBits::Switch),
                );
            }
            Err(DecodeError::BadMagic) => {
                // 空气里其它 ESP-NOW 设备的广播帧（家里/办公室常有）—— 静默忽略
            }
            Err(e) => warn!("[RX] decode error: {:?}", e),
        }

        // 让出调度器，避免 busy loop
        Timer::after(Duration::from_millis(1)).await;
    }
}
```

## 关键点说明

### 1. 无需 `add_peer` 就能接收

ESP-NOW 接收端不需要事先知道发送端 MAC；只要 Wi-Fi 硬件在同一频道
（默认都是 channel 1）即可收到广播帧。

发送端已经在初始化时把 `FF:FF:FF:FF:FF:FF` 加为了 peer，接收端**不需要**。

### 2. 频道对齐

如果发送端调整了 Wi-Fi 频道（例如以 STA 模式连了路由器），需要在接收端也调用
`esp_now.set_channel(channel)` 对齐，否则不同频道时收不到。

本样例保持默认，两端都在 channel 1，**上电即通**。

### 3. 空气中的干扰帧

ESP-NOW 广播地址是公开的，家里/办公室的其它 ESP-NOW 设备的广播帧也会被接收到。
此外，本手柄自身在同一广播频道上会发送 **3 种不同的帧**（Frame / Command / Response，
完整帧结构与鉴权对照见 [`protocol_air.md`](./protocol_air.md)）：

| 帧类型 | 长度 | Magic | 方向 |
|---|---|---|---|
| Frame | 25 B | `0xC71E` | 手柄 → 接收方（广播 + `dest_mask` 位图寻址） |
| Command  | 24 B | `0xCB01` | 手柄 → 接收方（HMAC 认证） |
| Response | 24 B | `0xCB02` | 接收方 → 手柄（HMAC 认证） |

`decode_frame` **只认 `FRAME_MAGIC = 0xC71E`**，其它 magic 以及外部干扰帧全部返回
[`DecodeError::BadMagic`]，接收端**无需业务层额外处理**。CRC-16 校验进一步保证了数据
完整性 —— 已足够应对 1 对 N 广播接收场景。

> 如果你希望**只监听命令与响应**（做抓包 / 调试探针），改判 magic 为 `0xCB01` /
> `0xCB02` 并复用 `controller_protocol::command` / `controller_protocol::response`
> 里的解码函数即可 —— 但需要 HMAC 密钥，属于控制端场景，见
> [`esp_now_controller.md`](./esp_now_controller.md)。

### 4. `dest_mask` 位图寻址

Frame 携带一个 `u32 dest_mask` 字段，语义如下：

- `bit-i == 1` ⇒ `receiver_id == i` 的接收方应处理该帧
- `0xFFFF_FFFF` = 广播（默认）
- `0` = 静默丢弃（手柄"暂停下发"时使用）

接收端只需一行代码即可完成过滤：

```rust
if !frame.is_addressed_to(MY_RECEIVER_ID) {
    continue;
}
```

`is_addressed_to` 是 `const fn`，编译器可以在启用 LTO 时把它内联到主循环里，**零 CPU
开销**。相较"发送方广播 → 接收方全解 payload"，这套方案：

- 广播空口占用不变（发送方只发一次）
- 接收方在 CRC 通过后、业务处理前一步 branch 就能过滤，节省下游 GPIO / SPI 开销
- 手柄的选择器 UI（长按 Switch 800 ms）产生的 `dest_mask` 自动作用到所有接收方，
  无需接收方感知选择过程

### 5. 延迟表现

| 环节 | 典型延迟 |
|---|---|
| 发送端采样 → encode_frame | <1 ms |
| ESP-NOW 空口传输 | 2..5 ms |
| 接收端 decode_frame + `dest_mask` 过滤 → 业务处理 | <1 ms |
| **端到端** | **<5 ms（最差 <10 ms）** |

> 上表中各环节相加，端到端通常在 **5 ms 以内**，最差情况（含调度让出、信道竞争）
> 不超过 **10 ms**。对比 BLE HID 的 15..30 ms 略优，真正的优势是**无需配对**、
> **无中央 host**、**天然 1 对 N**。

### 6. 1 对 N 广播 & 多接收方选择

因为发送端发的是**广播地址**，所以：

- 多个接收端同时开机，都能收到同一份手柄状态
- 适合"教室里 30 台机器人一起响应""赛车队方阵演示"这类场景
- 通过 `dest_mask` 位图，手柄可以在运行时选择"发给谁"：
  - **全场景广播**：`dest_mask = 0xFFFF_FFFF`（默认）
  - **单播 receiver_id=3**：`dest_mask = 1 << 3`
  - **多选**：`dest_mask = (1 << 1) | (1 << 5) | (1 << 9)`

> 如需**物理层单播**（省电、更安全）：把 `src/transport/esp_now/mod.rs` 里的
> `BROADCAST_ADDRESS` 改成目标 MAC，并在发送端 `add_peer(target_mac)`；
> 接收端无需改动。物理层单播 + `dest_mask` 逻辑寻址可以互补使用。

### 7. 与 BLE 双轨并存

发送端同时启用了 BLE HID 和 ESP-NOW，一次 `send()` 分别推给两个通道：

- **BLE 通道**：给手机/PC 做通用手柄（Xbox/PlayStation Tester 一样识别）
- **ESP-NOW 通道**：给自定义 ESP32 接收端做高速无线控制

一路断了另一路继续工作，容错性很好。

## 进阶：响应 Announce & 接受 AssignId（推荐）

要让手柄的选择器 UI 能显示到本机 receiver（角色 / MAC / RSSI），需要接收端
额外做两件事：

1. **响应 `CommandKind::Announce`**：手柄进入选择模式时会广播一条 Announce
   命令，接收端应回一条 `ResponseBody::AnnounceReply { mac, rssi_dbm, role_tag }`
2. **接受 `CommandKind::AssignId`**：手柄决定给你分配一个 `receiver_id` 时会
   广播一条 AssignId 命令；接收端对比 payload 里的 mac 与自身一致后，把
   `receiver_id` 持久化到 NVS，下次上电直接使用

关键代码骨架（完整实现见 [`esp_now_controller.md`](./esp_now_controller.md)
中 controller 侧的对偶部分）：

```rust
use controller_protocol::{
    Command, CommandBody, CommandDecodeError, CommandResponse, KeyId,
    ResponseBody, decode_command, encode_response,
};

// 接收端 3 种入口都在同一个 receive_async 循环里，用 magic 分派：
match u16::from_le_bytes([data[0], data[1]]) {
    0xC71E => handle_frame(data),                     // Frame（含 dest_mask 过滤）
    0xCB01 => handle_command_from_controller(data),   // Announce / AssignId / ...
    _ => {}                                            // 其它 magic 忽略
}

fn handle_command_from_controller(bytes: &[u8]) {
    let cmd = match decode_command(bytes) {
        Ok(c) => c,
        Err(CommandDecodeError::BadMagic | CommandDecodeError::BadLength) => return,
        Err(e) => {
            defmt::warn!("[RX] command decode err: {:?}", e);
            return;
        }
    };

    match cmd.kind {
        CommandBody::Announce => {
            // 广播 AnnounceReply，让手柄的 peer_registry 把我记下来
            let my_mac = read_own_mac();       // 通常来自 ESP-HAL efuse
            let reply = CommandResponse {
                req_seq: cmd.seq,
                key_id: cmd.key_id,
                body: ResponseBody::AnnounceReply {
                    mac: my_mac,
                    rssi_dbm: -50,             // 可从 pkt.info.rx_control 取
                    role_tag: *b"led",         // 3 字节 ASCII 角色
                },
            };
            broadcast(&encode_response(&reply));
        }
        CommandBody::AssignId { mac, receiver_id } => {
            // 仅当 mac 与自身一致时才吃下（AssignId 走广播）
            if mac == read_own_mac() {
                persist_receiver_id(receiver_id);
                defmt::info!("[RX] assigned receiver_id={}", receiver_id);
            }
        }
        _ => { /* 其它 kind：按需处理 */ }
    }
}
```

> **签名要求**：AnnounceReply / AssignId 都跟其它 Command / Response 一样走
> HMAC-SHA256 校验，接收端需要预置 `SECRET_V1` / `SECRET_V2`（`SHARED_SECRETS`）。
> 具体密钥注入方式见 [`crates/protocol/USAGE.md`](../crates/protocol/USAGE.md)。

## 烧录 & 验证

假设你已经烧好接收端固件：

1. 上电接收端 ESP32，打开 RTT 观察 defmt 日志
2. 上电发送端手柄
3. 拨动摇杆/按下按钮 → 接收端每 33 ms 打印一行 `seq=... dest=0xFFFFFFFF joy=(x,y) ...`
4. 短暂遮挡天线，观察 `seq gap` 警告 —— 验证丢包检测
5. **多接收方测试**：在手柄上长按 Switch 800 ms → 选择器面板出现你的
   receiver → 只选你 → 退出选择器 → 观察其它接收端日志停止 / 你自己的日志继续

## 还没有硬件？先在本机跑一遍协议

本仓库自带一个纯 host 侧的 demo，无需真机即可看 Frame 编解码 + seq gap
检测 + 错误处理的完整闭环：

```bash
cargo make example-receiver-demo
```

源码：[`crates/examples/controller-receiver-demo/`](../crates/examples/controller-receiver-demo/)
—— 它是本文档示例代码的**可编译镜像**，CI 每次都会跑，防止协议 API 变更
时文档腐化。

---

## 下一步：想让接收端回发命令？

本文档覆盖的是"只读订阅"场景。如果你希望接收端能**主动给手柄下发命令**
（例如设置灵敏度、点亮 LED、切换电池模拟模式），请参考：

👉 **[esp_now_controller.md](./esp_now_controller.md)** —— 完整的控制端参考实现，
包含 HMAC-SHA256 签名、NonceHello 握手、抗重放窗口、密钥轮换等细节。
