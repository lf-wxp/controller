//! # controller-receiver-demo
//!
//! **纯 host 侧的状态帧订阅演示**，无需真机 —— 我们在同一进程里同时扮演
//! "发送手柄"和"接收端"两个角色，完整走一遍 ESP-NOW 广播接收流程：
//!
//! ```text
//! 手柄侧                                     接收端
//!   │                                         │
//!   │─── ① Frame (0xC71E, 25B) ──────────────▶│
//!   │    encode_frame(seq=1)                  │
//!   │                                         │  decode_frame
//!   │                                         │  ├─ 校验 magic
//!   │                                         │  ├─ 校验 version
//!   │                                         │  ├─ 校验 CRC-16
//!   │                                         │  └─ 拆 payload
//!   │─── ② Frame (seq=2) ────────────────────▶│  seq gap 检测
//!   │─── ③ Frame (seq=4) ────────────────────▶│  ⚠️ 丢了 seq=3
//! ```
//!
//! ## 运行方式
//!
//! ```bash
//! cd crates/examples/controller-receiver-demo
//! cargo run
//! # 或从项目根：
//! cargo make example-receiver-demo
//! ```
//!
//! ## 与 docs/esp_now_receiver.md 的关系
//!
//! `docs/esp_now_receiver.md` 里的示例代码是**面向硬件**的（依赖 esp-hal），
//! 本 demo 是它的**协议层可编译镜像**：把 `receiver.receive_async()` 替换成
//! "直接从 `Vec<[u8; 21]>` 里迭代"，其它逻辑完全一致。
//!
//! **CI 每次运行 `cargo test` / `cargo run` 都会验证这段代码 —— 一旦
//! protocol API 变更（Frame 字段增减、ButtonBits 变体调整等），
//! 本 demo 会立刻编译失败，防止 receiver.md 里的示例代码腐化。**

use protocol::{
  ButtonBits, DecodeError, FRAME_LEN, Frame, GamepadState, decode_frame, encode_frame,
};

// ============================================================
// 模拟一段"手柄使用会话"
// ============================================================

/// 生成一批模拟手柄状态帧
///
/// 返回 `Vec<(seq, state)>`，接收端将逐帧编码 → 传输 → 解码。
/// 模拟真实使用场景：先按下按钮，再拨摇杆，最后转旋钮。
fn simulate_gamepad_session() -> Vec<(u32, GamepadState)> {
  let mut session = Vec::new();

  // t=0: 空闲
  session.push((1, GamepadState::EMPTY));

  // t=1: 按下 Btn1
  let mut s = GamepadState::EMPTY;
  s.set_button(ButtonBits::Btn1, true);
  session.push((2, s));

  // t=2: 按下 Btn1 + Btn3
  let mut s = GamepadState::EMPTY;
  s.set_button(ButtonBits::Btn1, true);
  s.set_button(ButtonBits::Btn3, true);
  session.push((3, s));

  // t=3: 摇杆向右上
  session.push((
    4,
    GamepadState {
      buttons: 0,
      joy_x: 20_000,
      joy_y: 15_000,
      knob_1: 0,
      knob_2: 0,
      _reserved: 0,
    },
  ));

  // t=4: 旋钮转到中位
  session.push((
    5,
    GamepadState {
      buttons: 0,
      joy_x: 0,
      joy_y: 0,
      knob_1: 32_768,
      knob_2: 16_384,
      _reserved: 0,
    },
  ));

  // t=5: 全按下所有按钮
  let mut s = GamepadState::EMPTY;
  s.set_button(ButtonBits::Btn1, true);
  s.set_button(ButtonBits::Btn2, true);
  s.set_button(ButtonBits::Btn3, true);
  s.set_button(ButtonBits::Btn4, true);
  session.push((6, s));

  session
}

// ============================================================
// 接收端逻辑
// ============================================================

/// 把 GamepadState 格式化成一行人类可读的描述
fn describe_state(state: &GamepadState) -> String {
  let mut buttons = Vec::new();
  for (bit, name) in [
    (ButtonBits::Btn1, "Btn1"),
    (ButtonBits::Btn2, "Btn2"),
    (ButtonBits::Btn3, "Btn3"),
    (ButtonBits::Btn4, "Btn4"),
  ] {
    if state.is_pressed(bit) {
      buttons.push(name);
    }
  }
  let btns = if buttons.is_empty() {
    "─".to_string()
  } else {
    buttons.join("+")
  };

  format!(
    "btn={:<12} joy=({:>+6},{:>+6}) knob=({:>5},{:>5})",
    btns, state.joy_x, state.joy_y, state.knob_1, state.knob_2
  )
}

