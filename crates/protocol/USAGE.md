
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
controller-protocol = { git = "https://github.com/lf-wxp/controller", tag = "protocol-v0.1.0", default-features = false }
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
use controller_protocol::{DecodeError, Frame, GamepadState, decode_frame};

/// 假设 `bytes` 是从 ESP-NOW / BLE / UART 收到的 21 字节报文
fn handle_incoming(bytes: &[u8]) {
    match decode_frame(bytes) {
        Ok(Frame { header, state }) => {
            // header.seq 递增；state 是 12B GamepadState
            log_state(header.seq, &state);
        }
        Err(DecodeError::BadLength) => { /* 丢包 / 未对齐 */ }
        Err(DecodeError::BadMagic)  => { /* 不是本协议报文 */ }
        Err(DecodeError::BadCrc)    => { /* 噪声位翻转 */ }
        Err(err)                    => { /* 版本 / 其它 */ let _ = err; }
    }
}

fn log_state(seq: u32, state: &GamepadState) {
    let _ = (seq, state);
}
```

### 4.2 发送 & 签名 Command（控制端反向命令）

```rust
use controller_protocol::{
    Command, CommandBody, KeyId, encode_command, init_session_nonce,
};

fn send_led_blink(seq: u32) -> [u8; 20] {
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
- **跨版本互通**：`Frame` / `Command` / `CommandResponse` 均带 magic + version 字段，接入方**必须**校验版本，遇到未知版本报 `WrongVersion` 并丢弃，不要盲目解析。

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

---

## 相关

- 空中协议详细字段布局 → `docs/protocol_air.md`
- 接收端参考实现 → `crates/examples/controller-receiver-demo/src/main.rs`
- 控制端参考实现 → `crates/examples/controller-host-demo/src/main.rs`
- 密钥注入实现 → `crates/protocol/build.rs`
