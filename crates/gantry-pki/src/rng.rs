//! 把注入的 [`Entropy`] 适配成密码学库要求的 `RngCore + CryptoRng`。
//!
//! 生产中 `gantryd` 注入操作系统熵源；测试注入可复现的序列。

use gantry_core::ids::Entropy;
use rand_core::{CryptoRng, RngCore};

pub(crate) struct EntropyRng<'a>(pub(crate) &'a dyn Entropy);

impl RngCore for EntropyRng<'_> {
    fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        self.0.fill(&mut buf);
        u32::from_le_bytes(buf)
    }

    fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        self.0.fill(&mut buf);
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.0.fill(dest);
        Ok(())
    }
}

// `Entropy` 的契约就是"可信熵源"（core ids.rs），这里如实标注。
impl CryptoRng for EntropyRng<'_> {}
