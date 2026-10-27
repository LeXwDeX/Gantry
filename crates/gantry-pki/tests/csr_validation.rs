//! pki.md §12.2「配对」中 CSR 与 `approval_pubkey` 的反例，以及"恶意 CSR 被受理但扩展被忽略"。
//!
//! 夹具在 `tests/fixtures/csr/`，由 openssl 生成（私钥未入库）。

use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_pki::csr::{
    encode_approval_pubkey, parse_approval_pubkey, validate_csr, ApprovalKeyError, CsrError,
};
use gantry_pki::{Authority, CertPolicy, CertSerial, KeyError};
use rcgen::{CertificateParams, KeyPair};
use x509_parser::prelude::{FromDer, X509Certificate};

const MAX: usize = 4096;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/csr/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn accepts_p256_csr_and_returns_its_public_key() {
    let key = validate_csr(&fixture("p256.csr"), MAX).unwrap();
    // 与同一私钥导出的未压缩点一致。
    let same = parse_approval_pubkey(fixture("approval_same_as_p256_csr.txt").trim()).unwrap();
    assert_eq!(key, same);
    assert_eq!(key.fingerprint(), same.fingerprint());
}

#[test]
fn rejects_non_p256_keys() {
    for name in ["p384.csr", "rsa2048.csr", "ed25519.csr"] {
        assert_eq!(
            validate_csr(&fixture(name), MAX),
            Err(CsrError::PublicKey(KeyError::NotP256)),
            "{name}"
        );
    }
}

#[test]
fn rejects_compressed_point_and_wrong_digest() {
    assert_eq!(
        validate_csr(&fixture("p256_compressed.csr"), MAX),
        Err(CsrError::PublicKey(KeyError::NotUncompressed))
    );
    assert_eq!(
        validate_csr(&fixture("p256_sha384.csr"), MAX),
        Err(CsrError::SignatureAlgorithm)
    );
}

#[test]
fn rejects_broken_signature() {
    let pem_text = fixture("p256.csr");
    let mut der = pem::parse(&pem_text).unwrap().into_contents();
    // 签名在 DER 末尾：翻转最后一个字节。
    let last = der.len() - 1;
    der[last] ^= 0x01;
    let tampered = pem::encode(&pem::Pem::new("CERTIFICATE REQUEST", der));
    assert_eq!(validate_csr(&tampered, MAX), Err(CsrError::Signature));

    // 改动被签名的部分（subject 里的一个字节）同样失败。
    let mut der = pem::parse(&pem_text).unwrap().into_contents();
    let pos = der.windows(8).position(|w| w == b"whatever").unwrap();
    der[pos] = b'W';
    let tampered = pem::encode(&pem::Pem::new("CERTIFICATE REQUEST", der));
    assert_eq!(validate_csr(&tampered, MAX), Err(CsrError::Signature));
}

#[test]
fn rejects_oversize_multi_block_and_trailing_garbage() {
    let good = fixture("p256.csr");
    assert_eq!(
        validate_csr(&good, good.len() - 1),
        Err(CsrError::TooLarge {
            limit: good.len() - 1
        })
    );
    let padded = format!("{good}{}", " ".repeat(MAX));
    assert!(matches!(
        validate_csr(&padded, MAX),
        Err(CsrError::TooLarge { .. })
    ));
    let two = format!("{good}{good}");
    assert_eq!(validate_csr(&two, MAX), Err(CsrError::Pem));
    let trailing = format!("{good}garbage");
    assert_eq!(validate_csr(&trailing, MAX), Err(CsrError::Pem));
    let leading = format!("garbage\n{good}");
    assert_eq!(validate_csr(&leading, MAX), Err(CsrError::Pem));
    let cert_tag = good.replace("CERTIFICATE REQUEST", "CERTIFICATE");
    assert_eq!(validate_csr(&cert_tag, MAX), Err(CsrError::Pem));
    assert_eq!(validate_csr("", MAX), Err(CsrError::Pem));

    // DER 后面跟着多余字节：PEM 合法但内容不是恰好一个 CSR。
    let mut der = pem::parse(&good).unwrap().into_contents();
    der.extend_from_slice(&[0, 0]);
    let extra = pem::encode(&pem::Pem::new("CERTIFICATE REQUEST", der));
    assert_eq!(validate_csr(&extra, MAX), Err(CsrError::Der));
    let junk = pem::encode(&pem::Pem::new("CERTIFICATE REQUEST", vec![1, 2, 3]));
    assert_eq!(validate_csr(&junk, MAX), Err(CsrError::Der));
}