/// 接收端处理一批"空口字节"，返回 (成功解码数, seq gap 数)
fn process_wire_batch(wire_frames: &[[u8; FRAME_LEN]]) -> (usize, usize) {
  let mut ok_count = 0;
  let mut gap_count = 0;
  let mut last_seq: Option<u32> = None;

  for bytes in wire_frames {
    match decode_frame(bytes) {
      Ok(frame) => {
        // seq gap 检测
        if let Some(prev) = last_seq {
          let expected = prev.wrapping_add(1);
          if frame.header.seq != expected {
            let missing = frame.header.seq.wrapping_sub(expected);
            println!(
              "  ⚠️  seq gap detected: expected={}, got={} (missing {} frame{})",
              expected,
              frame.header.seq,
              missing,
              if missing == 1 { "" } else { "s" }
            );
            gap_count += 1;
          }
        }
        last_seq = Some(frame.header.seq);

        println!(
          "  ← seq={:<4} {}",
          frame.header.seq,
          describe_state(&frame.payload)
        );
        ok_count += 1;
      }
      Err(err) => {
        println!("  ✗ decode failed: {:?}", err);
      }
    }
  }

  (ok_count, gap_count)
}

// ============================================================
// 主入口
// ============================================================

fn main() {
  println!("╔══════════════════════════════════════════════════════════════╗");
  println!("║  controller-receiver-demo — 状态帧订阅演示（纯 host 侧）    ║");
  println!("╚══════════════════════════════════════════════════════════════╝");
  println!();

  // ────────────────────────────────────────────────────────────────
  // Phase 1: 正常场景 —— 6 帧连续 seq
  // ────────────────────────────────────────────────────────────────
  println!("① 手柄发送 6 帧（seq=1..=6），接收端逐帧解码：");
  let session = simulate_gamepad_session();

  let wire: Vec<[u8; FRAME_LEN]> = session
    .iter()
    .map(|(seq, state)| encode_frame(&Frame::new(*seq, *state)))
    .collect();

  let (ok, gaps) = process_wire_batch(&wire);
  println!("   总计 {} 帧解码成功，{} 处 seq gap", ok, gaps);
  assert_eq!(ok, 6);
  assert_eq!(gaps, 0);

  // ────────────────────────────────────────────────────────────────
  // Phase 2: 丢包场景 —— 模拟 seq=3 的帧丢失
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("② 模拟丢包：故意跳过 seq=3 的帧");
  let mut lossy_wire = wire.clone();
  lossy_wire.remove(2); // 移除 seq=3 那一帧
  let (ok, gaps) = process_wire_batch(&lossy_wire);
  println!("   总计 {} 帧解码成功，{} 处 seq gap", ok, gaps);
  assert_eq!(ok, 5);
  assert_eq!(gaps, 1);

  // ────────────────────────────────────────────────────────────────
  // Phase 3: 错误处理演示 —— 5 种 DecodeError
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("③ 错误处理演示：");
  demo_decode_errors();

  // ────────────────────────────────────────────────────────────────
  // Phase 4: 干扰帧演示 —— 其它 magic 被静默忽略
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("④ 干扰帧演示：Command 帧 (0xCB01) 在同一广播频道上，接收端应忽略");
  // 构造一个假的 Command 帧（COMMAND_LEN 字节）—— magic = 0xCB01
  let mut fake_command = [0_u8; FRAME_LEN];
  fake_command[0] = 0x01;
  fake_command[1] = 0xCB;
  match decode_frame(&fake_command) {
    Err(DecodeError::BadMagic) => {
      println!("   ✓ 正确识别为 BadMagic，接收端可静默 continue");
    }
    other => println!("   意外结果：{:?}", other),
  }

  // ────────────────────────────────────────────────────────────────
  // Phase 5: Frame `dest_mask` 过滤演示
  // ────────────────────────────────────────────────────────────────
  println!();
  println!("⑤ dest_mask 过滤演示（Frame）：");
  demo_dest_mask_filtering();

  println!();
  println!("✅ 演示完成。Frame 编解码 + seq gap 检测 + 错误处理 + dest_mask 过滤全部验证通过。");
}

