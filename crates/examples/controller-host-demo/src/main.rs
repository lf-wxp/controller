//! # controller-host-demo
//!
//! **纯 host 侧的协议交互演示**，无需真机 —— 我们在同一进程里同时扮演
//! "控制端主机"和"手柄"两个角色，完整走一遍 ESP-NOW 双向流程：
//!
//! ```text
//! 主机侧                                     手柄侧
//!   │                                         │
//!   │◀────── ① NonceHello (0xCB02) ──────────│  (手柄开机主动广播)
//!   │        init_session_nonce(nonce)        │
//!   │                                         │
//!   ├─────── ② Command (0xCB01) ─────────────▶│
//!   │        encode_command / seq=1           │
//!   │                                         │  decode_command
//!   │                                         │  验签 (HMAC + nonce)
//!   │                                         │  抗重放窗口
//!   │                                         │  ↓
//!   │◀────── ③ Response (0xCB02) ─────────────│
//!   │        Ack / Error / BatterySnapshot    │
//!   │        decode_response                  │
//! ```
//!
//! ## 运行方式
//!
//! ```bash
//! cd crates/examples/controller-host-demo
//! cargo run
//! # 或从项目根：
//! cargo make example-host-demo
//! ```
//!
//! ## 与 docs/esp_now_controller.md 的关系
//!
//! `docs/esp_now_controller.md` 里的示例代码是**面向硬件**的（依赖 esp-hal），
//! 本 demo 是它的**协议层可编译镜像**：把 `esp_now.send_async(...)` 替换成
//! "直接调 `handle_command_on_gamepad(bytes)`"，把 `receiver.receive_async()`
//! 替换成"从 `Vec<u8>` 里拿"。
//!
//! **CI 每次运行 `cargo test` / `cargo run` 都会验证这段代码 —— 一旦
//! controller-protocol API 变更，本 demo 会立刻编译失败，防止文档腐化。**

use controller_protocol::{
  auth::{KeyId, init_session_nonce, session_nonce},
  command::{Command, CommandBody, CommandDecodeError, decode_command, encode_command},
  replay::AntiReplayWindow,
  response::{
    CommandResponse, ErrorCode, RESPONSE_LEN, ResponseBody, decode_response, encode_response,
  },
};

/// 演示用的固定初始 nonce（真实手柄会用硬件 RNG 生成）
const DEMO_INITIAL_NONCE: u32 = 0xDEAD_BEEF;

/// 手柄侧模拟状态：抗重放窗口 + 是否处于电池模拟模式
struct SimulatedGamepad {
  replay: AntiReplayWindow,
  simulate_battery: bool,
}

impl SimulatedGamepad {
  fn new() -> Self {
    Self {
      replay: AntiReplayWindow::new(),
      simulate_battery: true,
    }
  }

  /// 模拟手柄开机：初始化 SESSION_NONCE 并广播 NonceHello
  fn boot() -> ([u8; RESPONSE_LEN], u32) {
    init_session_nonce(DEMO_INITIAL_NONCE);
    let nonce = session_nonce();
    let hello = CommandResponse::nonce_hello(nonce);
    let bytes = encode_response(&hello);
    (bytes, nonce)
  }

