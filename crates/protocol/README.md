# controller-protocol

ESP32 游戏手柄的**纯协议层** —— 无硬件依赖、可跨 target 复用（`no_std` by default）。
手柄固件、Leptos WebBluetooth Dashboard、以及 ESP-NOW 接收/控制端 demo **共用同一份源码**，
协议漂移风险 = 0。

## 特性

| 特性 | 说明 |
|------|------|
| **Frame** (21B, magic 0xC71E) | 手柄→Host 广播摇杆状态，无认证（只读） |
| **Command** (20B, magic 0xCB01, v4) | Host→手柄反向命令，HMAC + seq 抗重放 + 4-bit key_id 密钥轮换 |
| **CommandResponse** (20B, magic 0xCB02, v4) | 手柄→Host 反馈（Ack / Error / Battery / NonceHello） |
| **CRC-16-IBM** | 抗随机噪声 |
| **HMAC-SHA256/4** | 抗签名伪造，nonce 混入抵抗密钥泄漏 |
| **64-bit Anti-Replay window** | 抗抓包重发（per-key-id 独立窗口） |
| **no_std / 定长帧** | 任意 target 编译；little-endian 内存序列化最快 |

## 三种协议帧

| 类型             | 长度 | Magic  | 版本 | 认证 | 抗重放           | 密钥轮换                    | 方向             |
|------------------|------|--------|------|------|------------------|-----------------------------|------------------|
| Frame            | 21 B | 0xC71E | 1    | 无   | 无               | 无                          | 手柄 → Host 广播 |
| Command          | 20 B | 0xCB01 | 4    | HMAC | seq+per-key 窗口 | 4-bit key_id → SHARED_SECRETS | Host → 手柄      |
| CommandResponse  | 20 B | 0xCB02 | 4    | HMAC | req_seq          | 同上                        | 手柄 → Host      |

> Frame 是只读广播，伪造只能让 Host 看到假状态，不会让手柄执行动作，因此无需签名。
> Command / Response 才是"控制面"，必须 HMAC 签名 + 抗重放。
> 完整字节布局见 [`docs/protocol_air.md`](../../docs/protocol_air.md)。

## 模块结构

```text
crates/protocol/src/
├── lib.rs        # 公共 API 面（re-export）+ 设计原则
├── state.rs      # GamepadState（12B）+ ButtonBits 位图
├── frame.rs      # Frame 编解码（21B）
├── command.rs    # Command 编解码（20B）+ 5 种 CommandBody
├── response.rs   # CommandResponse 编解码（20B）+ 4 种 ResponseBody
├── codec.rs      # transmit_frame 的无 HMAC 路径 + DecodeError
├── auth.rs       # HMAC-SHA256 计算 + session nonce + KeyId newtype
├── replay.rs     # AntiReplayWindow（64-bit 滑动窗）
├── crc.rs        # crc16_ibm
├── config.rs     # auth / keyring 常量（SHARED_SECRETS 等）
└── tests/        # proptest 属性测试（8 组往返）
```

## Feature 门控

| feature | 作用 | 谁用 |
|---------|------|------|
| `defmt` | 为错误/状态类型启用 `defmt::Format` | 手柄端 |
| `serde` | 为所有公共类型启用 `Serialize`/`Deserialize` | WASM / Dashboard 端 |
| `std` | 启用 std API（proptest 等） | host 测试 |
| `embed-default-secrets` | **默认开启**；环境变量缺失时用内置弱密钥 fallback | 开发/CI 冒烟 |
| `debug-auth-bypass` | **危险**：编译期关闭 HMAC 校验，仅本地构造报文 | 调试专用 |

### 密钥注入（生产必读）

HMAC 共享密钥通过 **编译期环境变量** 注入（`build.rs` → `env!()`），不落源码：

| 环境变量 | 要求 | 说明 |
|----------|------|------|
| `CONTROLLER_SECRET_V1` | 32 字节 UTF-8 | 主用密钥（必需） |
| `CONTROLLER_SECRET_V2` | 32 字节 UTF-8 | 备用密钥（必需） |

- **关闭 `embed-default-secrets`** 后若未提供环境变量 → **编译期直接 panic**，强迫生产 build 注入密钥；
- **开启（默认）** 时缺失则回退到内置弱密钥，仅用于开发/CI，禁止生产使用；
- `debug-auth-bypass` 仅限本地调试构造报文，生产 build 严禁开启（CI 应 assert 关闭）。

```bash
# 生产构建示例
CONTROLLER_SECRET_V1="$(head -c32 /dev/urandom | base64)" \
CONTROLLER_SECRET_V2="$(head -c32 /dev/urandom | base64)" \
cargo build --no-default-features --features defmt
```

## 安全模型

| 层次 | 措施 | 抵御的威胁 |
|------|------|------------|
| 完整性 | CRC-16-IBM 覆盖全帧 | 无线丢包 / 位翻转 |
| 认证 | HMAC-SHA256 截断 4 字节 | 伪造命令 / 中间人 |
| 抗重放 | 64-bit 滑动窗口 + Session Nonce | 录制回放攻击 |
| 密钥轮换 | 4 slot `key_id` 并存 | 密钥泄露后平滑替换 |

**不解决**：密钥泄漏（固件被物理 dump 后 secret 会暴露）——需配合 flash encryption / secure boot。

## 测试

```bash
cd crates/protocol

# 单元测试 + 属性测试（host target）
cargo test --target aarch64-apple-darwin -- --test-threads=1   # macOS
# cargo test --target x86_64-unknown-linux-gnu -- --test-threads=1   # Linux

# 仅跑属性测试
cargo test --features std proptest
```

- 72 个单元测试 + 8 组 proptest 往返测试，覆盖 3 种帧的 encode→decode 对称性、CRC/HMAC 校验失败路径、抗重放窗口边界。

## 作为依赖复用

```toml
# 接收端 / 控制端（纯 no_std）
controller-protocol = { path = "../../crates/protocol", default-features = false, features = ["defmt"] }

# Dashboard（WASM，需要 serde）
controller-protocol = { path = "../../crates/protocol", default-features = false, features = ["serde"] }
```

## 相关文档

- 空中协议对照 → [`docs/protocol_air.md`](../../docs/protocol_air.md)
- 接收端参考实现 → [`docs/esp_now_receiver.md`](../../docs/esp_now_receiver.md)
- 控制端参考实现 → [`docs/esp_now_controller.md`](../../docs/esp_now_controller.md)
- 手柄固件（本 crate 的使用方）→ [`src/bin/main.rs`](../../src/bin/main.rs)
