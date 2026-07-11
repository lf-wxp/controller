//! # 协议层配置常量
//!
//! 与协议编解码相关的**编译期配置**：认证开关、HMAC tag 长度、
//! 密钥槽位数、共享密钥数组等。**这些常量原本位于 `crate::config`
//! 手柄侧，为让 dashboard/WASM 端也能复用**，统一下沉到 protocol crate。
//!
//! ## 使用侧
//! - **手柄端**：通过 `crate::config::auth` / `crate::config::keyring` re-export 引用
//! - **Dashboard 端**：直接 `use controller_protocol::config::keyring::*`
//! - **协议内部**：`use crate::config::auth::*`（相对本 crate）

/// 认证子系统配置
pub mod auth {
  /// 认证开关（O 选项 + K1 联合）
  ///
  /// - `true`（默认）：所有入站 Command / Response 必须通过 HMAC 校验
  /// - `false`：**调试后门** —— 完全跳过 HMAC 校验（**生产环境务必设为 true**）
  ///
  /// # 编译期开关
  /// 本常量由 `debug-auth-bypass` feature **反向**控制：
  /// - **默认（生产 build）**：feature 关闭 → `AUTH_ENABLED = true`
  /// - **仅调试**：显式 `cargo build --features debug-auth-bypass` →
  ///   `AUTH_ENABLED = false`（完全跳过 HMAC；生产 CI 应当拒绝该 feature）
  ///
  /// # 联动
  /// `crate::auth::verify_hmac_tag` 首行判断此常量；`false` 时立即返回 `true`。
  #[cfg(feature = "debug-auth-bypass")]
  pub const AUTH_ENABLED: bool = false;

  /// 生产 build 下 `AUTH_ENABLED = true`（详见上方 doc）
  #[cfg(not(feature = "debug-auth-bypass"))]
  pub const AUTH_ENABLED: bool = true;

  /// 编译期断言：非 debug-auth-bypass build 必须保持 AUTH_ENABLED = true。
  ///
  /// CI 应当禁止合入任何显式关闭该断言的 PR。
  #[cfg(not(feature = "debug-auth-bypass"))]
  const _: () = assert!(
    AUTH_ENABLED,
    "AUTH_ENABLED must remain true in production builds"
  );

  /// HMAC 截断长度（字节）
  ///
  /// 完整的 HMAC-SHA256 是 32 字节；我们截断到前 4 字节。理由：
  /// - Command / Response 帧长度固定为 24 字节，无空间放 32B tag
  /// - 4 字节 = 32-bit 空间；即使攻击者尝试爆破，也需要 2^32 次尝试
  /// - 每次爆破需要一整轮 CRC + HMAC 计算，实际成本极高
  pub const HMAC_TAG_LEN: usize = 4;
}

/// 密钥环配置（K 选项 + O 选项）
pub mod keyring {
  /// 单个共享密钥的字节长度
  ///
  /// 与 [`sha2::Sha256`] 的**分块大小对齐**（64B）不是必要的；32B 已经是
  /// 强密钥。选 32 是取"够长而不冗余"的平衡。
  pub const SECRET_LEN: usize = 32;

  /// 密钥槽位数（O 选项：支持多密钥并存）
  ///
  /// # 密钥轮换语义
  /// - Day 0：Host 用 key_id=0 (SECRET_V1) → 手柄 slot 0 = Some(SECRET_V1)
  /// - Day 15：Host 部署更新，开始用 key_id=1 (SECRET_V2)
  ///   → 手柄 slot 0 = Some(SECRET_V1) 与 slot 1 = Some(SECRET_V2) 并存
  /// - Day 30：Host 完全切换到 key_id=1，slot 0 关停
  ///   → 手柄 slot 0 = None（拒绝老 key）
  ///
  /// # wire 上限
  /// [`crate::command`] 帧的 `version_byte` 高 4 位承载 key_id → 最大 15。
  /// 因此 **key_id 空间上限 15**；[`KEY_SLOTS`] 决定"当前固件实际支持多少版"。
  pub const KEY_SLOTS: usize = 4;

  /// 编译期断言：KEY_SLOTS 必须 ≤ 16（wire 4-bit key_id 上限）
  const _: () = assert!(KEY_SLOTS <= 16, "KEY_SLOTS must fit in 4 bits");

  /// **通过 build.rs 从 `CONTROLLER_SECRET_V1` 环境变量注入的**主用共享密钥（v1）
  ///
  /// # 密钥来源（C-3 加固）
  /// build.rs 会读取 `CONTROLLER_SECRET_V1` 环境变量并通过 `cargo:rustc-env`
  /// 暴露给源码；`env!()` 在编译期把字符串字面量注入到二进制中。
  ///
  /// - **生产 build**：应关闭 `embed-default-secrets` feature，并显式
  ///   `CONTROLLER_SECRET_V1=<32-byte high-entropy string> cargo build ...`
  /// - **开发 build**：`embed-default-secrets` 默认开启，缺失环境变量时
  ///   使用内置弱占位符（会 `cargo:warning` 提示）
  pub const SECRET_V1: &[u8; SECRET_LEN] =
    convert_secret(env!("CONTROLLER_SECRET_V1_BYTES").as_bytes());

  /// **通过 build.rs 从 `CONTROLLER_SECRET_V2` 环境变量注入的**备用共享密钥（v2）
  ///
  /// 密钥轮换时启用。语义与用法同 [`SECRET_V1`]。
  pub const SECRET_V2: &[u8; SECRET_LEN] =
    convert_secret(env!("CONTROLLER_SECRET_V2_BYTES").as_bytes());

  /// 在编译期把 `&[u8]` 转成 `&[u8; SECRET_LEN]`；长度不符则触发编译期 panic。
  ///
  /// # 为什么手写而不是 `<[u8; N]>::try_from`？
  /// `try_from` 是 runtime API，无法在 `const` 上下文使用；此函数用 `const fn`
  /// + 手动切片索引，在编译期完成校验，生成的机器码与手写数组字面量等价。
  const fn convert_secret(bytes: &[u8]) -> &[u8; SECRET_LEN] {
    assert!(
      bytes.len() == SECRET_LEN,
      "CONTROLLER_SECRET_V* must be exactly 32 bytes"
    );
    // SAFETY: 上面的 assert 已经保证长度精确等于 SECRET_LEN；
    // `&[u8]` 与 `&[u8; SECRET_LEN]` 是同一段内存的两种视图，指针 cast 安全。
    unsafe { &*(bytes.as_ptr() as *const [u8; SECRET_LEN]) }
  }

  /// 密钥环（O 选项）
  ///
  /// 长度 = [`KEY_SLOTS`]。每个 slot：
  /// - `Some(&[u8; 32])`：该 key_id 可用，[`crate::auth::verify_hmac_tag`]
  ///   会用此密钥校验 HMAC
  /// - `None`：该 key_id 已下线；任何声称使用此 key_id 的帧一律拒绝
  pub const SHARED_SECRETS: [Option<&'static [u8; SECRET_LEN]>; KEY_SLOTS] =
    [Some(SECRET_V1), Some(SECRET_V2), None, None];

  /// 手柄**主动出站**帧（Response / NonceHello）默认使用的 key_id
  ///
  /// - Response 本身响应了 Command 时，会用**请求命令的 key_id** 计算 HMAC
  /// - NonceHello 是主动广播（无对应 Command），用此默认 key_id
  pub const DEFAULT_KEY_ID: u8 = 0;
}
