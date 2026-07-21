# controller — ESP32 手柄固件

**ESP32-WROOM-32E 游戏手柄主程序**——embassy 异步运行时，一次采样同时经
BLE HID / ESP-NOW / OLED 三路分发；ESP-NOW 侧携带 `dest_mask: u32` 位图，
可动态发现最多 32 台接收方并用摇杆在 OLED 上选择单播 / 组播 / 广播目标。

> 协议逻辑（encode / decode / CRC / HMAC / replay window / dest_mask）复用
> [`protocol`](../protocol/README.md)，peer 目录复用
> [`comm::PeerRegistry`](../comm/src/peer_registry.rs)，杜绝协议漂移。
> workspace 总览 / 构建入口见 [根 README](../../README.md)。

---

## 📑 目录

- [常用命令](#-常用命令)
- [硬件清单](#-硬件清单esp32-wroom-32e)
- [固件架构](#-固件架构)
- [多接收方选择](#-多接收方选择)
- [开机自检屏（Boot POST）](#-开机自检屏boot-post)
- [OLED 屏布局](#-oled-屏布局12864font_6x10)
- [配置开关（编译期常量 / Feature）](#-配置开关编译期常量--feature)

---

## 🛠️ 常用命令

推荐用 workspace 根的 `cargo make`（统一 target / toolchain）：

```bash
cargo make controller-check
cargo make controller-clippy
cargo make controller-build           # dev 构建
cargo make controller-build-release   # release 构建（LTO）
cargo make controller-run             # 烧录 + probe-rs 实时日志
```

手动 cargo（在项目根跑，自动用 `.cargo/config.toml` 的 xtensa 默认 target）：

```bash
cargo build --bin controller           # 编译
cargo check --lib --bins               # 类型检查
cargo clippy --lib --bins -- -D warnings
cargo run --bin controller             # 真机烧录 + 日志（需要 probe-rs）
cargo test --test hello_test           # 真机测试（embedded-test framework）
```

---

## 🧩 硬件清单（ESP32-WROOM-32E）

所有引脚集中在 [`src/config.rs`](src/config.rs) 的 `pins` 模块，更换硬件只改这一处。
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
> [`src/config.rs`](src/config.rs) 注释。

---

## 🏗️ 固件架构

[`src/bin/main.rs`](src/bin/main.rs) 初始化硬件后 spawn 一组 embassy 任务，主循环只做
"采样 + 分频发送"，避免多任务同步复杂度。

### 任务（task）一览

| 任务 | 源文件 | 职责 |
|------|--------|------|
| `esp_now_broadcast_task` | [`src/transport/esp_now/mod.rs`](src/transport/esp_now/mod.rs) | ESP-NOW 广播 Frame（约 30 Hz）+ 出站 Command（Announce / AssignId）|
| `esp_now_receive_task` | 同上 | 接收 Command / Response 帧；`AnnounceReply` 交 [`REGISTRY.upsert`](src/lib.rs) 入库 |
| `nonce_broadcast_task` | 同上 | 每 5s 广播 `NonceHello`（K3） |
| `battery_monitor_simulated_task` | [`src/hal/battery.rs`](src/hal/battery.rs) | 模拟电量递减（可换真实测量） |
| `ble_gamepad_task` | [`src/transport/ble_hid/mod.rs`](src/transport/ble_hid/mod.rs) | BLE HID Gamepad + 自定义 GATT |
| `led_effects_task` | [`src/hal/led_effects.rs`](src/hal/led_effects.rs) | LED1/LED2 特效队列（闪烁 / 心跳）+ 彩灯（IO15）持续闪烁 |
| `oled_task` | [`src/ui/mod.rs`](src/ui/mod.rs) | SSD1306 渲染（约 20 Hz） |
| `persist_worker_*_task` | [`src/hal/persist.rs`](src/hal/persist.rs) | 后台异步落盘（NVS 双缓冲 / 内存） |

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

发送通过 [`CompositeTransport`](src/transport/mod.rs) 组合：一次 `send()` 同时送达三路，
任一失败不影响其它（见 `src/transport/mod.rs` 的 `CompositeError` 语义）。

---

## 🎯 多接收方选择

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

- 选择器状态机 & OLED 面板：[`src/ui/selector.rs`](src/ui/selector.rs) + [`src/ui/layout.rs`](src/ui/layout.rs)
- Peer 目录：[`comm::PeerRegistry`](../comm/src/peer_registry.rs)（`heapless::Vec<PeerEntry, 32>`，无堆分配；全局单例在 [`src/lib.rs`](src/lib.rs) 的 `pub static REGISTRY`）
- Announce 编排：[`src/transport/esp_now/mod.rs`](src/transport/esp_now/mod.rs) 的
  `broadcast_announce` / `send_assign_id` / `handle_incoming_response`
- 目标位图集成：[`src/bin/main.rs`](src/bin/main.rs) 里 `Frame::with_dest(seq, state, active_dest_mask())`

---

## 🩺 开机自检屏（Boot POST）

上电后 OLED 先停留约 1.5s 展示一屏硬件自检结果，再切入正常界面：

```text
BOOT SELF-TEST
─────────────────
SELF   OK        （协议 / 构建不变式自检，见 self_test.rs）
RADIO  OK        （Wi-Fi / BLE 控制器初始化）
OLED   OK        （I²C 0x3C 探测应答）
ADC    OK        （摇杆 X/Y + 两旋钮原始读数未在轨值）
```

`SELF` / `RADIO` 走到这里必为 OK（失败会提前 panic 复位）；真正可能显示 `FAIL`
的是 `OLED`（总线无 ACK）与 `ADC`（某路读数处于轨值，疑似浮空/短路）。自检为
**非致命诊断**——异常只作提示、不阻断启动。详见 [`src/hal/post.rs`](src/hal/post.rs)。

---

## 🖥️ OLED 屏布局（128×64，FONT_6X10）

```text
行 0 (y=0)  : Ctl  B● N● H●      >#03  （设备名 + BLE/NOW/心跳 状态灯 + 目标指示器）
行 1 (y=10) : ─────────────────────────
行 2 (y=20) : Joy X=+1000  Y=-1000
行 3 (y=30) : Kn  K1=1000 K2=1000
行 4 (y=40) : Btn [1][2][3][4][J]   （按下按钮反色方块）
行 5 (y=50) : Seq 0x0000ABCD  30Hz
```

顶部右侧目标指示器 `>#XX`（单选）/ `>ALL`（广播）/ `>---`（空选）随接收方选择变化；
电量图标已移除（本硬件经 LDO 稳压供电，ESP32 无电池原始电压采样通路，详见 `hal::battery`）。
低电量告警边框（B 选项）在无真实电量测量时不会触发，`ShowToast` 命令仍会在底部两行弹出反色提示条。

---

## ⚙️ 配置开关（编译期常量 / Feature）

代码注释中频繁出现带字母的"选项"代号，便于快速定位功能。汇总如下：

| 代号 | 选项 | 位置 | 说明 |
|------|------|------|------|
| **K** | HMAC 鉴权 | `protocol::config::auth` | Command/Response 帧 4B HMAC-SHA256 截断 |
| **K2** | 抗重放窗口 | `protocol::replay` + `transport/control` | per-key-id 64-bit 滑动窗 |
| **K3** | Session Nonce | `src/bin/main.rs` + `nonce_broadcast_task` | 上电 HW RNG 生成，每 5s 广播 |
| **K4** | 持久化加载 | `transport/control` + `hal/persist` | 启动时回填灵敏度 / 电量模式 / replay 窗 |
| **P** | NVS 落盘 | `config::persist::USE_NVS_STORAGE` | `false`=内存（默认），`true`=flash 双缓冲 |
| **O** | 密钥轮换 | `config::keyring::SHARED_SECRETS` | 4 个并存 key_id，平滑切换 |
| **U** | replay 窗持久化 | `hal/persist::PersistentConfig` | 重启保留抗重放窗口 |
| **L** | 低电量告警 | `ui/layout.rs::draw_low_battery_border` | 屏幕闪烁边框 |
| **H** | 自动回执 | `transport/control::broadcast_response` | 命令执行后广播 Ack |
| **Z** | 属性测试 | `crates/protocol/tests/` | proptest 往返测试（host 运行） |

> ⚠️ **生产部署注意**：`protocol` 默认启用 `embed-default-secrets`（弱 fallback 密钥），
> **生产 build 应关闭该 feature 并通过环境变量注入独立高熵密钥**；`debug-auth-bypass`
> 仅供本地构造报文，严禁用于生产（CI 应 assert 关闭）。

```bash
# 生产构建：关闭弱密钥回退，强制从环境变量注入密钥（缺失即编译失败）
CONTROLLER_SECRET_V1="$(head -c32 /dev/urandom | base64)" \
CONTROLLER_SECRET_V2="$(head -c32 /dev/urandom | base64)" \
cargo build -p protocol --no-default-features --features defmt
```

---

## 🔗 相关文档

- [根 README](../../README.md) — workspace 总览 / `cargo make` 构建入口
- [`docs/protocol_air.md`](../../docs/protocol_air.md) — 空中帧字节布局 / 安全模型
- [`protocol`](../protocol/README.md) — 协议 crate 设计与 API
- [`controller-dashboard`](../dashboard/README.md) — WebBluetooth 调试面板