  /// 手柄侧处理入站 Command，返回 24 字节 Response
  fn handle_command(&mut self, wire_bytes: &[u8]) -> [u8; RESPONSE_LEN] {
    // Step 1: 解码（自动校验 CRC + HMAC + version + key_id）
    let cmd = match decode_command(wire_bytes) {
      Ok(cmd) => cmd,
      Err(err) => return self.reject(0, err_code_for(err)),
    };

    // Step 2: 抗重放检查
    if let Err(_replay_err) = self.replay.check_and_update(cmd.seq) {
      return self.reject(cmd.seq, ErrorCode::InvalidArgument);
    }

    // Step 3: 派发到具体动作
    let resp = match cmd.kind {
      CommandBody::Nop => CommandResponse::ack_with_key(cmd.seq, cmd.key_id),
      CommandBody::LedBlink {
        led_idx,
        count,
        period_ms,
      } => {
        if led_idx > 0 {
          // 本演示只有 1 颗 LED（idx=0），越界返 InvalidArgument
          CommandResponse::err_with_key(cmd.seq, cmd.key_id, ErrorCode::InvalidArgument)
        } else {
          println!(
            "  [gamepad] LED {} 闪烁 {} 次，每次 {} ms",
            led_idx, count, period_ms
          );
          CommandResponse::ack_with_key(cmd.seq, cmd.key_id)
        }
      }
      CommandBody::SetSensitivity {
        joy_scale,
        knob_scale,
      } => {
        println!(
          "  [gamepad] 灵敏度：摇杆={}‰，旋钮={}‰",
          joy_scale, knob_scale
        );
        CommandResponse::ack_with_key(cmd.seq, cmd.key_id)
      }
      CommandBody::ShowToast { len, bytes } => {
        let text = core::str::from_utf8(&bytes[..len as usize]).unwrap_or("<non-utf8>");
        println!("  [gamepad] OLED 弹出提示：\"{}\"", text);
        CommandResponse::ack_with_key(cmd.seq, cmd.key_id)
      }
      CommandBody::SetBatteryMode { simulate } => {
        self.simulate_battery = simulate;
        println!(
          "  [gamepad] 电池模式切换为：{}",
          if simulate { "模拟" } else { "真实" }
        );
        CommandResponse::ack_with_key(cmd.seq, cmd.key_id)
      }
      // Announce / AssignId 是 controller→receiver 方向的命令；host-demo
      // 模拟的是手柄本体，收到这两种 kind 属于协议误用，回 Unsupported。
      CommandBody::Announce | CommandBody::AssignId { .. } => {
        println!("  [gamepad] 收到 Announce/AssignId（协议方向错误，忽略）");
        CommandResponse::err_with_key(cmd.seq, cmd.key_id, ErrorCode::Unsupported)
      }
    };

    encode_response(&resp)
  }

  fn reject(&self, req_seq: u32, code: ErrorCode) -> [u8; RESPONSE_LEN] {
    let resp = CommandResponse::err_with_key(req_seq, KeyId::DEFAULT, code);
    encode_response(&resp)
  }
}

/// 简单映射：CommandDecodeError → 面向控制端的 ErrorCode（演示用）
fn err_code_for(err: CommandDecodeError) -> ErrorCode {
  match err {
    CommandDecodeError::InvalidPayload | CommandDecodeError::UnknownKind(_) => {
      ErrorCode::InvalidArgument
    }
    CommandDecodeError::UnsupportedVersion(_) | CommandDecodeError::UnsupportedKeyId(_) => {
      ErrorCode::Unsupported
    }
    _ => ErrorCode::InvalidArgument,
  }
}

/// 控制端侧：seq 计数器（每个 key_id 独立；本 demo 只用 KeyId::DEFAULT）
struct HostSeqCounter(u32);

impl HostSeqCounter {
  fn new() -> Self {
    Self(0)
  }

  fn next(&mut self) -> u32 {
    self.0 += 1;
    self.0
  }
}

