//! # 启动自检（S1 · Boot Self-Test）
//!
//! **目的**：在 `main` 内 spawn 任何 embassy task 之前，验证协议核心不变式，
//! 一旦发现 build 配置 / feature flag / 依赖版本引入的自洽性破坏，
//! **立刻 panic 而非上机后才被用户发现"手柄不响应"**。
//!
//! ## 覆盖的不变式
//! 1. **密钥环长度正确**：`SECRET_V1` / `SECRET_V2` 恰好 32 字节
//!    （由 `crates/protocol/build.rs` + `const fn` 保证，此处再做一次运行时确认，
//!    捕获"未来重构漏改常量"的场景）
//! 2. **两把密钥不相等**：否则密钥轮换 (O 选项) 变成 no-op，等价于单密钥
//! 3. **HMAC 环回正确性**：用 slot 0 签、slot 0 验必须通过；
//!    slot 0 签、slot 1 验必须失败（跨 key_id 攻击必被拒）
//! 4. **Frame 编解码 round-trip 保持 21 字节**：任何依赖 `FRAME_LEN` 的
//!    构建脚本、feature flag 若把布局改坏了会在这里当场炸掉
//!
//! ## 何时调用
//! 在 [`crate::protocol::init_session_nonce`] **之前**调用；本函数会用
//! 固定测试 nonce (`0xA5A5_A5A5`) 短暂占据 `SESSION_NONCE`，
//! 运行结束后由真实 `init_session_nonce` 覆盖。
//!
//! ## 开销
//! - 约 6 次 HMAC-SHA256 计算 + 1 次 CRC16 + 内存 memcpy
//! - ESP32 240 MHz 下 **< 2 ms**；相较开机后 100+ ms 的 BLE stack 初始化可忽略
//!
//! ## 失败行为
//! `assert!` 触发 panic → `panic_rtt_target` handler 打印 backtrace →
//! ESP32 复位（无限循环重启，直到修复 build）。

use defmt::info;

use crate::protocol::auth::{compute_hmac_tag, verify_hmac_tag};
use crate::protocol::protocol_config::auth::AUTH_ENABLED;
use crate::protocol::protocol_config::keyring::{KEY_SLOTS, SECRET_LEN, SECRET_V1, SECRET_V2};
use crate::protocol::{FRAME_LEN, Frame, GamepadState, KeyId, decode_frame, encode_frame};

/// 自检使用的固定 nonce（任意非零常量即可，仅为让 HMAC 计算可复现）
const SELF_TEST_NONCE: u32 = 0xA5A5_A5A5;

/// 执行完整启动自检
///
/// # Panics
/// 任一不变式失败即 panic，阻断固件继续启动。
pub fn run() {
  info!("[SELF-TEST] Starting boot self-test");

  check_keyring();
  check_hmac_roundtrip();
  check_frame_roundtrip();

  info!("[SELF-TEST] All boot self-tests passed");
}

/// 校验 1 + 2：密钥环长度 + 两把密钥不同
fn check_keyring() {
  assert_eq!(
    SECRET_V1.len(),
    SECRET_LEN,
    "SECRET_V1 length must equal SECRET_LEN"
  );
  assert_eq!(
    SECRET_V2.len(),
    SECRET_LEN,
    "SECRET_V2 length must equal SECRET_LEN"
  );

  // KEY_SLOTS 至少覆盖 V1 / V2 两把常用槽位
  //
  // 使用 `const { assert!(..) }` 而不是运行时 assert：这是**编译期**断言，
  // 保证 KEY_SLOTS 被改小时立刻编译失败，比运行时启动才 panic 更早暴露。
  const {
    assert!(
      KEY_SLOTS >= 2,
      "KEY_SLOTS must accommodate SECRET_V1 and SECRET_V2"
    )
  };

  // 常时比较：两把密钥必须不同 —— 否则 O 选项轮换退化为 no-op
  //
  // 使用逐字节 XOR 累加，避免早期编译器优化把长比较变成 memcmp 从而泄露
  // 前若干字节比较结果的时序（虽然 self-test 场景无攻击面，保持一致的
  // 编码风格）。
  let mut diff: u8 = 0;
  for (a, b) in SECRET_V1.iter().zip(SECRET_V2.iter()) {
    diff |= a ^ b;
  }
  assert!(
    diff != 0,
    "SECRET_V1 and SECRET_V2 must differ (rotation would be a no-op)"
  );
}

/// 校验 3：HMAC 正向/反向环回
fn check_hmac_roundtrip() {
  // 用固定 nonce 让 tag 可复现；调用方保证 self-test 结束后由 init_session_nonce 覆盖
  crate::protocol::init_session_nonce(SELF_TEST_NONCE);

  let key0 = KeyId::new(0).expect("KeyId(0) always in range");
  let key1 = KeyId::new(1).expect("KeyId(1) always in range");

  let msg: &[u8] = b"boot self test canonical message";

  // 3.1 正向：slot 0 签、slot 0 验必须通过
  let tag0 = compute_hmac_tag(msg, key0).expect("SECRET_V1 slot must be enabled");
  assert!(
    verify_hmac_tag(msg, &tag0, key0),
    "HMAC self-test: sign/verify round-trip failed for slot 0"
  );

  // 3.2 反向 & 3.3 消息篡改：仅在生产 build（AUTH_ENABLED = true）下有意义。
  //
  // `verify_hmac_tag` 在 AUTH_ENABLED = false 时无条件返回 true，
  // 此时断言"必须失败"会反过来把自己 panic 掉。因此用常量分支跳过 —— 编译器
  // 会 dead-code 消除未选中的分支，运行时零开销。
  if AUTH_ENABLED {
    // 3.2 跨 key_id 攻击必被拒
    assert!(
      !verify_hmac_tag(msg, &tag0, key1),
      "HMAC self-test: cross-key_id verification must fail"
    );

    // 3.3 消息篡改必被拒
    let tampered: &[u8] = b"boot self test canonical MESSAGE"; // 仅大小写不同
    assert!(
      !verify_hmac_tag(tampered, &tag0, key0),
      "HMAC self-test: tampered message must be rejected"
    );
  } else {
    // debug-auth-bypass build 下 verify 恒真，跳过否证测试
    let _ = key1;
  }
}

/// 校验 4：Frame encode → decode 无损，且长度恒等于 25
fn check_frame_roundtrip() {
  let frame = Frame::new(0xDEAD_BEEF, GamepadState::EMPTY);
  let bytes = encode_frame(&frame);

  assert_eq!(
    bytes.len(),
    FRAME_LEN,
    "encoded frame length must equal FRAME_LEN"
  );
  assert_eq!(
    FRAME_LEN, 25,
    "wire protocol frame size must remain 25 bytes"
  );

  let decoded = decode_frame(&bytes).expect("self-test frame must decode");
  assert_eq!(
    decoded, frame,
    "self-test frame round-trip must be lossless"
  );
}