#[test]
fn malicious_csr_is_accepted_but_requested_extensions_are_ignored() {
    // 请求了 CA:TRUE、keyCertSign、通配 SAN、serverAuth，subject 也是伪造的。
    let key = validate_csr(&fixture("malicious.csr"), MAX).unwrap();

    let ca_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let authority = Authority::load(&ca_cert.pem(), &ca_key.serialize_pem()).unwrap();

    let id: DeviceId = "dev_01J0000000000000000000000C".parse().unwrap();
    let serial = CertSerial::from_be_bytes(&[0x42]).unwrap();
    let now = Timestamp::from_millis(1_798_761_600_000);
    let issued = authority
        .issue_device_leaf(&id, &key, &serial, now, &CertPolicy::default())
        .unwrap();
    let (_, cert) = X509Certificate::from_der(&issued.der).unwrap();

    let bc = cert.basic_constraints().unwrap().unwrap();
    assert!(!bc.value.ca);
    let ku = cert.key_usage().unwrap().unwrap().value;
    assert!(ku.digital_signature() && !ku.key_cert_sign());
    let eku = cert.extended_key_usage().unwrap().unwrap().value;
    assert!(eku.client_auth && !eku.server_auth);
    assert!(cert.subject_alternative_name().unwrap().is_none());
    let cns: Vec<_> = cert
        .subject()
        .iter_common_name()
        .map(|cn| cn.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(cns, vec![id.to_string()]);
    assert_eq!(cert.subject().iter_organization().count(), 0);
}

#[test]
fn approval_pubkey_accepts_canonical_p256_point() {
    let text = fixture("approval.txt");
    let key = parse_approval_pubkey(text.trim()).unwrap();
    assert_eq!(encode_approval_pubkey(&key), text.trim());
    assert!(key.fingerprint().to_string().starts_with("sha256:"));
}

#[test]
fn approval_pubkey_rejections() {
    let good = fixture("approval.txt").trim().to_owned();

    // 非 P-256（P-384 的 97 字节点）。
    assert_eq!(
        parse_approval_pubkey(fixture("approval_p384.txt").trim()),
        Err(ApprovalKeyError::Key(KeyError::NotUncompressed))
    );
    // 压缩点：0x02/0x03 + X。
    let bytes = base64_decode(&good);
    let mut compressed = vec![0x02 | (bytes[64] & 1)];
    compressed.extend_from_slice(&bytes[1..33]);
    assert_eq!(
        parse_approval_pubkey(&base64_encode(&compressed)),
        Err(ApprovalKeyError::Key(KeyError::NotUncompressed))
    );
    // 不在曲线上。
    let mut off = bytes.clone();
    off[64] ^= 0x01;
    assert_eq!(
        parse_approval_pubkey(&base64_encode(&off)),
        Err(ApprovalKeyError::Key(KeyError::NotOnCurve))
    );
    // 非 base64url：标准 base64 字符、填充、空白、空串。
    let standard = good.replace('-', "+").replace('_', "/");
    if standard != good {
        assert_eq!(
            parse_approval_pubkey(&standard),
            Err(ApprovalKeyError::Encoding)
        );
    }
    assert_eq!(
        parse_approval_pubkey("AAA+"),
        Err(ApprovalKeyError::Encoding)
    );
    assert_eq!(
        parse_approval_pubkey(&format!("{good}=")),
        Err(ApprovalKeyError::Encoding)
    );
    assert_eq!(
        parse_approval_pubkey(&format!(" {good}")),
        Err(ApprovalKeyError::Encoding)
    );
    assert_eq!(
        parse_approval_pubkey(""),
        Err(ApprovalKeyError::Key(KeyError::NotUncompressed))
    );
    // 非规范尾随位：65 字节编码成 87 字符，最后一个字符只有 2 位有效。
    let mut noncanonical = good.clone().into_bytes();
    let last = noncanonical.len() - 1;
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let index = alphabet
        .iter()
        .position(|c| *c == noncanonical[last])
        .unwrap();
    noncanonical[last] = alphabet[index | 0x01];
    let noncanonical = String::from_utf8(noncanonical).unwrap();
    if noncanonical != good {
        assert_eq!(
            parse_approval_pubkey(&noncanonical),
            Err(ApprovalKeyError::Encoding)
        );
    }
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .unwrap()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
