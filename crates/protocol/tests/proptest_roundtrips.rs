//! # Property-based tests for protocol
//!
//! 使用 [`proptest`] 对协议编解码函数做**随机往返验证**：
//! 手工挑选的边界值容易漏掉边角情况，proptest 用随机输入 + 缩小失败样例的
//! 组合策略可以自动找出手工测试遗漏的 bug。
//!
//! ## 覆盖的属性
//! 1. `frame_roundtrip_preserves_state`：`encode_frame(x)` 后 `decode_frame` 得到相同 x
//! 2. `frame_single_bit_flip_breaks_crc`：任何位翻转必然让 decode 失败（CRC 保护）
//! 3. `command_roundtrip_preserves_all_fields`：任意 kind/seq/key_id 组合能完整 roundtrip
//! 4. `command_kind_byte_matches_body`：wire 上第 3 字节的 kind 值与 Rust 枚举变体一一对应
//! 5. `response_roundtrip_preserves_all_fields`：ack/error/battery/nonce 四种 body 都能完整 roundtrip
//! 6. `crc16_is_deterministic`：同样输入永远得到同样 CRC 值
//! 7. `replay_monotonic_increasing_all_accepted`：严格单调递增 seq 序列全部接受
//! 8. `replay_exact_replay_always_rejected`：任意 seq 序列，重复 seq 一定被拒绝
//!
//! ## 运行
//! ```bash
//! # Host target 下运行（不能用默认 xtensa-esp32-none-elf）
//! cargo test -p protocol --target x86_64-apple-darwin
//! # 或（Apple Silicon）
//! cargo test -p protocol --target aarch64-apple-darwin
//! ```

use proptest::prelude::*;
use protocol::auth::{KeyId, init_session_nonce};
use protocol::crc::crc16_ibm;
use protocol::{
  AntiReplayWindow, Command, CommandBody, CommandKind, CommandResponse, ErrorCode, Frame,
  FrameHeader, GamepadState, PROTOCOL_VERSION, ResponseBody, decode_command, decode_frame,
  decode_response, encode_command, encode_frame, encode_response,
};

// ============================================================
// Strategy 生成器
// ============================================================

/// 生成任意合法 [`GamepadState`]
///
/// 字段范围略作限制以匹配硬件采样域，但仍能覆盖 codec 全部输入空间。
fn any_gamepad_state() -> impl Strategy<Value = GamepadState> {
  (
    any::<u16>(),
    any::<i16>(),
    any::<i16>(),
    any::<u16>(),
    any::<u16>(),
  )
    .prop_map(|(buttons, joy_x, joy_y, knob_1, knob_2)| GamepadState {
      buttons,
      joy_x,
      joy_y,
      knob_1,
      knob_2,
      _reserved: 0,
    })
}

/// 生成合法的 [`KeyId`]（0..=15）
fn any_key_id() -> impl Strategy<Value = KeyId> {
  (0_u8..=15_u8).prop_map(|raw| KeyId::new(raw).expect("0..=15 is always in range"))
}

/// 只生成"手柄配置里已启用"的 [`KeyId`]（slot 0 或 1）
///
/// [`encode_command`] 会对未启用 slot 返回错误；proptest 需要保证 encode 成功。
fn any_enabled_key_id() -> impl Strategy<Value = KeyId> {
  (0_u8..=1_u8).prop_map(|raw| KeyId::new(raw).expect("0..=1 is enabled in default keyring"))
}

/// 生成任意 [`CommandBody`] 变体
fn any_command_body() -> impl Strategy<Value = CommandBody> {
  prop_oneof![
    Just(CommandBody::Nop),
    (0_u8..=3, 1_u8..=20, 50_u16..=2000).prop_map(|(led_idx, count, period_ms)| {
      CommandBody::LedBlink {
        led_idx,
        count,
        period_ms,
      }
    }),
    (0_u16..=1000, 0_u16..=1000).prop_map(|(joy_scale, knob_scale)| {
      CommandBody::SetSensitivity {
        joy_scale,
        knob_scale,
      }
    }),
    // ShowToast: 长度 0..=5, 5 字节 buffer
    prop::collection::vec(any::<u8>(), 0..=5).prop_map(|text| {
      let len = text.len() as u8;
      let mut bytes = [0_u8; 5];
      for (i, b) in text.iter().enumerate() {
        bytes[i] = *b;
      }
      CommandBody::ShowToast { len, bytes }
    }),
    any::<bool>().prop_map(|simulate| CommandBody::SetBatteryMode { simulate }),
    Just(CommandBody::Announce),
    (any::<[u8; 6]>(), 0_u8..32_u8)
      .prop_map(|(mac, receiver_id)| CommandBody::AssignId { mac, receiver_id }),
  ]
}

/// 生成任意 [`Command`]（seq >= 1，避免 anti-replay 拒 0）
fn any_command() -> impl Strategy<Value = Command> {
  (1_u32.., any_enabled_key_id(), any_command_body()).prop_map(|(seq, key_id, kind)| Command {
    seq,
    key_id,
    kind,
  })
}

