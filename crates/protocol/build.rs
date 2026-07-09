//! # controller-protocol build script
//!
//! ## 职责（C-3 密钥安全修复）
//! 把 HMAC 共享密钥从"源码明文常量"转移到"编译期通过环境变量注入"，避免：
//! 1. 明文密钥被 git commit / 存档扫描到；
//! 2. dashboard WASM 打包时把生产密钥连同代码一起下发到浏览器。
//!
//! ## 环境变量
//! - `CONTROLLER_SECRET_V1`：主用共享密钥（**必需**为 32 字节 UTF-8 字符串）
//! - `CONTROLLER_SECRET_V2`：备用共享密钥（**必需**为 32 字节 UTF-8 字符串）
//!
//! ## 未提供环境变量的 fallback 策略
//! - 若开启 `embed-default-secrets` feature（**default 开启**）：使用内置的
//!   弱密钥占位符（可读 ASCII、仅用于开发/CI 冒烟测试）；
//! - 若关闭 `embed-default-secrets` feature：**编译期直接 panic**，强迫生产
//!   build 必须显式注入密钥。
//!
//! ## 输出
//! - `cargo:rustc-env=CONTROLLER_SECRET_V1_BYTES=...` —— 32 字节字符串
//! - `cargo:rustc-env=CONTROLLER_SECRET_V2_BYTES=...` —— 32 字节字符串
//!
//! 后续在 `src/config.rs::keyring` 中通过 `env!()` 读取。

use std::env;

/// 密钥字节长度（与 `keyring::SECRET_LEN` 保持一致）
const SECRET_LEN: usize = 32;

/// fallback 弱密钥（仅 `embed-default-secrets` feature 下使用）
///
/// 必须是恰好 32 字节的**可打印 ASCII 字符**，不能含 NUL — 因为
/// `cargo:rustc-env=KEY=VAL` 命令不允许 NUL 字节进入环境变量。
const FALLBACK_SECRET_V1: &str = "esp32-controller-shared-key-v1!!";
const FALLBACK_SECRET_V2: &str = "esp32-controller-shared-key-v2!!";

fn main() {
  println!("cargo:rerun-if-env-changed=CONTROLLER_SECRET_V1");
  println!("cargo:rerun-if-env-changed=CONTROLLER_SECRET_V2");
  println!("cargo:rerun-if-changed=build.rs");

  let secret_v1 = resolve_secret("CONTROLLER_SECRET_V1", FALLBACK_SECRET_V1);
  let secret_v2 = resolve_secret("CONTROLLER_SECRET_V2", FALLBACK_SECRET_V2);

  // 把最终字节字符串暴露给 src/config.rs 用 env! 读取
  println!("cargo:rustc-env=CONTROLLER_SECRET_V1_BYTES={secret_v1}");
  println!("cargo:rustc-env=CONTROLLER_SECRET_V2_BYTES={secret_v2}");
}

/// 解析单个密钥：优先取环境变量；若缺失，按 feature 决定 fallback 或 panic
fn resolve_secret(env_key: &str, fallback: &str) -> String {
  match env::var(env_key) {
    Ok(value) => {
      assert_eq!(
        value.len(),
        SECRET_LEN,
        "{env_key} must be exactly {SECRET_LEN} bytes (got {} bytes)",
        value.len(),
      );
      value
    }
    Err(_) => {
      if cfg!(feature = "embed-default-secrets") {
        println!(
          "cargo:warning=[SECURITY] {env_key} not set; falling back to built-in DEBUG secret. \
           This build MUST NOT be used in production."
        );
        fallback.to_string()
      } else {
        panic!(
          "{env_key} environment variable is required (feature `embed-default-secrets` is off). \
           Set it to a 32-byte string before building."
        );
      }
    }
  }
}
