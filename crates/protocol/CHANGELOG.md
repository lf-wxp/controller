# Changelog

本文件记录 `controller-protocol` crate 的版本演进。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)。

> 提示：在 0.x 阶段，MINOR 号的跳动即视为 **可能包含破坏性变更**（SemVer 0.x 语义）。请查看具体条目的 `Breaking` 标记。

---

## [0.2.0] — 2026-07-11 · **Breaking**

本次发布是从 0.1 之后的**首个协议不兼容升级**。三种帧的长度、版本号、payload 布局全部变更；0.1 手柄与 0.2 Host 之间**无法互通**，双端需同步升级。

### Breaking Changes

- **协议版本号跳变**：
  - `Frame.PROTOCOL_VERSION`：`1 → 2`
  - `Command.COMMAND_VERSION`：`4 → 5`
  - `CommandResponse.RESPONSE_VERSION`：`4 → 5`
  - 旧版本帧会被新版本 decoder 明确拒绝（不做兼容处理）。
- **帧长扩展**：
  - `Frame`：`21 B → 25 B`（新增 4 字节 `dest_mask`）
  - `Command`：`20 B → 24 B`（`payload` 由 6 B 扩展到 10 B）
  - `CommandResponse`：`20 B → 24 B`（`payload` 由 6 B 扩展到 10 B）
- **`Frame` 新增寻址字段** `dest_mask: u32`：
  - 32 位位图，`bit-i == 1` ⇒ `receiver_id == i` 的接收方应处理该帧。
  - 新增常量 [`BROADCAST_DEST_MASK`]`= u32::MAX` 表示全体接收。
  - 新增构造器 `Frame::with_dest(seq, state, dest_mask)`；`Frame::new` 仍保持"默认广播"语义。
  - 新增判定方法 `Frame::is_addressed_to(receiver_id)`。
- **`CommandKind` 新增变体**：
  - `Announce` —— Controller 广播 peer 发现请求（payload 全 0）。
  - `AssignId { mac, receiver_id }` —— Controller 向指定 MAC 的 receiver 下发 `receiver_id` 分配。
- **`ResponseKind` 新增变体**：
  - `AnnounceReply` —— receiver 响应 controller 的 `Announce`，`payload = [mac[0..6], rssi_dbm: i8, role_tag[3]]`。
- **`NonceHello` payload 布局微调**：保留区从 `payload[4..6]` 扩展到 `payload[4..10]`（全 0 填充），语义不变，字节位移变化。

### Added

- **`src/peer_registry`**：controller 侧的 peer 注册表（存储 receiver 的 mac / rssi / role / receiver_id，供 UI selector 展示与目标寻址）。
- **`src/ui/selector`**：OLED 上的目标接收方选择器（长按 switch 进入选择模式，摇杆上下选择，btn1 确认 / btn2 取消）。
- **`docs/protocol_air.md`** / **`docs/esp_now_controller.md`** / **`docs/esp_now_receiver.md`**：同步更新到 v0.2 的帧布局与 peer discovery 流程说明。
- **属性测试**：`crates/protocol/tests/proptest_roundtrips.rs` 扩展覆盖 `dest_mask` 位图寻址、`Announce`、`AssignId`、`AnnounceReply` 的编解码往返。

### Changed

- **CRC/HMAC 覆盖范围随帧长扩展相应移动**：
  - `Command` HMAC 覆盖 `bytes[0..18]`（原 `bytes[0..14]`），CRC 覆盖 `bytes[0..22]`（原 `bytes[0..18]`）。
  - `CommandResponse` 同上。
- 顶层模块文档 [`lib.rs`] 的"三种协议帧"总表已同步至新长度与版本。
- `crates/protocol/README.md` / `USAGE.md` / `examples/receiver-import-example.md` 中的 tag 引用已更新为 `protocol-v0.2.0`。

### Migration Guide（0.1.x → 0.2.0）

1. **Cargo 依赖**：把下游项目里的
   ```toml
   controller-protocol = { git = "…", tag = "protocol-v0.1.0", … }
   ```
   改为
   ```toml
   controller-protocol = { git = "…", tag = "protocol-v0.2.0", … }
   ```
   并执行 `cargo update -p controller-protocol`。
2. **传输层缓冲区**：把接收/发送缓冲区从 `[u8; 21]` / `[u8; 20]` 分别扩到 `[u8; 25]` / `[u8; 24]`（或直接引用常量 `FRAME_LEN` / `COMMAND_LEN` / `RESPONSE_LEN`）。
3. **`Frame` 构造**：
   - 只要广播 → 继续调用 `Frame::new(seq, state)`（行为等价）。
   - 需要单播/子集 → 改用 `Frame::with_dest(seq, state, mask)`；`mask` 可通过 `1u32 << receiver_id` 组合。
4. **`CommandKind` / `ResponseKind` 匹配**：若下游有 `match` 未开启 `#[non_exhaustive]`，需补齐 `Announce` / `AssignId` / `AnnounceReply` 三个新分支（否则将编译失败）。
5. **双端同步升级**：手柄固件与 host / receiver 三方**必须同版本发布**，不同版本互发的帧会被 magic + version_byte 校验直接拒绝。

---

## [0.1.0] — 2026-07-11

首次发布，作为独立 crate 抽离自手柄固件。

### Added

- `state::GamepadState`：摇杆/按钮/旋钮的完整快照结构。
- `frame::Frame`（21 B, magic `0xC71E`, version 1）：手柄 → Host 的广播帧。
- `command::Command`（20 B, magic `0xCB01`, version 4）：Host → 手柄的反向命令，HMAC-SHA256 签名 + seq 抗重放 + 4-bit key_id 密钥轮换。
- `response::CommandResponse`（20 B, magic `0xCB02`, version 4）：手柄 → Host 的命令回执，含 `NonceHello` session nonce 广播。
- `replay::AntiReplayWindow`：64-bit 滑动窗口抗重放。
- `auth`：HMAC-SHA256 计算 + session nonce 管理。
- `crc::crc16_ibm`：CRC-16-IBM 校验实现。
- Cargo features：`std` / `defmt` / `serde` / `debug-auth-bypass` / `embed-default-secrets`（默认启用后者，供开发场景开箱即用）。
- 发布元数据：`repository` / `homepage` / `documentation` / `include` 白名单。
- 使用指南 `USAGE.md` 与 receiver 侧最小引用示例 `examples/receiver-import-example.md`。

[0.2.0]: https://github.com/lf-wxp/controller/releases/tag/protocol-v0.2.0
[0.1.0]: https://github.com/lf-wxp/controller/releases/tag/protocol-v0.1.0