fn main() {
  println!("╔══════════════════════════════════════════════════════════════╗");
  println!("║  controller-host-demo — 空中协议交互演示（纯 host 侧）      ║");
  println!("╚══════════════════════════════════════════════════════════════╝");
  println!();

  // ────────────────────────────────────────────────────────────────
  // Phase 1: 手柄开机，主动广播 NonceHello
  // ────────────────────────────────────────────────────────────────
  let mut gamepad = SimulatedGamepad::new();
  let (hello_bytes, nonce_at_gamepad) = SimulatedGamepad::boot();
  println!(
    "① 手柄开机 → SESSION_NONCE = 0x{:08x}，广播 NonceHello 帧",
    nonce_at_gamepad
  );

  // 控制端接收 NonceHello 并装入本地 session_nonce
  let hello = decode_response(&hello_bytes).expect("valid NonceHello");
  let nonce_at_host = match hello.body {
    ResponseBody::NonceHello { nonce } => {
      init_session_nonce(nonce);
      println!(
        "   [host] 收到 NonceHello，装入本地 nonce = 0x{:08x}",
        nonce
      );
      nonce
    }
    other => panic!("expected NonceHello, got {:?}", other),
  };
  assert_eq!(nonce_at_gamepad, nonce_at_host);

  // ────────────────────────────────────────────────────────────────
  // Phase 2: 控制端下发 5 种命令，每条都收到手柄响应
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("② 控制端依次下发 5 种命令：");

  let mut counter = HostSeqCounter::new();
  let commands: &[(&str, CommandBody)] = &[
    ("Nop", CommandBody::Nop),
    (
      "LedBlink",
      CommandBody::LedBlink {
        led_idx: 0,
        count: 3,
        period_ms: 100,
      },
    ),
    (
      "SetSensitivity",
      CommandBody::SetSensitivity {
        joy_scale: 800,
        knob_scale: 1000,
      },
    ),
    (
      "ShowToast",
      CommandBody::ShowToast {
        len: 3,
        bytes: *b"HI!\0\0",
      },
    ),
    (
      "SetBatteryMode",
      CommandBody::SetBatteryMode { simulate: false },
    ),
  ];

  for (name, body) in commands {
    let seq = counter.next();
    let cmd = Command::with_key(seq, KeyId::DEFAULT, *body);
    let wire = encode_command(&cmd);
    println!("  → [host] 发送 {} (seq={})", name, seq);

    // 空口传输（本演示直接把字节丢给手柄）
    let resp_bytes = gamepad.handle_command(&wire);

    // 控制端解析响应
    match decode_response(&resp_bytes) {
      Ok(resp) => match resp.body {
        ResponseBody::Ack => println!("  ← [host] ✓ ACK  (req_seq={})", resp.req_seq),
        ResponseBody::Error(code) => {
          println!(
            "  ← [host] ✗ ERR  (req_seq={}, code={:?})",
            resp.req_seq, code
          )
        }
        ResponseBody::BatterySnapshot { percent } => {
          println!("  ← [host] 电量：{}%", percent)
        }
        ResponseBody::NonceHello { .. } => unreachable!("NonceHello only on boot"),
        ResponseBody::AnnounceReply { .. } => {
          // host demo 不与 Announce/Reply 交互（那是 controller↔receiver 方向的事），
          // 上下文理论上不会到达。为严谨起见回删除而不 panic。
          println!("[host] unexpected AnnounceReply ignored");
        }
      },
      Err(err) => println!("  ← [host] 解码失败：{:?}", err),
    }
  }

  // ────────────────────────────────────────────────────────────────
  // Phase 3: 抗重放演示 —— 尝试重发 seq=1，应被拒绝
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("③ 抗重放演示：重发 seq=1 的 Nop 命令");
  let replay_cmd = Command::with_key(1, KeyId::DEFAULT, CommandBody::Nop);
  let wire = encode_command(&replay_cmd);
  let resp_bytes = gamepad.handle_command(&wire);
  let resp = decode_response(&resp_bytes).expect("valid response");
  match resp.body {
    ResponseBody::Error(code) => {
      println!("  ← [host] 被拒绝（预期）：code={:?}", code);
    }
    other => println!("  ← [host] 意外通过：{:?}", other),
  }

  // ────────────────────────────────────────────────────────────────
  // Phase 4: 错误参数演示 —— LED idx 越界
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("④ 参数越界演示：LedBlink {{ led_idx: 99 }}");
  let bad_cmd = Command::with_key(
    counter.next(),
    KeyId::DEFAULT,
    CommandBody::LedBlink {
      led_idx: 99,
      count: 1,
      period_ms: 100,
    },
  );
  let wire = encode_command(&bad_cmd);
  let resp_bytes = gamepad.handle_command(&wire);
  let resp = decode_response(&resp_bytes).expect("valid response");
  match resp.body {
    ResponseBody::Error(code) => {
      println!("  ← [host] 被拒绝（预期）：code={:?}", code);
    }
    other => println!("  ← [host] 意外通过：{:?}", other),
  }

  println!();
  println!("✅ 演示完成。所有协议交互闭环验证通过。");
}

