//! # controller-protocol build script
//!
//! ## 职责（C-3 密钥安全修复）
//! 把 HMAC 共享密钥从"源码明文常量"转移到"编译期通过环境变量注入"，避免：
//! 1. 明文密钥被 git commit / 存档扫描到；
//! 2. dashboard WASM 打包时把生产密钥连同代码一起下发到浏览器。
//!
//! ## 密钥来源优先级（从高到低）
//! 1. **Shell / CI 环境变量** `CONTROLLER_SECRET_V1` / `CONTROLLER_SECRET_V2`
//! 2. **`.env` 文件**（workspace 根，自动向上查找，最多 6 层）
//! 3. **`embed-default-secrets` fallback**（弱占位符，仅供开发/CI 冒烟）
//!
//! ## 环境变量
//! - `CONTROLLER_SECRET_V1`：主用共享密钥（**必需**为 32 字节 UTF-8 字符串）
//! - `CONTROLLER_SECRET_V2`：备用共享密钥（**必需**为 32 字节 UTF-8 字符串）
//!
//! ## `.env` 文件语法
//! - 忽略空行与 `#` 开头的注释行
//! - 支持 `KEY=VALUE`、`export KEY=VALUE`
//! - 支持 `KEY="VALUE"` / `KEY='VALUE'`（成对引号会被剥离）
//! - 只解析 `CONTROLLER_SECRET_V1` / `CONTROLLER_SECRET_V2`，其余键忽略
//! - 若同名变量已在环境中存在，则 `.env` **不会覆盖**（shell/CI 优先）
//!
//! ## 未提供任何来源时的 fallback 策略
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
use std::fs;
use std::path::{Path, PathBuf};

/// 密钥字节长度（与 `keyring::SECRET_LEN` 保持一致）
const SECRET_LEN: usize = 32;

/// 支持的密钥环境变量名（也是 `.env` 中允许解析的键白名单）
const SECRET_KEYS: &[&str] = &["CONTROLLER_SECRET_V1", "CONTROLLER_SECRET_V2"];

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

  // 优先级 2：从 workspace 根附近的 .env 加载缺失的密钥（不覆盖已存在的环境变量）
  load_dotenv_into_env();

  let secret_v1 = resolve_secret("CONTROLLER_SECRET_V1", FALLBACK_SECRET_V1);
  let secret_v2 = resolve_secret("CONTROLLER_SECRET_V2", FALLBACK_SECRET_V2);

  // 把最终字节字符串暴露给 src/config.rs 用 env! 读取
  println!("cargo:rustc-env=CONTROLLER_SECRET_V1_BYTES={secret_v1}");
  println!("cargo:rustc-env=CONTROLLER_SECRET_V2_BYTES={secret_v2}");
}

/// 从 workspace 附近向上搜寻 `.env`；找到则解析并补全缺失的密钥变量。
///
/// - 只补齐 [`SECRET_KEYS`] 中的白名单变量，避免污染其它环境
/// - 已由 shell / CI 导出的同名变量**不会**被覆盖（保证生产覆盖优先）
/// - 通过 `cargo:rerun-if-changed=<path>` 让 cargo 感知 `.env` 变更
fn load_dotenv_into_env() {
  let Some(path) = find_workspace_dotenv() else {
    return;
  };

  // 让 cargo 追踪 .env 变更；改动后自动重跑 build.rs
  println!("cargo:rerun-if-changed={}", path.display());

  let text = match fs::read_to_string(&path) {
    Ok(t) => t,
    Err(err) => {
      println!(
        "cargo:warning=[dotenv] failed to read {}: {err}",
        path.display()
      );
      return;
    }
  };

  for raw in text.lines() {
    let Some((key, value)) = parse_dotenv_line(raw) else {
      continue;
    };
    if !SECRET_KEYS.contains(&key.as_str()) {
      continue;
    }
    if env::var_os(&key).is_some() {
      // shell / CI 已经导出，.env 不覆盖
      continue;
    }
    // SAFETY: build.rs 由 cargo 单线程调用，此处仅在初始化阶段写入固定几个键；
    // Rust 2024 将 set_var 标为 unsafe 是出于多线程场景的考虑，这里不适用。
    unsafe {
      env::set_var(&key, &value);
    }
  }
}

/// 从 `CARGO_MANIFEST_DIR` 开始向上找 `.env`，最多回溯 6 层（足以覆盖
/// `crates/protocol` → workspace root 的常见布局）。
fn find_workspace_dotenv() -> Option<PathBuf> {
  let manifest_dir = env::var("CARGO_MANIFEST_DIR").ok()?;
  let mut dir: &Path = Path::new(&manifest_dir);
  for _ in 0..6 {
    let candidate = dir.join(".env");
    if candidate.is_file() {
      return Some(candidate);
    }
    dir = dir.parent()?;
  }
  None
}

/// 解析一行 `.env`：
/// - `KEY=VALUE`
/// - `export KEY=VALUE`
/// - `KEY="VALUE"` / `KEY='VALUE'`（成对引号剥离）
///
/// 忽略空行与 `#` 开头的注释行；不支持 `${VAR}` 展开、转义、多行字符串等
/// 复杂 dotenv 语法 —— build.rs 是安全敏感位置，刻意保持最小实现。
fn parse_dotenv_line(raw: &str) -> Option<(String, String)> {
  let line = raw.trim();
  if line.is_empty() || line.starts_with('#') {
    return None;
  }
  let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
  let (key, value) = line.split_once('=')?;
  let key = key.trim().to_string();
  if key.is_empty() {
    return None;
  }
  let value = value.trim();
  let value = strip_matched_quotes(value).to_string();
  Some((key, value))
}

/// 若字符串首尾是同种引号（`"` 或 `'`）则剥离一层；否则原样返回。
fn strip_matched_quotes(value: &str) -> &str {
  for quote in ['"', '\''] {
    if value.len() >= 2
      && value.starts_with(quote)
      && value.ends_with(quote)
    {
      return &value[1..value.len() - 1];
    }
  }
  value
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
