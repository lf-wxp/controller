# controller-dashboard

**ESP32 Controller 的 WebBluetooth 调试面板**——Leptos 0.8 + Rust WASM，
与手柄固件**共享同一份** `controller-protocol` crate，协议漂移风险 = 0。

## 特性

| 功能 | 说明 |
|------|------|
| 🔌 **连接 / 断开** | 一键 `navigator.bluetooth.requestDevice` 弹出选择器 |
| 🎯 **实时可视化** | SVG 摇杆圆 + 5 个按钮亮灯 + 双旋钮进度条 |
| 📤 **命令发送** | 支持全部 5 种 CommandBody：Nop / LedBlink / SetSensitivity / ShowToast / SetBatteryMode |
| 📥 **响应解析** | 自动解析 Ack / Error / BatterySnapshot / NonceHello，同步到 UI |
| 🔑 **Key ID 切换** | 下拉框切换密钥槽（0 / 1），验证 O 选项密钥轮换 |
| 📋 **时序日志** | 环形缓冲 200 条，方向徽章（RX/TX/IN/WN），可展开 hex dump |

## 前置依赖

- Rust **stable** channel（此 crate 自带 `rust-toolchain.toml`）
- WASM target：`rustup target add wasm32-unknown-unknown`
- **Trunk**：Rust WASM 打包工具，[官方文档](https://trunkrs.dev/)

```bash
# 一次性安装
rustup target add wasm32-unknown-unknown
cargo install trunk
```

## 运行

```bash
cd crates/dashboard
trunk serve --open
# 浏览器自动打开 http://127.0.0.1:8080
```

首次运行会：
1. 编译 `controller-protocol` + `controller-dashboard` 到 WASM
2. wasm-opt 优化二进制
3. Trunk 启动 HTTP 服务并注入 auto-reload 脚本

## 生产构建

```bash
trunk build --release
# 产物在 crates/dashboard/dist/
```

生产版通过 `--config profile=wasm-release` 使用极致体积优化（LTO + opt-level=z）。

## 浏览器要求

- **必须**：Chrome / Edge 最新版（支持 WebBluetooth）
- **必须**：`localhost` 或 `https://`（WebBluetooth 安全上下文要求）
- **不支持**：Firefox / Safari（默认不启用 WebBluetooth）

## 架构对照

```text
┌─────────────────────────────────────────────────────────────┐
│  ESP32 手柄 (Rust, no_std, xtensa target)                    │
│    └─ 使用 controller-protocol crate 编码 Frame/Response     │
│                        ▲                                     │
│                        │ BLE GATT                            │
│                        ▼                                     │
│  Chrome 浏览器 (WebBluetooth)                                │
│    └─ Leptos WASM app                                        │
│         └─ 使用同一份 controller-protocol crate              │
└─────────────────────────────────────────────────────────────┘
```

**核心价值**：手柄和 Dashboard **共享 encode/decode 实现**。修改协议只需改
`crates/protocol/`，两端一同编译验证，避免"手柄改了 wire 格式，Host 端忘同步"的常见 bug。

## 文件布局

```
crates/dashboard/
├── Cargo.toml            # 依赖：leptos + wasm-bindgen + controller-protocol
├── Trunk.toml            # 打包配置（端口 / dist / wasm-opt）
├── rust-toolchain.toml   # 覆盖顶层 esp channel，用 stable
├── .cargo/config.toml    # 覆盖顶层 xtensa target，改 wasm32-unknown-unknown
├── index.html            # Trunk 入口
├── style.css             # 全局样式（暗色主题）
└── src/
    ├── main.rs           # 入口：panic hook + console_log + mount App
    ├── app.rs            # 顶层布局
    ├── state.rs          # AppState (RwSignal-based) + EventEntry
    ├── bluetooth.rs      # WebBluetooth 封装（手工 wasm_bindgen extern）
    └── components/
        ├── mod.rs
        ├── status_panel.rs   # 顶栏（连接/电量/nonce/kid）
        ├── gamepad_visual.rs # 摇杆/按钮/旋钮可视化
        ├── command_panel.rs  # 5 种命令发送表单
        └── event_log.rs      # 时序日志 + hex dump
```