/// 生成任意 [`ResponseBody`] 变体
fn any_response_body() -> impl Strategy<Value = ResponseBody> {
  prop_oneof![
    Just(ResponseBody::Ack),
    prop_oneof![
      Just(ErrorCode::InvalidArgument),
      Just(ErrorCode::Unsupported),
      Just(ErrorCode::Busy),
    ]
    .prop_map(ResponseBody::Error),
    (0_u8..=100).prop_map(|percent| ResponseBody::BatterySnapshot { percent }),
    any::<u32>().prop_map(|nonce| ResponseBody::NonceHello { nonce }),
    (any::<[u8; 6]>(), any::<i8>(), any::<[u8; 3]>()).prop_map(|(mac, rssi_dbm, role_tag)| {
      ResponseBody::AnnounceReply {
        mac,
        rssi_dbm,
        role_tag,
      }
    }),
  ]
}

/// 生成任意 [`CommandResponse`]
fn any_response() -> impl Strategy<Value = CommandResponse> {
  (any::<u32>(), any_enabled_key_id(), any_response_body()).prop_map(|(req_seq, key_id, body)| {
    CommandResponse {
      req_seq,
      key_id,
      body,
    }
  })
}

// ============================================================
// Frame roundtrip
// ============================================================

proptest! {
  #[test]
  fn frame_roundtrip_preserves_state(
    payload in any_gamepad_state(),
    seq in any::<u32>(),
    dest_mask in any::<u32>(),
  ) {
    // Arrange
    let frame = Frame {
      header: FrameHeader {
        magic: protocol::FRAME_MAGIC,
        version: PROTOCOL_VERSION,
        seq,
      },
      payload,
      dest_mask,
    };

    // Act
    let encoded = encode_frame(&frame);
    let decoded = decode_frame(&encoded).expect("frame should decode after encode");

    // Assert
    prop_assert_eq!(decoded.header.seq, seq);
    prop_assert_eq!(decoded.dest_mask, dest_mask);
    prop_assert_eq!(decoded.payload.buttons, payload.buttons);
    prop_assert_eq!(decoded.payload.joy_x, payload.joy_x);
    prop_assert_eq!(decoded.payload.joy_y, payload.joy_y);
    prop_assert_eq!(decoded.payload.knob_1, payload.knob_1);
    prop_assert_eq!(decoded.payload.knob_2, payload.knob_2);
  }

  #[test]
  fn frame_single_bit_flip_breaks_crc(
    payload in any_gamepad_state(),
    seq in any::<u32>(),
    flip_offset in 0_usize..25,
    flip_bit in 0_u8..8,
  ) {
    // Arrange: encode 一个合法帧
    let frame = Frame {
      header: FrameHeader {
        magic: protocol::FRAME_MAGIC,
        version: PROTOCOL_VERSION,
        seq,
      },
      payload,
      dest_mask: u32::MAX,
    };
    let mut encoded = encode_frame(&frame);

    // Act: 翻转一个位
    encoded[flip_offset] ^= 1 << flip_bit;

    // Assert: CRC/magic/version 中至少一个会失败
    let decoded = decode_frame(&encoded);
    prop_assert!(decoded.is_err(),
      "flipping bit {} at offset {} should invalidate frame",
      flip_bit, flip_offset);
  }
}

// ============================================================
// Command roundtrip
// ============================================================