/// 模拟本机 receiver 的逻辑 ID（由 AssignId 命令在真机上下发）
const DEMO_RECEIVER_ID: u8 = 3;

/// 演示 `dest_mask` 位图过滤：模拟一个 `receiver_id = 3` 的接收方，
/// 依次接收 3 帧：广播帧、寻址到自己的帧、寻址到别人的帧，
/// 只有前两帧会被处理。
fn demo_dest_mask_filtering() {
  // 1) 广播帧（默认 dest_mask = 0xFFFF_FFFF）
  let broadcast = Frame::new(10, GamepadState::EMPTY);
  print_dest_mask_result("广播帧", &broadcast);

  // 2) 单播到本机 (bit-3)
  let mine_mask = 1_u32 << DEMO_RECEIVER_ID;
  let mine = Frame::with_dest(11, GamepadState::EMPTY, mine_mask);
  print_dest_mask_result("单播到本机", &mine);

  // 3) 单播到别的 receiver (bit-7)
  let others_mask = 1_u32 << 7;
  let others = Frame::with_dest(12, GamepadState::EMPTY, others_mask);
  print_dest_mask_result("单播到 receiver_id=7", &others);
}

/// 辅助函数：编码 → 解码 → 判断 `is_addressed_to(DEMO_RECEIVER_ID)`
fn print_dest_mask_result(label: &str, frame: &Frame) {
  let wire = encode_frame(frame);
  let decoded = decode_frame(&wire).expect("decode ok");
  let addressed = decoded.is_addressed_to(DEMO_RECEIVER_ID);
  let outcome = if addressed {
    "✓ 处理"
  } else {
    "✗ 丢弃"
  };
  println!(
    "   {} seq={} dest_mask=0x{:08x} → receiver_id={} {}",
    outcome, decoded.header.seq, decoded.dest_mask, DEMO_RECEIVER_ID, label
  );
}

/// 演示 [`DecodeError`] 的 4 种失败场景（编译期覆盖所有变体）
fn demo_decode_errors() {
  // 1. BadLength：长度不对
  let short = [0_u8; 10];
  match decode_frame(&short) {
    Err(DecodeError::BadLength) => println!("   ✓ BadLength（长度 10 ≠ {FRAME_LEN}）"),
    other => println!("   意外结果：{:?}", other),
  }

  // 2. BadMagic：魔数不对
  let mut bad_magic = encode_frame(&Frame::new(1, GamepadState::EMPTY));
  bad_magic[0] ^= 0xFF;
  match decode_frame(&bad_magic) {
    Err(DecodeError::BadMagic) => println!("   ✓ BadMagic（magic 字节被翻转）"),
    other => println!("   意外结果：{:?}", other),
  }

  // 3. UnsupportedVersion：版本不支持
  let mut bad_ver = encode_frame(&Frame::new(1, GamepadState::EMPTY));
  bad_ver[2] = 99;
  match decode_frame(&bad_ver) {
    Err(DecodeError::UnsupportedVersion(v)) => {
      println!("   ✓ UnsupportedVersion({})（version 字节被改成 99）", v);
    }
    other => println!("   意外结果：{:?}", other),
  }

  // 4. BadCrc：payload 被篡改但 CRC 未同步
  let mut bad_crc = encode_frame(&Frame::new(1, GamepadState::EMPTY));
  bad_crc[10] ^= 0xFF; // 翻转 payload 中间一字节
  match decode_frame(&bad_crc) {
    Err(DecodeError::BadCrc { expected, actual }) => println!(
      "   ✓ BadCrc(expected=0x{:04x}, actual=0x{:04x})（payload 被篡改）",
      expected, actual
    ),
    other => println!("   意外结果：{:?}", other),
  }
}

