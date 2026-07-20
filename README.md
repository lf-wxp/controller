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

## � 目录

- [Workspace 布局](#-workspace-布局)
- [常用命令](#-常用命令)
- [协议特性总览](#-协议特性总览)
- [硬件清单](#-硬件清单esp32-wroom-32e)
- [固件架构](#-固件架构手柄主程序)
- [配置开关](#-配置开关编译期常量--feature)
- [文档地图](#-文档地图)

---

## �📦 Workspace 布局

```text
controller/
├── Cargo.toml                    # workspace root + 手柄 package
├── Makefile.toml                 # cargo-make 统一构建入口（推荐）
├── src/                          # 手柄固件（esp32 xtensa target）
│   ├── bin/main.rs               # embassy 主循环
│   ├── protocol.rs               # re-export shim → controller_protocol::*
│   ├── lib.rs                    # crate root：pub static REGISTRY: PeerRegistry
│   ├── config.rs                 # 硬件配置（引脚 / 电池 / persist ...）
│   ├── hal/                      # 硬件抽象层（按钮/摇杆/LED/OLED/RNG/NVS）
│   ├── input/                    # 输入采样聚合
│   ├── transport/                # BLE HID + ESP-NOW + 控制命令分发
│   └── ui/                       # SSD1306 OLED 渲染 + 接收方选择器
├── crates/
│   ├── protocol/                 # controller-protocol：纯 no_std 协议 crate
│   │   ├── Makefile.toml         # 子 crate cargo-make 任务
│   │   ├── src/                  #   crc / auth / state / frame / codec
│   │   │                         #   command / response / replay / config
│   │   └── tests/                #   proptest 属性测试（8 组）
│   └── dashboard/                # controller-dashboard：Leptos WebBluetooth UI
│       ├── Makefile.toml         # 子 crate cargo-make 任务
│       ├── src/main.rs           #   Leptos 挂载
│       ├── src/bluetooth.rs      #   WebBluetooth 手工 wasm-bindgen 绑定
│       └── src/components/       #   4 个 UI 组件
├── tests/hello_test.rs           # 手柄真机 embedded-test
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

## 🧩 硬件清单（ESP32-WROOM-32E）

所有引脚集中在 [`crates/controller/src/config.rs`](crates/controller/src/config.rs) 的 `pins` 模块，更换硬件只改这一处。
⚠️ 标记的引脚为 **strapping pin**（上电电平会影响启动模式），需注意上下拉。

### 输入

| 信号 | GPIO | 备注 |
|------|------|------|
| 摇杆 X | IO32 (ADC1_CH4) | Attenuation 11dB |
| 摇杆 Y | IO33 (ADC1_CH5) | Attenuation 11dB |
| 摇杆按下键 | IO12 | ⚠️ strapping，需 Pull::Down |
| 按钮 1–4 | IO27 / IO13 / IO25 / IO23 | 按下拉低，内部上拉 |
| 旋钮 1 | IO36 (ADC1_CH0, SENSOR_VP) | 仅输入 |
| 旋钮 2 | IO39 (ADC1_CH3, SENSOR_VN) | 仅输入 |
| 电池电压 | IO34 (ADC1_CH6) | 1/2 分压电路（100kΩ + 100kΩ） |

### 输出 / 通信

| 信号 | GPIO | 备注 |
|------|------|------|
| LED 1 | IO5 | ⚠️ strapping，上电可能短暂高电平 |
| LED 2 | IO18 | |
| 彩灯（4 颗并联） | IO15 | ⚠️ strapping；4 颗 LED 阳极接 IO15、阴极接 GND（每路带限流电阻），推挽输出置高点亮，由 `led_effects_task` 持续闪烁。**原为拨动开关输入，因该脚实为彩灯驱动节点、无接地通路故读输入恒 HIGH，已改为输出驱动** |
| I²C SDA | IO21 | OLED 128×64 |
| I²C SCL | IO22 | OLED 地址 0x3C |

> 电池分压接线（`VBAT ─[R1=100kΩ]──┬── GPIO34 ─[R2=100kΩ]── GND`），分压比 1/2，
> 满电 4.2V 经分压到 GPIO34 = 2.1V，配合 11dB 满量程 3.3V 可测。详见
> [`crates/controller/src/config.rs`](crates/controller/src/config.rs) 注释。

---

## 🏗️ 固件架构（手柄主程序）

[`crates/controller/src/bin/main.rs`](crates/controller/src/bin/main.rs) 初始化硬件后 spawn 一组 embassy 任务，主循环只做
"采样 + 分频发送"，避免多任务同步复杂度。

### 任务（task）一览

| 任务 | 源文件 | 职责 |
|------|--------|------|
| `esp_now_broadcast_task` | [`crates/controller/src/transport/esp_now/mod.rs`](crates/controller/src/transport/esp_now/mod.rs) | ESP-NOW 广播 Frame（约 30 Hz）+ 出站 Command（Announce / AssignId）|
| `esp_now_receive_task` | 同上 | 接收 Command / Response 帧；`AnnounceReply` 交 [`REGISTRY.upsert`](crates/controller/src/lib.rs) 入库 |
| `nonce_broadcast_task` | 同上 | 每 5s 广播 `NonceHello`（K3） |
| `battery_monitor_simulated_task` | [`crates/controller/src/hal/battery.rs`](crates/controller/src/hal/battery.rs) | 模拟电量递减（可换真实测量） |
| `ble_gamepad_task` | [`crates/controller/src/transport/ble_hid/mod.rs`](crates/controller/src/transport/ble_hid/mod.rs) | BLE HID Gamepad + 自定义 GATT |
| `led_effects_task` | [`crates/controller/src/hal/led_effects.rs`](crates/controller/src/hal/led_effects.rs) | LED1/LED2 特效队列（闪烁 / 心跳）+ 彩灯（IO15）持续闪烁 |
| `oled_task` | [`crates/controller/src/ui/mod.rs`](crates/controller/src/ui/mod.rs) | SSD1306 渲染（约 20 Hz） |
| `persist_worker_*_task` | [`crates/controller/src/hal/persist.rs`](crates/controller/src/hal/persist.rs) | 后台异步落盘（NVS 双缓冲 / 内存） |

### 主循环节拍

```text
INPUT_SCAN_INTERVAL_MS = 10ms (100Hz)  ── 每次：采样 + 本地 LED 反馈 + 应用灵敏度
        │  每 TRANSMIT_EVERY_N (=3) 次采样
        ▼
TRANSMIT_INTERVAL_MS = 33ms (≈30Hz)   ── 构造 Frame，一次 send() 分发到：
                                          ├─ BLE HID  (手机/PC 通用手柄)
                                          ├─ ESP-NOW  (自定义 ESP32 接收端)
                                          └─ OLED UI  (本地屏幕)
```

发送通过 [`CompositeTransport`](crates/controller/src/transport/mod.rs) 组合：一次 `send()` 同时送达三路，
任一失败不影响其它（见 `crates/controller/src/transport/mod.rs` 的 `CompositeError` 语义）。

### 多接收方选择

手柄可动态发现最多 32 台 ESP-NOW 接收方，并在 OLED 上用摇杆挑选发送目标。全流程如下：

```text
[1] 长按摇杆按钮(IO12) 800ms ─► 进入 Selecting 模式
                              └─ 主循环发一次 Announce（Command，24B，广播）

[2] 所有 receiver 收到 Announce ─► 回 AnnounceReply（Response，24B）
                                    payload = { mac[6], rssi_dbm, role_tag[3] }

[3] esp_now_receive_task 收到 AnnounceReply
    └─ REGISTRY.upsert(mac, role, rssi, now)
       ├─ Inserted { id } → 单播 AssignId 让 receiver 记住逻辑 id
       ├─ Updated  { id } → 只刷 rssi / last_seen
       └─ Full            → 32 slot 已用完

[4] Selecting 面板实时渲染 REGISTRY.snapshot()
    ├─ 摇杆 Y      = 光标上下移动
    ├─ Btn1        = 加入 pending_mask (bit-i where i == receiver_id)
    ├─ Btn2        = 移出 pending_mask
    └─ 再长按摇杆按钮(IO12) = 保存 pending_mask → ACTIVE_DEST_MASK，退出 Selecting

[5] Normal 模式发帧
    └─ Frame::with_dest(seq, state, active_dest_mask())
       ├─ 未改动     → 0xFFFF_FFFF（广播）
       ├─ 单选一台   → 1 << receiver_id
       └─ 多选 N 台  → 位或

[6] 接收方过滤（frame.is_addressed_to(my_id)）
    └─ bit 未置位 → 静默丢弃（CRC 通过后、业务处理前）
```

关键源码：
- 选择器状态机 & OLED 面板：[`crates/controller/src/ui/selector.rs`](crates/controller/src/ui/selector.rs) + [`crates/controller/src/ui/layout.rs`](crates/controller/src/ui/layout.rs)
- Peer 目录：[`crates/comm/src/peer_registry.rs`](crates/comm/src/peer_registry.rs)（`heapless::Vec<PeerEntry, 32>`，无堆分配；全局单例在 [`crates/controller/src/lib.rs`](crates/controller/src/lib.rs) 的 `pub static REGISTRY`）
- Announce 编排：[`crates/controller/src/transport/esp_now/mod.rs`](crates/controller/src/transport/esp_now/mod.rs) 的
  `broadcast_announce` / `send_assign_id` / `handle_incoming_response`
- 目标位图集成：[`crates/controller/src/bin/main.rs`](crates/controller/src/bin/main.rs) 里 `Frame::with_dest(seq, state, active_dest_mask())`

### OLED 屏布局（128×64，FONT_6X10）

```text
行 0 (y=0)  : Ctl  B● N● H●   99%      （设备名 + BLE/NOW/心跳 状态灯 + 电量）
行 1 (y=10) : ─────────────────────────
行 2 (y=20) : Joy X=+1000  Y=-1000
行 3 (y=30) : Kn  K1=1000 K2=1000
行 4 (y=40) : Btn [1][2][3][4][J]   （按下按钮反色方块）
行 5 (y=50) : Seq 0x0000ABCD  30Hz
```

低电量时屏幕四周绘制闪烁边框（B 选项），`ShowToast` 命令会在底部两行弹出反色提示条。

---

## ⚙️ 配置开关（编译期常量 / Feature）

代码注释中频繁出现带字母的"选项"代号，便于快速定位功能。汇总如下：

| 代号 | 选项 | 位置 | 说明 |
|------|------|------|------|
| **K** | HMAC 鉴权 | `controller-protocol::config::auth` | Command/Response 帧 4B HMAC-SHA256 截断 |
| **K2** | 抗重放窗口 | `protocol::replay` + `transport/control` | per-key-id 64-bit 滑动窗 |
| **K3** | Session Nonce | `crates/controller/src/bin/main.rs` + `nonce_broadcast_task` | 上电 HW RNG 生成，每 5s 广播 |
| **K4** | 持久化加载 | `transport/control` + `hal/persist` | 启动时回填灵敏度 / 电量模式 / replay 窗 |
| **P** | NVS 落盘 | `config::persist::USE_NVS_STORAGE` | `false`=内存（默认），`true`=flash 双缓冲 |
| **O** | 密钥轮换 | `config::keyring::SHARED_SECRETS` | 4 个并存 key_id，平滑切换 |
| **U** | replay 窗持久化 | `hal/persist::PersistentConfig` | 重启保留抗重放窗口 |
| **L** | 低电量告警 | `ui/layout.rs::draw_low_battery_border` | 屏幕闪烁边框 |
| **H** | 自动回执 | `transport/control::broadcast_response` | 命令执行后广播 Ack |
| **Z** | 属性测试 | `crates/protocol/tests/` | proptest 往返测试（host 运行） |

> ⚠️ **生产部署注意**：`controller-protocol` 默认启用 `embed-default-secrets`（弱 fallback 密钥），
> **生产 build 应关闭该 feature 并通过环境变量注入独立高熵密钥**；`debug-auth-bypass`
> 仅供本地构造报文，严禁用于生产（CI 应 assert 关闭）。

```bash
# 生产构建：关闭弱密钥回退，强制从环境变量注入密钥（缺失即编译失败）
CONTROLLER_SECRET_V1="$(head -c32 /dev/urandom | base64)" \
CONTROLLER_SECRET_V2="$(head -c32 /dev/urandom | base64)" \
cargo build -p controller-protocol --no-default-features --features defmt
```


---

## 📚 文档地图

> 想快速上手？先看三篇用户指南（`esp_now_receiver` / `esp_now_controller` /
> dashboard README）；`protocol_air.md` 仅在你需要**深入理解协议**或
> **实现自定义主机**时查阅。

| 文档 | 面向读者 | 内容 |
| :--- | :--- | :--- |
| [`docs/protocol_air.md`](docs/protocol_air.md) | 协议实现者 / 自定义主机开发者 | 3 种空中帧的完整字节布局、安全模型、时间表 |
| [`docs/esp_now_receiver.md`](docs/esp_now_receiver.md) | ESP32 接收端开发者 | 只读订阅手柄状态的参考实现 |
| [`docs/esp_now_controller.md`](docs/esp_now_controller.md) | ESP32 控制端开发者 | 主动下发命令（HMAC / Nonce 握手 / 抗重放）参考实现 |
| [`crates/dashboard/README.md`](crates/dashboard/README.md) | Web 调试面板使用者 | Leptos WebBluetooth 调试面板使用说明 |
| [`crates/protocol/README.md`](crates/protocol/README.md) | 协议 crate 使用者 | `controller-protocol` 设计原则、Feature 门控、测试 |
| [`crates/protocol/USAGE.md`](crates/protocol/USAGE.md) | 协议 crate 使用者 | API 用法（Frame::with_dest / Announce / AnnounceReply / AssignId 完整示例） |
| [`crates/examples/controller-receiver-demo/`](../crates/examples/controller-receiver-demo/) | 无硬件验证者 | 纯 host 侧 Frame 编解码 + seq gap 检测 + `dest_mask` 过滤 demo（CI 跑） |
| [`crates/examples/controller-host-demo/`](../crates/examples/controller-host-demo/) | 无硬件验证者 | 纯 host 侧 Command/Response 双向交互 demo（CI 跑） |
