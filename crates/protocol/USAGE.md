
# `controller-protocol` 使用指南

本文面向 **下游接入方**（另一个 receiver 项目 / 控制端 / 网关等），说明如何以**不拷贝源码**的方式复用 `controller-protocol`，并正确启用 feature、注入密钥、编写最小可运行示例。

---

## 目录

- [1. 选择依赖方式](#1-选择依赖方式)
- [2. Feature 组合矩阵](#2-feature-组合矩阵)
- [3. 密钥注入（必读）](#3-密钥注入必读)
- [4. 最小可运行示例](#4-最小可运行示例)
- [5. `no_std` / `wasm32` / `host` 目标差异](#5-no_std--wasm32--host-目标差异)
- [6. 版本兼容与 MSRV](#6-版本兼容与-msrv)
- [7. 常见踩坑](#7-常见踩坑)

---

## 1. 选择依赖方式

`controller-protocol` 已是**独立 crate**，无需拷贝源码。按项目场景任选一种：

### 1.1 Git 依赖（推荐，私有项目零成本）

```toml
# 下游项目的 Cargo.toml
[dependencies]
controller-protocol = { git = "https://github.com/lf-wxp/controller", tag = "protocol-v0.2.0", default-features = false }
```

- **必须**加 `tag`（或 `rev`），不要用裸 `branch`，否则每次 `cargo update` 都可能出现协议漂移。
- Cargo 会自动扫描 workspace 找到 `crates/protocol` 目录下的 crate，无需 `path`。

### 1.2 crates.io（如已发布）

```toml
[dependencies]
controller-protocol = { version = "0.1", default-features = false }
```

Semver 严格：主版本变化 = 协议破坏性变更，接入方需人工评估。

### 1.3 本地 path（仅本地并行开发）

```toml
[dependencies]
controller-protocol = { path = "../controller/crates/protocol", default-features = false }
```

**不要在 CI / 发布产物中使用 `path`**，别人 clone 会立刻编译失败。

### 1.4 同 workspace（monorepo）

若下游 receiver 与本仓库合并管理，把它加入根 `Cargo.toml` 的 `[workspace] members`，然后：

```toml
[dependencies]
controller-protocol = { path = "../protocol", default-features = false }
```

---

## 2. Feature 组合矩阵

| 接入端场景                | `default-features` | 需要开启的 features           | 说明 |
|--------------------------|--------------------|-------------------------------|------|
| ESP32 `no_std` 接收器    | `false`            | `["defmt"]`                   | 打印走 defmt，最小依赖 |
| PC / Linux 网关（std）    | `false`            | `["std"]`                     | 需要 `Vec`/`String` 等 |
| PC 网关 + JSON 上行      | `false`            | `["std", "serde"]`            | 打开 `Serialize`/`Deserialize` |
| WASM / Dashboard          | `false`            | `["serde"]`                   | wasm32 无 std |
| 单元测试 / proptest       | `false`            | `["std"]`                     | proptest 需要 `std` |
| 本地抓包解析（跳过校验）  | `false`            | `["debug-auth-bypass"]`       | **仅调试**，禁止入生产 |

**⚠️ 关键**：**始终**写 `default-features = false`。默认开启的 `embed-default-secrets` 会带上内置弱密钥，**生产 build 绝不允许**。

---

## 3. 密钥注入（必读）

HMAC 共享密钥通过**编译期环境变量**注入，走 `build.rs` → `env!()`，不落源码：

| 环境变量                  | 类型          | 长度要求          | 用途 |
|--------------------------|---------------|-------------------|------|
| `CONTROLLER_SECRET_V1`   | UTF-8 字符串  | **恰好 32 字节**   | 主用密钥 |
| `CONTROLLER_SECRET_V2`   | UTF-8 字符串  | **恰好 32 字节**   | 备用密钥（用于轮换） |

### 3.1 生产 build

```bash
# 生成两个 32 字节的高熵密钥（base64 编码后恰好 32 chars？不一定！）
# 简易可复现方式：走可打印 ASCII 32 字节
export CONTROLLER_SECRET_V1="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 32)"
export CONTROLLER_SECRET_V2="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 32)"

cargo build --release --no-default-features --features std,serde
```

- 长度**必须**是 32 字节，否则 `build.rs` 中 `assert_eq!` 会 panic 阻止编译。
- 关闭 `embed-default-secrets` 后，若环境变量缺失将**编译期 panic**，避免生产 build 静默使用弱密钥。

### 3.2 CI 注入

- GitHub Actions：`secrets.CONTROLLER_SECRET_V1` / `V2` → `env:` 段落
- GitLab CI：Masked variables
- **禁止**把密钥写进 `.env` / 代码 / commit message。

### 3.3 密钥轮换

上下游同时改 `V2`，把命令 `key_id` 从 `1` 切到 `2`，观察一段时间后再改 `V1`，即可无停机换密钥。

---

## 4. 最小可运行示例

### 4.1 接收 & 解码 Frame（手柄状态广播）

```rust
use controller_protocol::{DecodeError, FRAME_LEN, Frame, GamepadState, decode_frame};

/// 假设 `bytes` 是从 ESP-NOW / BLE / UART 收到的 `FRAME_LEN`（25）字节报文
fn handle_incoming(bytes: &[u8], my_receiver_id: u8) {
    match decode_frame(bytes) {
        Ok(frame) => {
            // Frame 有三个字段：header / payload / dest_mask
            // dest_mask 是位图寻址：bit-i = 1 表示 receiver_id == i 应处理该帧
            if !frame.is_addressed_to(my_receiver_id) {
                // 广播帧（dest_mask == u32::MAX）永远匹配；
                // 目标不含本机时静默丢弃，是接收端过滤的核心
                return;
            }
            log_state(frame.header.seq, &frame.payload);
        }
        Err(DecodeError::BadLength)              => { /* 丢包 / 未对齐 */ }
        Err(DecodeError::BadMagic)               => { /* 不是本协议报文 */ }
        Err(DecodeError::BadCrc { .. })          => { /* 噪声位翻转 */ }
        Err(DecodeError::UnsupportedVersion(_))  => { /* 不兼容的协议版本 */ }
    }
}

fn log_state(seq: u32, state: &GamepadState) {
    let _ = (seq, state);
}
```

> **`Frame` 字段说明**：`Frame` 由 `header` / `payload` / `dest_mask` 三个字段组成，
> `dest_mask: u32` 用于位图寻址（bit-i = 1 表示 `receiver_id == i` 应处理该帧）。
> 访问字段使用 `frame.header` / `frame.payload` / `frame.dest_mask`；广播场景
> 可直接使用 `Frame::new(seq, state)`（默认 `dest_mask = u32::MAX`，即广播）。

### 4.2 发送 & 签名 Command（控制端反向命令）

```rust
use controller_protocol::{
    COMMAND_LEN, Command, CommandBody, KeyId, encode_command, init_session_nonce,
};

/// Command wire size = `COMMAND_LEN`（24 字节；payload 为 10B）
fn send_led_blink(seq: u32) -> [u8; COMMAND_LEN] {
    // 会话初始化只需一次；nonce 参与 HMAC，务必用真随机 seed
    init_session_nonce(0x1234_5678_ABCD_EF01);

    let key_id = KeyId::new(1).expect("key slot 1 must exist");
    let cmd = Command::with_key(
        seq,
        key_id,
        CommandBody::LedBlink { led_idx: 0, count: 3, period_ms: 200 },
    );

    encode_command(&cmd)
}
```

> **重要**：返回类型请**始终**用 `[u8; COMMAND_LEN]` 常量而非 `[u8; 24]`
> 硬编码，避免与协议实际字节长度脱节。同理 `Response`
> 使用 `[u8; RESPONSE_LEN]`。

### 4.3 解码 & 校验 Command（接收端）

```rust
use controller_protocol::{
    AntiReplayWindow, Command, CommandDecodeError, ReplayError, decode_command,
};

/// 返回 Ok 表示命令合法且抗重放通过；否则丢弃。
fn on_command(
    bytes: &[u8],
    window: &mut AntiReplayWindow,
) -> Result<Command, OnCommandError> {
    let cmd = decode_command(bytes).map_err(OnCommandError::Decode)?;
    // 抗重放窗口：每个 key_id 独立一份，别跨 slot 复用
    window.check_and_update(cmd.seq).map_err(OnCommandError::Replay)?;
    Ok(cmd)
}

#[derive(Debug)]
enum OnCommandError {
    Decode(CommandDecodeError),
    Replay(ReplayError),
}
```

> `AntiReplayWindow` **每个 `key_id` 独立一份**（`KEY_SLOTS` 个窗口），不要用同一个窗口跨 slot 校验，也不要在重启后从零开始——建议持久化窗口高水位到 NVS/flash。

### 4.4 单播 / 组播：使用 `dest_mask`（v2）

```rust
use controller_protocol::{Frame, GamepadState};

/// 单播到 `receiver_id == 3` 的接收方
fn build_unicast_frame(seq: u32, state: GamepadState) -> Frame {
    let dest_mask = 1_u32 << 3;
    Frame::with_dest(seq, state, dest_mask)
}

/// 组播到多个接收方（比如 id=1, 5, 9）
fn build_multicast_frame(seq: u32, state: GamepadState) -> Frame {
    let dest_mask = (1_u32 << 1) | (1_u32 << 5) | (1_u32 << 9);
    Frame::with_dest(seq, state, dest_mask)
}

/// 广播（`Frame::new` 的默认行为，等价于 `dest_mask = u32::MAX`）
fn build_broadcast_frame(seq: u32, state: GamepadState) -> Frame {
    Frame::new(seq, state)
}
```

接收端过滤只需一行：

```rust
if !frame.is_addressed_to(my_receiver_id) {
    return; // 目标不含本机，静默丢弃
}
```

> `receiver_id` 取值范围 `[0, 31]`，与 `dest_mask` 的 32 个 bit 一一对应；
> 大于 31 的 `receiver_id` 恒返回 `false`（`is_addressed_to` 已做边界处理）。

### 4.5 Peer 发现：Announce / AnnounceReply / AssignId

**Controller 侧**：广播 `Announce`，等待 receivers 回 `AnnounceReply`：

```rust
use controller_protocol::{
    Command, CommandBody, KeyId, encode_command,
};

/// Controller：广播 Announce，让所有 receivers 上报自己
fn broadcast_announce(seq: u32) -> [u8; 24] {
    let cmd = Command::with_key(seq, KeyId::DEFAULT, CommandBody::Announce);
    encode_command(&cmd)
}

/// Controller：单播 AssignId 给某个 receiver（payload 携带目标 mac + 分配的 id）
fn assign_id(seq: u32, mac: [u8; 6], receiver_id: u8) -> [u8; 24] {
    let cmd = Command::with_key(
        seq,
        KeyId::DEFAULT,
        CommandBody::AssignId { mac, receiver_id },
    );
    encode_command(&cmd)
}
```

**Receiver 侧**：收到 `Announce` 后回 `AnnounceReply`；收到 `AssignId` 后校验 mac 与自身一致再持久化 id：

```rust
use controller_protocol::{
    Command, CommandBody, CommandResponse, KeyId, ResponseBody,
    decode_command, encode_response,
};

fn on_command(bytes: &[u8], my_mac: [u8; 6]) -> Option<[u8; 24]> {
    let cmd = decode_command(bytes).ok()?;
    match cmd.kind {
        CommandBody::Announce => {
            // 上报自己：mac / rssi / role_tag
            let reply = CommandResponse::announce_reply(
                cmd.seq,
                cmd.key_id,
                my_mac,
                -50,       // 占位：真机上从 ESP-NOW rx_control 提取
                *b"led",   // 3 字节 role_tag；不足右侧补 0
            );
            Some(encode_response(&reply))
        }
        CommandBody::AssignId { mac, receiver_id } => {
            if mac != my_mac {
                return None; // 不是给我的，静默忽略
            }
            persist_receiver_id(receiver_id);
            let ack = CommandResponse::ack_with_key(cmd.seq, cmd.key_id);
            Some(encode_response(&ack))
        }
        _ => None,
    }
}

fn persist_receiver_id(id: u8) {
    // 落盘到 NVS / flash / EEPROM，重启后从这里恢复
    let _ = id;
}
```

> **`AnnounceReply.payload` 布局**：`[mac(6B), rssi_dbm(1B), role_tag(3B)]`
> —— MAC 是接收方**自报**的地址（不依赖无线层 src_mac，避免中继/桥接改写）；
> `rssi_dbm` 若未知可填 `i8::MIN`；`role_tag` 是 3 字节 ASCII 展示标签。

---

## 5. `no_std` / `wasm32` / `host` 目标差异

| 目标                          | `alloc` 可用 | `std` 可用 | 建议 features           |
|------------------------------|-------------|-----------|-------------------------|
| `xtensa-esp32-none-elf`      | 是（需 esp-alloc） | 否 | `["defmt"]`             |
| `thumbv7em-none-eabihf`       | 视 HAL       | 否        | `[]` 或 `["defmt"]`     |
| `wasm32-unknown-unknown`      | 是           | 否        | `["serde"]`             |
| `x86_64-unknown-linux-gnu`    | 是           | 是        | `["std", "serde"]`      |
| `aarch64-apple-darwin`        | 是           | 是        | `["std"]`               |

本 crate 的编解码 API **全部** `no_std`，`Vec` / `String` 只出现在 `["std"]` gated 的测试辅助中，接入方零 `alloc` 也能跑。

---

## 6. 版本兼容与 MSRV

- **Rust MSRV**：`1.88`（edition 2024）
- **Semver 约定**：
  - `0.1.x`：Bug 修复，字节布局**不变**
  - `0.2.x`：可能引入新的 `CommandKind` / `ResponseKind`，向后兼容解码器
  - `1.0`：字节布局稳定承诺
- **跨版本互通**：`Frame` / `Command` / `CommandResponse` 均带 magic + version 字段，接入方**必须**校验版本，遇到未知版本报 `UnsupportedVersion` 并丢弃，不要盲目解析。

### 6.1 当前版本

| 消息类型         | wire size | version | Magic    | 关键特性                                 |
|-----------------|-----------|---------|----------|------------------------------------------|
| `Frame`         | **25 B**  | **2**   | `0xC71E` | `dest_mask: u32`（位图寻址）             |
| `Command`       | **24 B**  | **5**   | `0xCB01` | `Announce` / `AssignId` 命令             |
| `CommandResponse` | **24 B**  | **5**   | `0xCB02` | `AnnounceReply` 响应                     |

---

## 7. 常见踩坑

| 症状 | 原因 | 修复 |
|------|------|------|
| `cargo build` 报 `CONTROLLER_SECRET_V1 must be exactly 32 bytes` | 环境变量长度 ≠ 32 | `echo -n "$CONTROLLER_SECRET_V1" \| wc -c` 应为 32 |
| Release build 里出现 `esp32-controller-shared-key-v1!!` 字面量 | 忘了 `--no-default-features`，默认带上了 `embed-default-secrets` | 显式 `default-features = false` |
| `decode_command` 一直返回 `BadMac` | 收发两端 `CONTROLLER_SECRET_V*` 不一致，或 `key_id` slot 对不上 | CI 里 assert 两端密钥同源；用同一份 `build.rs` 环境变量 |
| WASM 打包体积异常大 | 误开了 `["defmt"]` | WASM 端只开 `["serde"]` |
| 反复出现 `Replay` 错误 | 用了单个共享 `AntiReplayWindow` 跨 key_id | 每 slot 一个窗口，重启时从持久化中恢复 |
| `cargo publish` 失败 `missing repository field` | metadata 不全 | 已在 `Cargo.toml` 中补齐 `repository` / `homepage` / `documentation` |
| receiver 收到帧全部丢弃 | 未处理 `dest_mask` 字段 | 用 `frame.payload`（而非旧 `.state`），并调用 `frame.is_addressed_to(my_id)` 做过滤 |
| receiver 无法被 controller 发现 | 未响应 `CommandBody::Announce` | 在 `on_command` 里为 `Announce` 分支回 `CommandResponse::announce_reply(...)` |
| Receiver 重启后 `receiver_id` 丢失 | 收到 `AssignId` 未落盘 | 在 `AssignId` 分支持久化 `receiver_id` 到 NVS/flash，开机时恢复 |

---

## 相关

- 空中协议详细字段布局 → `docs/protocol_air.md`
- 接收端参考实现 → `crates/examples/controller-receiver-demo/src/main.rs`
- 控制端参考实现 → `crates/examples/controller-host-demo/src/main.rs`
- 密钥注入实现 → `crates/protocol/build.rs`
