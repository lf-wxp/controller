# controller — ESP32 Game Controller Workspace

> [!NOTE]
> Rust cargo workspace，包含 **ESP32 手柄固件** + **纯协议 crate** + **Leptos WebBluetooth 调试面板**。
> **核心亮点**：协议逻辑（encode / decode / CRC / HMAC / replay window / dest_mask 位图寻址）在三处共享同一份 Rust 源码，
> 杜绝"手柄改协议 → dashboard 忘同步"的常见 bug。
>
> **起支持多接收方寻址**：Frame 携带 `dest_mask: u32`（32 slot 位图），
> 配合 [`PeerRegistry`](crates/comm/src/peer_registry.rs) + `Announce`/`AnnounceReply`/`AssignId`
> 三元通道，手柄可动态发现最多 32 台 ESP-NOW 接收方，并用 OLED + 摇杆选择
> 单播 / 组播 / 广播目标。详见
> [`docs/protocol_air.md`](docs/protocol_air.md) 与
> [`crates/protocol/USAGE.md`](crates/protocol/USAGE.md)。

![workspace](https://img.shields.io/badge/workspace-cargo-blue)
![no_std](https://img.shields.io/badge/protocol-no__std-green)
![target](https://img.shields.io/badge/target-esp32--xtensa-orange)
![ci](https://img.shields.io/badge/CI-cargo--makepassed-brightgreen)

---

## 📑 目录

- [Workspace 布局](#-workspace-布局)
- [常用命令](#-常用命令)
- [协议特性总览](#-协议特性总览)
- [文档地图](#-文档地图)

> 手柄固件（硬件引脚 / embassy 任务 / OLED / Feature 开关等）已独立到
> [`crates/controller/README.md`](crates/controller/README.md)。

---

## 📦 Workspace 布局

```text
controller/
├── Cargo.toml                    # workspace root（virtual manifest）
├── Makefile.toml                 # cargo-make 统一构建入口（推荐）
├── crates/
│   ├── controller/               # controller：ESP32 手柄固件（esp32 xtensa target）
│   │   ├── README.md             #   固件专属文档（硬件 / 架构 / Feature）
│   │   ├── src/bin/main.rs       #   embassy 主循环
│   │   ├── src/lib.rs            #   crate root：pub static REGISTRY
│   │   ├── src/config.rs         #   硬件配置（引脚 / 电池 / persist ...）
│   │   ├── src/hal/              #   硬件抽象层（按钮/摇杆/LED/OLED/RNG/NVS）
│   │   ├── src/input/            #   输入采样聚合
│   │   ├── src/transport/        #   BLE HID + ESP-NOW + 控制命令分发
│   │   ├── src/ui/               #   SSD1306 OLED 渲染 + 接收方选择器
│   │   └── tests/hello_test.rs   #   手柄真机 embedded-test
│   ├── protocol/                 # protocol：纯 no_std 协议 crate
│   │   ├── src/                  #   crc / auth / state / frame / codec
│   │   │                         #   command / response / replay / config
│   │   └── tests/                #   proptest 属性测试（8 组）
│   ├── comm/                     # controller-comm：PeerRegistry 等通信支持
│   ├── c6/                       # ESP32-C6 相关 crate
│   ├── dashboard/                # controller-dashboard：Leptos WebBluetooth UI
│   │   ├── src/main.rs           #   Leptos 挂载
│   │   ├── src/bluetooth.rs      #   WebBluetooth 手工 wasm-bindgen 绑定
│   │   └── src/components/       #   UI 组件
│   └── examples/                 # 纯 host 侧 demo（CI 跑）
└── .github/workflows/ci.yml      # cargo make ci 单一 job
```

---

## 🛠️ 常用命令

### 🚀 推荐入口：`cargo make`

本项目使用 [cargo-make](https://github.com/sagiegurari/cargo-make) 作为**统一构建入口**，
一次性屏蔽三个 crate 各自不同的 target/toolchain/参数。

```bash
# 一次性安装（本机 / CI）
cargo install cargo-make --locked

# 列出所有可用任务
cargo make            # 打印帮助
cargo make --list-all-steps

# ------- 常用 -------
cargo make quick             # 手柄 check + clippy（30 秒内）
cargo make ci                # 完整 CI 流程（≈ 5 秒本地首次运行后）
cargo make fmt               # 三 crate 一键格式化
cargo make fmt-check         # 格式检查（不修改）

# ------- 手柄固件 -------
cargo make controller-check
cargo make controller-clippy
cargo make controller-build           # 手柄 dev 构建
cargo make controller-build-release   # 手柄 release 构建（LTO）
cargo make controller-run             # 烧录 + probe-rs 实时日志

# ------- 协议 crate -------
cargo make protocol-check
cargo make protocol-clippy
cargo make protocol-test              # 72 单元测试 + 8 proptest

# ------- Dashboard -------
cargo make dashboard-check
cargo make dashboard-clippy
cargo make dashboard-serve            # trunk serve --open（自动装 trunk）
cargo make dashboard-build            # trunk build --release

# ------- 聚合 -------
cargo make check-all         # 三 crate check
cargo make clippy-all        # 三 crate clippy 严格模式
cargo make clean             # 清理所有产物
```

**核心优势**：`cargo make ci` 与 GitHub CI 跑**完全相同**的命令，本地跑通即 CI 跑通。

### 手动命令（底层参考）

如果不装 cargo-make，也可以直接用 cargo：

#### 手柄固件（esp32 xtensa target，默认工作流）

```bash
# 在项目根跑（自动用 .cargo/config.toml 的 xtensa 默认 target）
cargo build --bin controller           # 编译
cargo check --lib --bins               # 类型检查
cargo clippy --lib --bins -- -D warnings

# 真机烧录 + 日志（需要 probe-rs）
cargo run --bin controller

# 真机测试（embedded-test framework）
cargo test --test hello_test
```

#### 协议 crate 单元测试 + property-based 测试（host target）

```bash
cd crates/protocol
cargo test --target aarch64-apple-darwin -- --test-threads=1
# Linux:
# cargo test --target x86_64-unknown-linux-gnu -- --test-threads=1
```

#### Dashboard 开发（浏览器 WASM）

```bash
# 前置：一次性安装
rustup target add wasm32-unknown-unknown
cargo install trunk

# 启动开发服务器（浏览器自动打开 http://127.0.0.1:8080）
cd crates/dashboard
trunk serve --open

# 生产构建
trunk build --release
```

---

## 🔐 协议特性总览

| 特性 | 说明 |
|------|------|
| **Frame** (25B, magic 0xC71E) | 手柄→Host 广播摇杆状态 + `dest_mask: u32` 位图寻址（32 slot），CRC-16 校验、无 HMAC 签名 |
| **Command** (24B, magic 0xCB01) | Host→手柄反向命令，HMAC + seq 抗重放 + 4-bit key_id 密钥轮换，payload 10B |
| **CommandResponse** (24B, magic 0xCB02) | 手柄→Host 反馈（Ack / Error / Battery / NonceHello / AnnounceReply） |
| **Announce / AssignId** (Command) | 手柄侧发现 + 分配 `receiver_id` 的下行通道 |
| **AnnounceReply** (Response) | 接收方回报自身 mac + role_tag + rssi_dbm，供手柄侧 [`PeerRegistry`](crates/comm/src/peer_registry.rs) 入库 |
| **`dest_mask` 位图寻址** | Frame 携带 `u32` 位图；`bit-i == 1` 表示 `receiver_id == i` 处理该帧；`0xFFFF_FFFF` = 广播；`0` = 静默丢弃 |
| **CRC-16-IBM** | 抗随机噪声 |
| **HMAC-SHA256/4** | 抗签名伪造，nonce 混入抵抗密钥泄漏 |
| **64-bit Anti-Replay window** | 抗抓包重发（per-key-id 独立窗口） |
| **NVS 双缓冲持久化** | flash 抗掉电，重启保留灵敏度 + replay 窗口 |
| **HW RNG** | ESP32 硬件 RNG + 时钟抖动 XOR 生成 session nonce seed |

详细协议格式（字节布局 / 源码位置 / 安全模型）见
[`docs/protocol_air.md`](docs/protocol_air.md)，以及
`crates/protocol/src/{command,response,frame}.rs` 顶部注释表格。

---

## 📚 文档地图

> 想快速上手？先看三篇用户指南（`esp_now_receiver` / `esp_now_controller` /
> dashboard README）；`protocol_air.md` 仅在你需要**深入理解协议**或
> **实现自定义主机**时查阅。

| 文档 | 面向读者 | 内容 |
| :--- | :--- | :--- |
| [`crates/controller/README.md`](crates/controller/README.md) | 手柄固件开发者 | 硬件引脚、embassy 任务架构、多接收方选择、OLED、Feature 开关 |
| [`docs/protocol_air.md`](docs/protocol_air.md) | 协议实现者 / 自定义主机开发者 | 3 种空中帧的完整字节布局、安全模型、时间表 |
| [`docs/esp_now_receiver.md`](docs/esp_now_receiver.md) | ESP32 接收端开发者 | 只读订阅手柄状态的参考实现 |
| [`docs/esp_now_controller.md`](docs/esp_now_controller.md) | ESP32 控制端开发者 | 主动下发命令（HMAC / Nonce 握手 / 抗重放）参考实现 |
| [`crates/dashboard/README.md`](crates/dashboard/README.md) | Web 调试面板使用者 | Leptos WebBluetooth 调试面板使用说明 |
| [`crates/protocol/README.md`](crates/protocol/README.md) | 协议 crate 使用者 | `protocol` 设计原则、Feature 门控、测试 |
| [`crates/protocol/USAGE.md`](crates/protocol/USAGE.md) | 协议 crate 使用者 | API 用法（Frame::with_dest / Announce / AnnounceReply / AssignId 完整示例） |
| [`crates/examples/controller-receiver-demo/`](crates/examples/controller-receiver-demo/) | 无硬件验证者 | 纯 host 侧 Frame 编解码 + seq gap 检测 + `dest_mask` 过滤 demo（CI 跑） |
| [`crates/examples/controller-host-demo/`](crates/examples/controller-host-demo/) | 无硬件验证者 | 纯 host 侧 Command/Response 双向交互 demo（CI 跑） |
