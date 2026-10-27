//! 能力令牌的铸造与哈希（`docs/interfaces/egress-control.md` §3）。
//!
//! 令牌是 `gjt1_` / `gst1_` + 32 字节 CSPRNG 随机数的 base64url（无填充，43 字符）。
//! Egress 只保存 `sha256(token)`；校验时先对来访令牌求哈希再按哈希查表，比较的对象是
//! 哈希而不是令牌原文，查表耗时不泄露令牌内容。

use gantry_core::canon::{sha256, Sha256Digest};
use gantry_core::egress::{JobToken, SvcToken, JOB_TOKEN_PREFIX, SVC_TOKEN_PREFIX};
use gantry_core::ids::Entropy;

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// 操作系统 CSPRNG。
#[derive(Clone, Copy, Debug, Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        // 取不到系统熵时不能退化成弱随机数：直接中止。
        getrandom::fill(buf).expect("operating system CSPRNG is unavailable");
    }
}

/// 令牌哈希：表里唯一保存的形式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TokenHash(Sha256Digest);

impl TokenHash {
    pub fn of(token: &str) -> Self {
        Self(sha256(token.as_bytes()))
    }

    pub fn digest(self) -> Sha256Digest {
        self.0
    }
}

pub fn mint_job_token(entropy: &dyn Entropy) -> (JobToken, TokenHash) {
    let raw = mint(entropy, JOB_TOKEN_PREFIX);
    let hash = TokenHash::of(&raw);
    let token = JobToken::parse(&raw).expect("minted job token matches its own format");
    (token, hash)
}

pub fn mint_svc_token(entropy: &dyn Entropy) -> (SvcToken, TokenHash) {
    let raw = mint(entropy, SVC_TOKEN_PREFIX);
    let hash = TokenHash::of(&raw);
    let token = SvcToken::parse(&raw).expect("minted svc token matches its own format");
    (token, hash)
}

fn mint(entropy: &dyn Entropy, prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    entropy.fill(&mut bytes);
    let mut out = String::with_capacity(prefix.len() + 43);
    out.push_str(prefix);
    out.push_str(&base64url(&bytes));
    out
}

fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | u32::from(*b) << (16 - 8 * i));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(B64URL[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_rfc4648_vectors() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(b"foob"), "Zm9vYg");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}