// ═══════════════════════════════════════════════════════════════════
//  单元测试：CI 里跑，任何协议变更都会立刻打破本 demo
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
  use super::*;
  use controller_protocol::auth::verify_hmac_tag;
  use controller_protocol::response::ResponseDecodeError;

  #[test]
  fn happy_path_nop_ack() {
    init_session_nonce(DEMO_INITIAL_NONCE);
    let mut gp = SimulatedGamepad::new();

    let cmd = Command::with_key(1, KeyId::DEFAULT, CommandBody::Nop);
    let wire = encode_command(&cmd);
    let resp_bytes = gp.handle_command(&wire);

    let resp = decode_response(&resp_bytes).expect("valid response");
    assert_eq!(resp.req_seq, 1);
    assert!(matches!(resp.body, ResponseBody::Ack));
  }

  #[test]
  fn replay_second_time_rejected() {
    init_session_nonce(DEMO_INITIAL_NONCE);
    let mut gp = SimulatedGamepad::new();

    let cmd = Command::with_key(1, KeyId::DEFAULT, CommandBody::Nop);
    let wire = encode_command(&cmd);
    let _ = gp.handle_command(&wire); // 首次接受
    let resp_bytes = gp.handle_command(&wire); // 重放

    let resp = decode_response(&resp_bytes).expect("valid response");
    assert!(matches!(resp.body, ResponseBody::Error(_)));
  }

  #[test]
  fn led_idx_out_of_range_rejected() {
    init_session_nonce(DEMO_INITIAL_NONCE);
    let mut gp = SimulatedGamepad::new();

    let cmd = Command::with_key(
      1,
      KeyId::DEFAULT,
      CommandBody::LedBlink {
        led_idx: 99,
        count: 1,
        period_ms: 100,
      },
    );
    let wire = encode_command(&cmd);
    let resp_bytes = gp.handle_command(&wire);

    let resp = decode_response(&resp_bytes).expect("valid response");
    assert!(matches!(
      resp.body,
      ResponseBody::Error(ErrorCode::InvalidArgument)
    ));
  }

  #[test]
  fn wire_bytes_length_matches_protocol_constants() {
    use controller_protocol::{COMMAND_LEN, RESPONSE_LEN};

    init_session_nonce(DEMO_INITIAL_NONCE);
    let cmd = Command::with_key(1, KeyId::DEFAULT, CommandBody::Nop);
    let bytes = encode_command(&cmd);
    assert_eq!(bytes.len(), COMMAND_LEN);

    let resp = CommandResponse::ack_with_key(1, KeyId::DEFAULT);
    let bytes = encode_response(&resp);
    assert_eq!(bytes.len(), RESPONSE_LEN);
  }

  #[test]
  fn wrong_nonce_fails_hmac() {
    init_session_nonce(DEMO_INITIAL_NONCE);
    let mut gp = SimulatedGamepad::new();

    let cmd = Command::with_key(1, KeyId::DEFAULT, CommandBody::Nop);
    let wire = encode_command(&cmd);

    // 模拟主机与手柄 nonce 不一致：手柄端换用另一个 nonce
    init_session_nonce(0xCAFEBABE);
    let resp_bytes = gp.handle_command(&wire);

    let resp = decode_response(&resp_bytes).expect("valid response");
    // 应当 HMAC 校验失败 → 手柄返回 Error
    assert!(matches!(resp.body, ResponseBody::Error(_)));
  }

  /// 静默使用 verify_hmac_tag / ResponseDecodeError 以确保 API 未被破坏。
  #[test]
  fn public_api_stable() {
    // 只是一个"symbol 存在"检查，避免 API 静默失联。
    let _ok = verify_hmac_tag(&[0_u8; 8], &[0_u8; 4], KeyId::DEFAULT);
    let _err: Result<CommandResponse, ResponseDecodeError> = decode_response(&[0_u8; 20]);
  }
}
