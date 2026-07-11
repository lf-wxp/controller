//! CRC-16/IBM（也叫 CRC-16/ARC、CRC-16/LHA）
//!
//! - **多项式**：0xA001（0x8005 反射后）
//! - **初始值**：0x0000
//! - **反射输入/输出**：是
//! - **异或输出**：0x0000
//!
//! ## 为什么选这个？
//! - 实现简单：查表可省，直接位运算约 10 行代码
//! - 广泛使用：Modbus RTU / ARC / LZH 都用这个变体
//! - 强度足够：对 25 字节的短帧，未检出错误率极低
//!
//! ## 为什么不用 crc crate？
//! 避免额外依赖，且本实现无 std 需求、可 const。

/// 计算数据的 CRC-16/IBM
///
/// # Example
/// ```ignore
/// let data = b"123456789";
/// assert_eq!(controller::protocol::crc::crc16_ibm(data), 0xBB3D);
/// ```
#[inline]
pub fn crc16_ibm(data: &[u8]) -> u16 {
  let mut crc: u16 = 0x0000;
  for &byte in data {
    crc ^= u16::from(byte);
    for _ in 0..8 {
      if (crc & 0x0001) != 0 {
        crc = (crc >> 1) ^ 0xA001;
      } else {
        crc >>= 1;
      }
    }
  }
  crc
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 标准测试向量："123456789" 的 CRC-16/IBM = 0xBB3D
  #[test]
  fn known_vector_123456789() {
    assert_eq!(crc16_ibm(b"123456789"), 0xBB3D);
  }

  #[test]
  fn empty_input() {
    assert_eq!(crc16_ibm(&[]), 0x0000);
  }

  #[test]
  fn different_inputs_produce_different_crc() {
    let a = crc16_ibm(&[0x01, 0x02, 0x03]);
    let b = crc16_ibm(&[0x01, 0x02, 0x04]);
    assert_ne!(a, b);
  }
}