// ═══════════════════════════════════════════════════════════════════
//  单元测试：CI 里跑，任何 Frame API 变更都会立刻打破本 demo
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrip_empty_frame() {
    let frame = Frame::new(42, GamepadState::EMPTY);
    let wire = encode_frame(&frame);
    let decoded = decode_frame(&wire).expect("decode ok");
    assert_eq!(frame, decoded);
  }

  #[test]
  fn roundtrip_full_gamepad_state() {
    let mut state = GamepadState {
      buttons: 0,
      joy_x: -12_345,
      joy_y: 32_100,
      knob_1: 60_000,
      knob_2: 1,
      _reserved: 0,
    };
    state.set_button(ButtonBits::Btn2, true);
    state.set_button(ButtonBits::Btn4, true);

    let frame = Frame::new(0xDEAD_BEEF, state);
    let wire = encode_frame(&frame);
    let decoded = decode_frame(&wire).expect("decode ok");
    assert_eq!(decoded.header.seq, 0xDEAD_BEEF);
    assert_eq!(decoded.payload, state);
    assert!(decoded.payload.is_pressed(ButtonBits::Btn2));
    assert!(decoded.payload.is_pressed(ButtonBits::Btn4));
    assert!(!decoded.payload.is_pressed(ButtonBits::Btn1));
  }

  #[test]
  fn frame_wire_length_matches_protocol_constant() {
    let wire = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    assert_eq!(wire.len(), FRAME_LEN);
  }

  #[test]
  fn bad_length_detected() {
    assert_eq!(decode_frame(&[0_u8; 10]), Err(DecodeError::BadLength));
  }

  #[test]
  fn bad_magic_detected() {
    let mut wire = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    wire[0] ^= 0xFF;
    assert_eq!(decode_frame(&wire), Err(DecodeError::BadMagic));
  }

  #[test]
  fn corrupted_payload_detected_by_crc() {
    let mut wire = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    wire[10] ^= 0xFF;
    assert!(matches!(
      decode_frame(&wire),
      Err(DecodeError::BadCrc { .. })
    ));
  }

  #[test]
  fn session_replay_zero_gap() {
    let session = simulate_gamepad_session();
    let wire: Vec<[u8; FRAME_LEN]> = session
      .iter()
      .map(|(seq, state)| encode_frame(&Frame::new(*seq, *state)))
      .collect();

    let (ok, gaps) = process_wire_batch(&wire);
    assert_eq!(ok, 6);
    assert_eq!(gaps, 0);
  }

  #[test]
  fn session_replay_detects_missing_frame() {
    let session = simulate_gamepad_session();
    let mut wire: Vec<[u8; FRAME_LEN]> = session
      .iter()
      .map(|(seq, state)| encode_frame(&Frame::new(*seq, *state)))
      .collect();
    wire.remove(2); // drop seq=3

    let (ok, gaps) = process_wire_batch(&wire);
    assert_eq!(ok, 5);
    assert_eq!(gaps, 1);
  }

  #[test]
  fn describe_state_lists_pressed_buttons() {
    let mut state = GamepadState::EMPTY;
    state.set_button(ButtonBits::Btn1, true);
    state.set_button(ButtonBits::Btn3, true);
    let text = describe_state(&state);
    assert!(text.contains("Btn1+Btn3"));
  }

  #[test]
  fn broadcast_frame_is_addressed_to_all_receivers() {
    // 默认构造的 Frame 应等价于广播
    let frame = Frame::new(1, GamepadState::EMPTY);
    for rid in 0..32_u8 {
      assert!(
        frame.is_addressed_to(rid),
        "broadcast frame must address receiver_id={rid}",
      );
    }
  }

  #[test]
  fn unicast_frame_addresses_only_target_receiver() {
    // dest_mask = 1<<3 应只寻址到 receiver_id=3
    let frame = Frame::with_dest(1, GamepadState::EMPTY, 1_u32 << 3);
    assert!(frame.is_addressed_to(3));
    for rid in (0..32_u8).filter(|r| *r != 3) {
      assert!(
        !frame.is_addressed_to(rid),
        "unicast should not address receiver_id={rid}",
      );
    }
  }

  #[test]
  fn multicast_mask_addresses_selected_subset() {
    // dest_mask = bit-1 | bit-5 | bit-9
    let mask = (1_u32 << 1) | (1_u32 << 5) | (1_u32 << 9);
    let frame = Frame::with_dest(1, GamepadState::EMPTY, mask);
    for rid in [1_u8, 5, 9] {
      assert!(frame.is_addressed_to(rid), "expected to address {rid}");
    }
    for rid in [0_u8, 2, 6, 10, 31] {
      assert!(
        !frame.is_addressed_to(rid),
        "did not expect to address {rid}",
      );
    }
  }

  #[test]
  fn dest_mask_survives_roundtrip() {
    let mask = 0xDEAD_BEEF;
    let frame = Frame::with_dest(42, GamepadState::EMPTY, mask);
    let wire = encode_frame(&frame);
    let decoded = decode_frame(&wire).expect("decode ok");
    assert_eq!(decoded.dest_mask, mask);
  }
}
