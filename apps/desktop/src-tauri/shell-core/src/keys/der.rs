//! ECDSA 签名 DER → IEEE P1363（desktop-shell.md §6.5 末段、§12"DER → P1363 转换"）。
//!
//! 平台（Security.framework、CNG）返回 `SEQUENCE { INTEGER r, INTEGER s }`；`grant` 要的是
//! 定长 `r ‖ s`。DER 整数是有符号最短编码：高位为 1 时带前导 `0x00`，数值小时短于 32 字节。

use super::P1363Sig;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("ECDSA DER 签名格式错误：{0}")]
pub struct DerError(&'static str);

pub fn der_to_p1363(der: &[u8]) -> Result<P1363Sig, DerError> {
    let (body, rest) = read_tlv(der, 0x30)?;
    if !rest.is_empty() {
        return Err(DerError("SEQUENCE 后有多余字节"));
    }
    let (r, body) = read_tlv(body, 0x02)?;
    let (s, body) = read_tlv(body, 0x02)?;
    if !body.is_empty() {
        return Err(DerError("SEQUENCE 内有多余字节"));
    }
    let mut out = [0u8; 64];
    write_scalar(r, &mut out[..32])?;
    write_scalar(s, &mut out[32..])?;
    Ok(P1363Sig::from_bytes(out))
}

/// IEEE P1363 `r ‖ s` → DER（CNG 的 ECDSA 签名是 P1363，rustls 的 `Signer` 要 DER）。
pub fn p1363_to_der(sig: &[u8; 64]) -> Vec<u8> {
    fn int(scalar: &[u8]) -> Vec<u8> {
        let start = scalar
            .iter()
            .position(|&b| b != 0)
            .unwrap_or(scalar.len() - 1);
        let digits = &scalar[start..];
        let mut out = vec![0x02];
        if digits[0] & 0x80 != 0 {
            out.push(digits.len() as u8 + 1);
            out.push(0);
        } else {
            out.push(digits.len() as u8);
        }
        out.extend_from_slice(digits);
        out
    }
    let mut body = int(&sig[..32]);
    body.extend(int(&sig[32..]));
    let mut out = vec![0x30, body.len() as u8];
    out.extend(body);
    out
}

/// 读一个短 / 长形式长度的 TLV，返回（内容，剩余）。P-256 签名最长 72 字节，长度不超过 1 字节。
fn read_tlv(input: &[u8], tag: u8) -> Result<(&[u8], &[u8]), DerError> {
    let (&t, input) = input.split_first().ok_or(DerError("意外结束"))?;
    if t != tag {
        return Err(DerError("标签不符"));
    }
    let (&len, input) = input.split_first().ok_or(DerError("意外结束"))?;
    let (len, input) = match len {
        0..=0x7f => (len as usize, input),
        0x81 => {
            let (&len, input) = input.split_first().ok_or(DerError("意外结束"))?;
            if len < 0x80 {
                return Err(DerError("长度不是最短编码"));
            }
            (len as usize, input)
        }
        _ => return Err(DerError("长度超出范围")),
    };
    if input.len() < len {
        return Err(DerError("长度超出输入"));
    }
    Ok(input.split_at(len))
}

/// 把 DER INTEGER 写成 32 字节大端；要求正数、最短编码、值不超过 32 字节。
fn write_scalar(int: &[u8], out: &mut [u8]) -> Result<(), DerError> {
    let (&first, _) = int.split_first().ok_or(DerError("空整数"))?;
    if first & 0x80 != 0 {
        return Err(DerError("负整数"));
    }
    let digits = if first == 0 {
        match int.get(1) {
            Some(next) if next & 0x80 != 0 => &int[1..],
            Some(_) => return Err(DerError("整数不是最短编码")),
            None => return Err(DerError("整数为零")),
        }
    } else {
        int
    };
    if digits.len() > out.len() {
        return Err(DerError("整数超过 32 字节"));
    }
    let pad = out.len() - digits.len();
    out[..pad].fill(0);
    out[pad..].copy_from_slice(digits);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn der(r: &[u8], s: &[u8]) -> Vec<u8> {
        let mut body = vec![0x02, r.len() as u8];
        body.extend_from_slice(r);
        body.extend_from_slice(&[0x02, s.len() as u8]);
        body.extend_from_slice(s);
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out
    }

    #[test]
    fn full_width_scalars() {
        let sig = der_to_p1363(&der(&[0x11; 32], &[0x22; 32])).unwrap();
        assert_eq!(&sig.as_bytes()[..32], &[0x11; 32]);
        assert_eq!(&sig.as_bytes()[32..], &[0x22; 32]);
    }

    #[test]
    fn leading_zero_for_high_bit_is_stripped() {
        let mut r = vec![0x00];
        r.extend_from_slice(&[0xff; 32]);
        let sig = der_to_p1363(&der(&r, &[0x01; 32])).unwrap();
        assert_eq!(&sig.as_bytes()[..32], &[0xff; 32]);
    }

    #[test]
    fn short_scalars_are_left_padded() {
        let sig = der_to_p1363(&der(&[0x01, 0x02], &[0x7f; 31])).unwrap();
        let mut want = [0u8; 64];
        want[30] = 0x01;
        want[31] = 0x02;
        want[33..].fill(0x7f);
        assert_eq!(sig.as_bytes(), &want);
    }

    #[test]
    fn long_form_length_is_accepted() {
        // 两个带前导 0x00 的 33 字节整数 → SEQUENCE 内容 70 字节，仍是短形式；
        // 构造 0x81 形式需内容 ≥ 128，P-256 不会出现，只验证非最短编码被拒。
        let mut bad = der(&[0x11; 32], &[0x22; 32]);
        let len = bad[1];
        bad.splice(1..2, [0x81, len]);
        assert_eq!(der_to_p1363(&bad), Err(DerError("长度不是最短编码")));
    }

    #[test]
    fn malformed_inputs_are_rejected() {
        let mut r33 = vec![0x01];
        r33.extend_from_slice(&[0; 32]);
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            der(&[0x80; 32], &[0x01; 32]),   // 负数
            der(&[0x00, 0x01], &[0x01; 32]), // 非最短
            der(&[0x00], &[0x01; 32]),       // 零
            der(&r33, &[0x01; 32]),          // 超过 32 字节
            der(&[], &[0x01; 32]),           // 空整数
            {
                let mut v = der(&[1; 32], &[1; 32]);
                v.push(0);
                v
            }, // 尾随字节
            {
                let mut v = der(&[1; 32], &[1; 32]);
                v[0] = 0x31;
                v
            }, // 标签
        ];
        for case in cases {
            assert!(der_to_p1363(&case).is_err(), "{case:02x?}");
        }
    }

    #[test]
    fn p1363_to_der_round_trips() {
        let mut cases = vec![[0x11u8; 64], [0xffu8; 64]];
        let mut short = [0u8; 64];
        short[31] = 1;
        short[63] = 0x80;
        cases.push(short);
        for sig in cases {
            let der = p1363_to_der(&sig);
            assert_eq!(der_to_p1363(&der).unwrap().as_bytes(), &sig);
        }
    }

    #[test]
    fn matches_p256_crate_conversion() {
        use p256::ecdsa::signature::Signer;
        use p256::ecdsa::{Signature, SigningKey};
        let key = SigningKey::from_slice(&[3u8; 32]).unwrap();
        for i in 0..64u8 {
            let sig: Signature = key.sign(&[i; 7]);
            let der = sig.to_der();
            assert_eq!(
                der_to_p1363(der.as_bytes()).unwrap().as_bytes()[..],
                sig.to_bytes()[..]
            );
        }
    }
}
