# Receiver 项目引用 `controller-protocol` 的最小示例

本文件配合 tag [`protocol-v0.2.0`](https://github.com/lf-wxp/controller/releases/tag/protocol-v0.2.0) 使用，
展示**另一个 receiver 项目**如何以 Git 依赖方式复用本 crate，**无需拷贝源码**。

> 详细的 feature 组合矩阵、密钥注入契约、常见坑请见 [`../USAGE.md`](../USAGE.md)。
> 本文件只给"最小可粘贴"的骨架。

---

## 0. Tag 快照

```text
tag:    protocol-v0.2.0
commit: 5eda0a8        # git rev-parse protocol-v0.2.0^{}
crate:  controller-protocol
repo:   https://github.com/lf-wxp/controller
path:   crates/protocol         # Cargo 会自动在 workspace 中定位
```

---

## 1. Cargo.toml —— 3 类 target 逐个对应

### 1.1 ESP32 no_std 接收器（最常见场景）

```toml
[package]
name = "my-receiver"
edition = "2024"
rust-version = "1.88"

[dependencies]
controller-protocol = {
  git = "https://github.com/lf-wxp/controller",
  tag = "protocol-v0.2.0",
  default-features = false,      # 关掉 embed-default-secrets，强制注入真密钥
  features = ["defmt"],          # 仅启用 defmt::Format 集成
}

# 你自己的 esp-hal / esp-radio / defmt 依赖照旧
esp-hal   = { version = "~1.1.0", features = ["esp32", "unstable"] }
esp-radio = { version = "0.18.0", features = ["esp-now", "esp32"] }
defmt     = "1"
```

### 1.2 PC / Linux 网关（std host，比如把命令帧转发到 MQTT）

```toml
[dependencies]
controller-protocol = {
  git = "https://github.com/lf-wxp/controller",
  tag = "protocol-v0.2.0",
  default-features = false,
  features = ["std", "serde"],   # std：alloc/vec；serde：跨语言 JSON
}
tokio = { version = "1", features = ["full"] }
```

### 1.3 WASM / Web Dashboard（浏览器侧签发命令）

```toml
[dependencies]
controller-protocol = {
  git = "https://github.com/lf-wxp/controller",
  tag = "protocol-v0.2.0",
  default-features = false,
  features = ["serde"],
}
wasm-bindgen = "0.2"
```

> ⚠️ **生产 WASM 打包时**：`CONTROLLER_SECRET_V1/V2` 会被 `env!()` 嵌进产物，
> 意味着**任何拿到 .wasm 的人都能反编译出密钥**。因此浏览器端的"签命令"能力必须要么：
> (a) 只在受信任的内网/网关侧完成，浏览器只发 UI 意图；或
> (b) 通过后端 API 代签，不把密钥交给前端。
> 见 [USAGE.md §6 常见坑 #4](../USAGE.md)。

---

## 2. 最小接收端代码（no_std / ESP-NOW）

只做一件事：**收 25B `Frame` 广播 → 打印摇杆状态**。

```rust
#![no_std]
#![no_main]

use controller_protocol::{Frame, GamepadState, DecodeError};
use defmt::info;

/// ESP-NOW 回调里拿到的原始 25 字节 payload
pub fn on_espnow_frame(payload: &[u8]) {
  // 早退：长度不对直接丢弃（协议是定长）
  if payload.len() != Frame::LEN {
    return;
  }

  // Frame 是只读广播，无 HMAC；decode 只做 magic + CRC 校验
  match Frame::decode(payload) {
    Ok(frame) => handle_state(&frame.state),
    Err(DecodeError::BadMagic)   => info!("drop: bad magic"),
    Err(DecodeError::BadCrc)     => info!("drop: crc mismatch"),
    Err(DecodeError::BadLength)  => info!("drop: bad length"),
    Err(e) => info!("drop: {:?}", e),
  }
}

fn handle_state(state: &GamepadState) {
  info!(
    "lx={} ly={} rx={} ry={} buttons={:x}",
    state.left_x, state.left_y, state.right_x, state.right_y,
    state.buttons.bits(),
  );
}
```

---

## 3. 最小控制端代码（std host / 发命令 + 收响应）

```rust
use controller_protocol::{
  Command, CommandBody, CommandResponse, KeyId,
  auth::SessionNonce, replay::AntiReplayWindow,
};

/// 单调递增的 seq，per key_id 独立
static mut TX_SEQ: u64 = 0;

fn send_rumble(strength: u8) -> [u8; Command::LEN] {
  let seq = unsafe { TX_SEQ += 1; TX_SEQ };
  let cmd = Command::new(
    KeyId::V1,                              // 主密钥
    seq,
    CommandBody::Rumble { strength },
    SessionNonce::from_bytes([0xAB; 4]),    // 由 NonceHello 协商得到
  );
  cmd.encode_signed()                       // 20 字节，含 HMAC-SHA256/4
}

/// 收响应侧要维护一个 per-key_id 的抗重放窗口
fn on_response(bytes: &[u8], window: &mut AntiReplayWindow) {
  let rsp = match CommandResponse::decode_verified(bytes, /* nonce = */ &Default::default()) {
    Ok(r) => r,
    Err(e) => { eprintln!("drop response: {e:?}"); return; }
  };
  if !window.check_and_update(rsp.req_seq) {
    eprintln!("replay detected on req_seq={}", rsp.req_seq);
    return;
  }
  println!("{:?}", rsp.body);
}
```

> 上面的 `SessionNonce::from_bytes([0xAB; 4])` 是**举例**。真实握手：
> 手柄上电后主动广播 `NonceHello`，控制端把它记下来作为签名混入值。
> 参考实现见根仓库 [`crates/examples/controller-host-demo/`](../../../crates/examples/controller-host-demo/)。

---

## 4. 密钥注入（生产必读）

无论哪种 target，**编译时都必须**通过环境变量注入 32 字节密钥（缺一不可）：

```bash
# ⚠️ 恰好 32 字节 ASCII，多一字节少一字节都会 build.rs panic
export CONTROLLER_SECRET_V1="$(openssl rand -base64 24 | head -c 32)"
export CONTROLLER_SECRET_V2="$(openssl rand -base64 24 | head -c 32)"

cargo build --release
```

**必须**关闭 `embed-default-secrets`（上文 Cargo.toml 已 `default-features = false`），
否则未注入时会静默 fallback 到内置弱密钥 `esp32-controller-shared-key-v1!!` —— 生产事故。

### CI 断言（防呆）

在 receiver 项目的 CI 里加一条检查，杜绝有人偷偷开回 `embed-default-secrets` 或 `debug-auth-bypass`：

```yaml
# .github/workflows/ci.yml
- name: Assert no dangerous features leak into release
  run: |
    if cargo tree -e features -p controller-protocol \
        | grep -E "embed-default-secrets|debug-auth-bypass"; then
      echo "::error::controller-protocol dangerous feature enabled in release build"
      exit 1
    fi
```

---

## 5. 版本与依赖

Receiver 项目直接引用当前版本即可：

```toml
# Cargo.toml
controller-protocol = {
  git = "https://github.com/lf-wxp/controller",
  tag = "protocol-v0.2.0",
  default-features = false,
  features = ["defmt"],
}
```

```bash
cargo update -p controller-protocol
cargo build --release
```

本 crate 的字节布局以 crate 内 `PROTOCOL_VERSION` / `COMMAND_VERSION` / `RESPONSE_VERSION` 常量为准，
接入方无需在业务代码里硬编码版本号。

---

## 6. 联调核对清单

打完 tag、receiver 侧拉下来后，先跑一遍这五条：

- [ ] `cargo tree -p controller-protocol` 显示 tag 匹配 `protocol-v0.2.0`
- [ ] `cargo tree -e features -p controller-protocol` **不含** `embed-default-secrets` / `debug-auth-bypass`
- [ ] `env | grep CONTROLLER_SECRET_V` 两条都是 32 字节
- [ ] 空跑一次 `Frame::decode(&[0u8; 21])` → 应返回 `Err(BadMagic)`（证明协议 crate 真的被链接进来了）
- [ ] `KeyId::V1` 与手柄端约定的 key slot 一致（4-bit，共 16 个 slot）

任何一条不过，先别写业务代码。
