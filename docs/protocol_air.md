<p align="center">
  <b>controller · 空中协议参考（Air Protocol Reference）</b><br />
  <sub>手柄在 ESP-NOW 广播信道上出现的全部帧类型、字节布局与安全模型</sub>
</p>

> [!NOTE]
> 本文档是 [`esp_now_receiver.md`](./esp_now_receiver.md) 与
> [`esp_now_controller.md`](./esp_now_controller.md) 的**公共协议底图**。
> 想快速上手请先看那两份用户指南；本文档仅在你需要**深入理解协议**或
> **实现自定义主机**时查阅。

📍 文档导航：[`README.md`](../README.md) · [协议 crate README](../crates/protocol/README.md) · 接收端 · 控制端

## 📑 目录

- [📑 目录](#-目录)
- [📡 空气中的 3 种帧](#-空气中的-3-种帧)
- [1. 🎮 Frame（状态帧，25 B）](#1--frame状态帧25-b)
- [2. 🕹️ Command（控制命令，24 B）](#2-️-command控制命令24-b)
- [3. 📨 Response（命令响应 + Nonce 广播 + AnnounceReply，24 B）](#3--response命令响应--nonce-广播--announcereply24-b)
- [4. 🎯 多接收方寻址（`dest_mask` + Announce/AssignId 三步握手）](#4--多接收方寻址dest_mask--announceassignid-三步握手)
- [🛡️ 安全模型](#️-安全模型)
- [手柄的空口时间表](#手柄的空口时间表)
- [版本与部署要点](#版本与部署要点)
  - [协议版本](#协议版本)
  - [密钥注入（生产必读）](#密钥注入生产必读)
  - [编译 Feature 速查](#编译-feature-速查)
- [🔗 参考实现](#-参考实现)
- [🗂️ 源码位置速查](#️-源码位置速查)

## 📡 空气中的 3 种帧

| Magic | 帧类型 | 方向 | 长度 | 鉴权 | 序列号 | 抗重放 |
| ----- | ------ | ---- | ---- | ---- | ------ | ------ |
| `0xC71E` | Frame | 手柄 → 主机 | **25 B** | CRC-16 | 有 | 无 |
| `0xCB01` | Command | 主机 → 手柄 / 手柄 → receiver | **24 B** | HMAC-SHA256 | 有 | ✅ 64 位滑动窗 |
| `0xCB02` | Response | 手柄 → 主机 / receiver → 手柄 | **24 B** | HMAC-SHA256 | 有 | 无 |

> [!IMPORTANT]
> Frame 携带 `dest_mask: u32` 位图寻址（4 B），wire size 为 25 B；Command/Response 携带 Announce/AssignId/AnnounceReply 通道，payload 为 10 B，wire size 为 24 B。

## 1. 🎮 Frame（状态帧，25 B）

**用途**：手柄每 33 ms（约 30 Hz）广播一次实时状态（按键 / 摇杆 / 旋钮）+ 目标寻址位图。

```text
 offset | size | field
 -------+------+-----------
   0    |  2   | magic = 0xC71E (LE)
   2    |  1   | version = 2
   3    |  4   | seq (LE, u32)
   7    | 12   | payload (GamepadState)
  19    |  4   | dest_mask (LE, u32)
  23    |  2   | crc16_ibm(bytes[0..23]) (LE)
```

**`dest_mask` 语义** —— 32 位目标寻址位图：

| 值 | 含义 | 用途 |
| --- | --- | --- |
| `0xFFFF_FFFF` | 广播（`BROADCAST_DEST_MASK`） | 默认；所有 receiver 处理该帧 |
| `1 << N` | 仅 `receiver_id == N` 处理 | 单播/多播；bit-i 对应 `receiver_id == i` 的 receiver |
| `0` | 静默丢弃 | 暂停下发（保持空口时序但没有 receiver 处理） |

接收端一行代码判定：`frame.is_addressed_to(my_receiver_id)`，未命中即静默丢帧。

**GamepadState (12 B)** —— 见 [`crates/protocol/src/state.rs`](../crates/protocol/src/state.rs)：

| 字段        | 类型 | 说明                                       |
| ----------- | ---- | ------------------------------------------ |
| `buttons`   | `u16` | 按键位图（`ButtonBits`）                 |
| `joy_x`     | `i16` | 摇杆 X 轴 `-32768..=32767`               |
| `joy_y`     | `i16` | 摇杆 Y 轴                                |
| `knob_1`    | `u16` | 旋钮 1（0..=65535）                      |
| `knob_2`    | `u16` | 旋钮 2                                    |
| `_reserved` | `u16` | 预留（未来扩展）                          |

**`buttons` 位图（`ButtonBits`）** —— 位分配保留前向兼容，新增按钮用下一个未用比特：

| 比特 | 掩码 (hex) | 名称 | 硬件 |
| ---- | ---------- | ---- | ---- |
| 0 | `0x0001` | `Btn1` | IO27 |
| 1 | `0x0002` | `Btn2` | IO13 |
| 2 | `0x0004` | `Btn3` | IO25 |
| 3 | `0x0008` | `Btn4` | IO23 |
| 4 | `0x0010` | `JoyBtn` | IO12（摇杆按下） |
| 5 | `0x0020` | `Switch` | IO15（拨动开关，"开"为 1） |
| 6–15 | — | 预留 | — |

> 判断某键是否按下：`state.buttons & ButtonBits::X.mask() != 0`；或调用
> `state.is_pressed(ButtonBits::X)`。序列化见 `GamepadState::to_bytes`（little-endian）。

> [!TIP]
> **设计取舍**
> - **明文**：主机侧接收无需密钥；如需防窥探应改用 BLE 加密链路
> - **仅 CRC**：状态帧允许丢包（下一帧 33 ms 就到），无需完整性保证
> - **单向**：手柄绝不"应答"任何状态帧，因此不占空口

## 2. 🕹️ Command（控制命令，24 B）

**用途**：
- 主机（Dashboard / 自定义控制端）主动下发命令给手柄
- 手柄主动下发 Announce/AssignId 到 receiver

```text
 offset | size | field
 -------+------+---------
   0    |  2   | magic = 0xCB01 (LE)
   2    |  1   | version_byte
        |      |   ↳ 低 4 位 = protocol_version (=5)
        |      |   ↳ 高 4 位 = key_id (0..=15)
   3    |  1   | kind
   4    |  4   | seq (LE, u32)
   8    | 10   | payload
  18    |  4   | hmac tag (SHA256 截断 4B)
  22    |  2   | crc16_ibm(bytes[0..22])
```

**7 种 CommandKind** —— 见 [`crates/protocol/src/command.rs`](../crates/protocol/src/command.rs)：

| kind | 名称              | payload                                     | 语义                          |
| ---- | ----------------- | ------------------------------------------- | ----------------------------- |
| 0x00 | `Nop`             | ―                                           | 心跳 / 连接性检查             |
| 0x01 | `LedBlink`        | `led_idx: u8, count: u8, period_ms: u16`    | LED 闪烁 N 次                 |
| 0x02 | `SetSensitivity`  | `joy_scale: u16, knob_scale: u16` (0..=1000) | 摇杆 / 旋钮定点缩放           |
| 0x03 | `ShowToast`       | `len: u8, bytes: [u8; 5]`（ASCII）          | OLED 底部弹提示（≤5 字节）    |
| 0x04 | `SetBatteryMode`  | `simulate: bool`                            | 电池模拟 / 真实模式切换       |
| 0x05 | `Announce` ⭐     | `payload` 全 0                              | 广播发现请求；所有在线 receiver 应回 `AnnounceReply` |
| 0x06 | `AssignId` ⭐     | `mac: [u8; 6], receiver_id: u8`             | 分配逻辑 ID；receiver 校验 `mac == 自身` 才吃下 |

**校验顺序**（`decode_command`）：`长度 → magic → version → key_id → CRC → HMAC → payload`。
任一失败立即返回对应 [`CommandDecodeError`](../crates/protocol/src/command.rs)。

## 3. 📨 Response（命令响应 + Nonce 广播 + AnnounceReply，24 B）

**用途**：
- 手柄回执命令执行结果（Ack / Error / BatterySnapshot）
- 手柄**主动**广播 K3 Session Nonce（`NonceHello`）供控制端反重放
- receiver 响应手柄的 `Announce`（`AnnounceReply`）

```text
 offset | size | field
 -------+------+---------
   0    |  2   | magic = 0xCB02 (LE)
   2    |  1   | version_byte（同 Command，protocol_version=5）
   3    |  4   | req_seq（对应 Command.seq；NonceHello/AnnounceReply 时为 0）
   7    |  1   | kind
   8    | 10   | payload
  18    |  4   | hmac tag
  22    |  2   | crc16_ibm(bytes[0..22])
```

**5 种 ResponseKind** —— 见 [`crates/protocol/src/response.rs`](../crates/protocol/src/response.rs)：

| kind | 名称                | payload                                    | 语义                          |
| ---- | ------------------- | ------------------------------------------ | ----------------------------- |
| 0x00 | `Ack`               | ―                                          | 命令成功执行                  |
| 0x01 | `Error`             | `code: ErrorCode`                          | 命令执行失败                  |
| 0x02 | `BatterySnapshot`   | `percent: u8` (0..=100)                    | 电量快照                      |
| 0x03 | `NonceHello`        | `nonce: u32` (LE)                          | K3 主动广播，`req_seq = 0`    |
| 0x04 | `AnnounceReply` ⭐  | `mac: [u8; 6], rssi_dbm: i8, role_tag: [u8; 3]` | receiver 上报自身身份 |

**ErrorCode**：

| 码值  | 名称              | 触发场景                             |
| ---- | ----------------- | ------------------------------------ |
| 0x01 | `InvalidArgument` | 参数越界（如 `led_idx = 99`）        |
| 0x02 | `Unsupported`     | 手柄不认识该命令                     |
| 0x03 | `Busy`            | 内部忙（如 LED 特效队列已满）        |

## 4. 🎯 多接收方寻址（`dest_mask` + Announce/AssignId 三步握手）

通过 3 步握手让手柄可以在同一空口下**精准选择**要发给哪些 receiver：

```text
 Step 1  手柄 → 广播  Command { kind: Announce }             (24 B)
 Step 2  receiver → 广播  Response { kind: AnnounceReply,     (24 B)
                       mac, rssi_dbm, role_tag }
         └─ 手柄 upsert peer_registry：给未知 mac 分配最小可用 receiver_id (0..32)
 Step 3  手柄 → 广播  Command { kind: AssignId,               (24 B)
                       mac, receiver_id }
         └─ receiver 校验 payload.mac == 自身 → 记住 receiver_id 到 NVS
```

完成后手柄发 Frame 时用 `dest_mask` 选择接收方：

```text
 Frame { dest_mask: 0xFFFF_FFFF }        → 所有 receiver 处理（广播）
 Frame { dest_mask: 1 << 3 }             → 只有 receiver_id=3 处理
 Frame { dest_mask: (1<<1)|(1<<5)|(1<<9) } → receiver_id ∈ {1,5,9} 处理
 Frame { dest_mask: 0 }                  → 无人处理（暂停下发）
```

receiver 侧的过滤逻辑（推荐）：`if !frame.is_addressed_to(my_receiver_id) { continue; }`。
物理层仍是广播——**只在软件层做位图过滤**，好处：
- 单一广播任务同时服务所有 receiver，无需 ESP-NOW peer 表管理
- receiver 数量增减无需协商 —— 上电即 Announce，掉电即静默
- receiver_id 复用（离线 slot 会被下一个 Announce 的新 receiver 回收）

> [!TIP]
> **peer_registry 容量**：32 slot，对应 `dest_mask` 32 bit。多于 32 的 receiver 需要
> 升 `dest_mask` 到 `u64` —— 见 [`crates/protocol/src/frame.rs`](../crates/protocol/src/frame.rs)
> 的 `BROADCAST_DEST_MASK` 与 `is_addressed_to` 上限。

## 🛡️ 安全模型

| 层次           | 措施                                    | 抵御的威胁              |
| -------------- | --------------------------------------- | ----------------------- |
| **完整性**     | CRC-16-IBM 覆盖全帧                     | 无线丢包 / 位翻转       |
| **认证**       | HMAC-SHA256 截断 4 字节                 | 伪造命令 / 中间人       |
| **抗重放**     | 64 位滑动窗口 + Session Nonce           | 录制回放攻击            |
| **密钥轮换**   | 4 slot `key_id` 并存                    | 密钥泄露后平滑替换      |

**密钥槽位**（[`crates/protocol/src/config.rs`](../crates/protocol/src/config.rs)）：

```rust
pub const SHARED_SECRETS: [Option<&'static [u8; 32]>; 4] =
  [Some(SECRET_V1), Some(SECRET_V2), None, None];
```

平滑轮换流程：Day 0 用 slot 0 → Day 15 主机切 slot 1（手柄双 slot 并存）
→ Day 30 slot 0 下线（`SHARED_SECRETS[0] = None`）。

## 手柄的空口时间表

```text
 t (s)  event
 -----  ----------------------------
  0     手柄上电，SESSION_NONCE 随机初始化
  0.1   Frame 首帧广播（seq=1, dest_mask=0xFFFF_FFFF）
  0.13  Frame（seq=2）
  ...   每 33 ms 一帧 ≈ 30 Hz
  5     NonceHello 广播（第一次）
  10    NonceHello 广播（第二次，每 5 s 一次）

【用户长按 Switch 进入 Selecting 模式的额外事件】
  T     Command { kind: Announce, seq++ } 广播（手柄发起 peer 发现）
 T+ε    receiver 陆续回  Response { kind: AnnounceReply }
 T+ε'   手柄单播 Command { kind: AssignId, mac, receiver_id } 到新 receiver
 T'     用户再次长按 Switch 退出 → 后续 Frame 的 dest_mask 更新为选中的 peer 位图
```

**控制端上电后**：
- ≤ 33 ms 内收到第一帧 Frame
- ≤ 5 s 内收到 `NonceHello` → 才能开始发合法 Command

## 版本与部署要点

### 协议版本

| 帧 | `version` 字段 | 常量 | 说明 |
|----|----------------|------|------|
| Frame | `PROTOCOL_VERSION = 2` | `frame::PROTOCOL_VERSION` | 状态帧；携带 `dest_mask` 位图寻址 |
| Command | `COMMAND_VERSION = 5` | `command::COMMAND_VERSION` | 控制面；含 Announce/AssignId |
| CommandResponse | `RESPONSE_VERSION = 5` | `response::RESPONSE_VERSION` | 与 Command 对齐；含 AnnounceReply |

`version_byte` 的低 4 位存 protocol version，高 4 位存 `key_id`（见 Command 布局）。
解码时若版本不匹配会返回对应 `DecodeError` / `CommandDecodeError`。

### 密钥注入（生产必读）

HMAC 共享密钥 **不在源码明文存放**，由 `controller-protocol` 的 `build.rs` 在编译期从
环境变量注入：

| 环境变量 | 要求 | 说明 |
|----------|------|------|
| `CONTROLLER_SECRET_V1` | 32 字节 UTF-8 | 主用密钥（必需） |
| `CONTROLLER_SECRET_V2` | 32 字节 UTF-8 | 备用密钥（必需） |

- 关闭 `embed-default-secrets` feature 后缺失环境变量 → **编译期 panic**，强迫生产注入；
- 默认开启该 feature 时缺失则回退弱密钥，**仅限开发/CI**，严禁生产；
- `debug-auth-bypass` feature 会编译期关闭 HMAC 校验，仅本地构造报文用，生产 build 严禁开启。

### 编译 Feature 速查

| feature | 启用场景 |
|---------|----------|
| `defmt` | 手柄 / ESP32 固件（错误类型可被 defmt 打印） |
| `serde` | Dashboard（WASM，跨语言 JSON 交换） |
| `std` | host 属性测试 |
| `embed-default-secrets` | 开发/CI 冒烟（默认开启） |
| `debug-auth-bypass` | 本地调试构造报文（危险） |

## 🔗 参考实现

- **接收端（只读订阅）**：[`esp_now_receiver.md`](./esp_now_receiver.md)
- **控制端（回发命令）**：[`esp_now_controller.md`](./esp_now_controller.md)
- **手柄侧广播/接收**：[`src/transport/esp_now/mod.rs`](../src/transport/esp_now/mod.rs)
- **Dashboard（BLE 版）**：[`crates/dashboard/src/bluetooth.rs`](../crates/dashboard/src/bluetooth.rs)
- **完整可运行 demo（host 侧模拟）**：
  - 双向控制（Command / Response）：[`crates/examples/controller-host-demo/`](../crates/examples/controller-host-demo/)
  - 状态帧订阅（Frame）：[`crates/examples/controller-receiver-demo/`](../crates/examples/controller-receiver-demo/)

## 🗂️ 源码位置速查

| 主题       | 文件                                                                                          |
| ---------- | --------------------------------------------------------------------------------------------- |
| Frame 编解码   | [`crates/protocol/src/codec.rs`](../crates/protocol/src/codec.rs)                              |
| Command 编解码 | [`crates/protocol/src/command.rs`](../crates/protocol/src/command.rs)                          |
| Response 编解码 | [`crates/protocol/src/response.rs`](../crates/protocol/src/response.rs)                        |
| HMAC / Nonce   | [`crates/protocol/src/auth.rs`](../crates/protocol/src/auth.rs)                                |
| 抗重放窗口     | [`crates/protocol/src/replay.rs`](../crates/protocol/src/replay.rs)                            |
| CRC-16-IBM     | [`crates/protocol/src/crc.rs`](../crates/protocol/src/crc.rs)                                  |
| GamepadState   | [`crates/protocol/src/state.rs`](../crates/protocol/src/state.rs)                              |
| 密钥环 / 常量  | [`crates/protocol/src/config.rs`](../crates/protocol/src/config.rs)                            |