proptest! {
  #[test]
  fn command_roundtrip_preserves_all_fields(cmd in any_command()) {
    // Arrange: 设置一个已知 session nonce（HMAC 依赖它）
    init_session_nonce(0xDEAD_BEEF);

    // Act
    let encoded = encode_command(&cmd);
    let decoded = decode_command(&encoded).expect("valid command must decode");

    // Assert: 所有字段完整保留
    prop_assert_eq!(decoded.seq, cmd.seq);
    prop_assert_eq!(decoded.key_id, cmd.key_id);
    match (&decoded.kind, &cmd.kind) {
      (CommandBody::Nop, CommandBody::Nop) => {}
      (
        CommandBody::LedBlink { led_idx: a1, count: a2, period_ms: a3 },
        CommandBody::LedBlink { led_idx: b1, count: b2, period_ms: b3 },
      ) => {
        prop_assert_eq!(a1, b1);
        prop_assert_eq!(a2, b2);
        prop_assert_eq!(a3, b3);
      }
      (
        CommandBody::SetSensitivity { joy_scale: a1, knob_scale: a2 },
        CommandBody::SetSensitivity { joy_scale: b1, knob_scale: b2 },
      ) => {
        prop_assert_eq!(a1, b1);
        prop_assert_eq!(a2, b2);
      }
      (
        CommandBody::ShowToast { len: a1, bytes: a2 },
        CommandBody::ShowToast { len: b1, bytes: b2 },
      ) => {
        prop_assert_eq!(a1, b1);
        prop_assert_eq!(a2, b2);
      }
      (
        CommandBody::SetBatteryMode { simulate: a1 },
        CommandBody::SetBatteryMode { simulate: b1 },
      ) => {
        prop_assert_eq!(a1, b1);
      }
      (CommandBody::Announce, CommandBody::Announce) => {}
      (
        CommandBody::AssignId { mac: a1, receiver_id: a2 },
        CommandBody::AssignId { mac: b1, receiver_id: b2 },
      ) => {
        prop_assert_eq!(a1, b1);
        prop_assert_eq!(a2, b2);
      }
      _ => prop_assert!(false, "body variant mismatch after roundtrip"),
    }
  }

  #[test]
  fn command_kind_byte_matches_body(cmd in any_command()) {
    // Arrange
    init_session_nonce(1);

    // Act
    let encoded = encode_command(&cmd);

    // Assert: wire 上第 3 字节的 kind 值与 Rust 枚举变体一一对应
    let expected_kind = match &cmd.kind {
      CommandBody::Nop => CommandKind::Nop,
      CommandBody::LedBlink { .. } => CommandKind::LedBlink,
      CommandBody::SetSensitivity { .. } => CommandKind::SetSensitivity,
      CommandBody::ShowToast { .. } => CommandKind::ShowToast,
      CommandBody::SetBatteryMode { .. } => CommandKind::SetBatteryMode,
      CommandBody::Announce => CommandKind::Announce,
      CommandBody::AssignId { .. } => CommandKind::AssignId,
    };
    prop_assert_eq!(encoded[3], expected_kind as u8);
  }
}

// ============================================================
// Response roundtrip
// ============================================================

proptest! {
  #[test]
  fn response_roundtrip_preserves_all_fields(resp in any_response()) {
    // Arrange
    init_session_nonce(0xCAFE_BABE);

    // Act
    let encoded = encode_response(&resp);
    let decoded = decode_response(&encoded).expect("valid response must decode");

    // Assert
    prop_assert_eq!(decoded.req_seq, resp.req_seq);
    prop_assert_eq!(decoded.key_id, resp.key_id);
    match (&decoded.body, &resp.body) {
      (ResponseBody::Ack, ResponseBody::Ack) => {}
      (ResponseBody::Error(a), ResponseBody::Error(b)) => {
        prop_assert_eq!(*a as u8, *b as u8);
      }
      (
        ResponseBody::BatterySnapshot { percent: a },
        ResponseBody::BatterySnapshot { percent: b },
      ) => {
        prop_assert_eq!(a, b);
      }
      (
        ResponseBody::NonceHello { nonce: a },
        ResponseBody::NonceHello { nonce: b },
      ) => {
        prop_assert_eq!(a, b);
      }
      (
        ResponseBody::AnnounceReply { mac: a1, rssi_dbm: a2, role_tag: a3 },
        ResponseBody::AnnounceReply { mac: b1, rssi_dbm: b2, role_tag: b3 },
      ) => {
        prop_assert_eq!(a1, b1);
        prop_assert_eq!(a2, b2);
        prop_assert_eq!(a3, b3);
      }
      _ => prop_assert!(false, "response body variant mismatch"),
    }
  }
}

// ============================================================
// CRC 属性
// ============================================================

proptest! {
  #[test]
  fn crc16_is_deterministic(data in prop::collection::vec(any::<u8>(), 0..=256)) {
    // 同样输入应总是产生同样 CRC —— 无状态、纯函数
    let crc1 = crc16_ibm(&data);
    let crc2 = crc16_ibm(&data);
    prop_assert_eq!(crc1, crc2);
  }
}

// ============================================================
// Anti-Replay 状态机属性
// ============================================================

proptest! {
  #[test]
  fn replay_monotonic_increasing_all_accepted(
    start in 1_u32..=1000,
    count in 1_usize..=100,
  ) {
    // Arrange
    let mut window = AntiReplayWindow::new();

    // Act + Assert
    for i in 0..count {
      let seq = start.wrapping_add(i as u32);
      prop_assert!(window.check_and_update(seq).is_ok(),
        "strictly monotonic seq {} should be accepted", seq);
    }
  }

  #[test]
  fn replay_exact_replay_always_rejected(
    seqs in prop::collection::vec(1_u32..=10_000, 1..=20),
  ) {
    // Arrange
    let mut window = AntiReplayWindow::new();
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();

    // Act + Assert
    for seq in seqs {
      let is_new = seen.insert(seq);
      let is_too_old = window.last_seq() >= 64
        && seq < window.last_seq().saturating_sub(63);
      let result = window.check_and_update(seq);
      if is_new && !is_too_old {
        prop_assert!(
          result.is_ok(),
          "new seq {} within window should be accepted",
          seq
        );
      } else if !is_new {
        prop_assert!(
          result.is_err(),
          "duplicate seq {} must be rejected",
          seq
        );
      }
    }
  }
}
