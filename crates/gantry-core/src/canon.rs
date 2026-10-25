//! 严格摘要、RFC 8785 规范化、审批签名载荷与 Outbox 幂等键。

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use schemars::Schema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::entities::{ApprovalAction, ApprovalTarget, ForgeActionKind};
use crate::ids::{ApprovalId, DeliveryId, DeviceId, ProjectId};
use crate::time::Timestamp;

/// 审批签名字节的前缀与版本。
pub const APPROVAL_SIGNING_PREFIX: &[u8] = b"gantry/approval/v1\n";
pub const APPROVAL_PAYLOAD_VERSION: u32 = 1;
pub const OUTBOX_KEY_VERSION: &str = "gantry/outbox/v1";
pub const OUTBOX_KEY_PREFIX: &str = "gk1_";

/// SHA-256 摘要，文本形式 `sha256:` + 64 位小写十六进制。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        to_hex(&self.0)
    }

    pub fn parse(value: &str) -> Result<Self, DigestError> {
        let hex = value
            .strip_prefix("sha256:")
            .ok_or(DigestError::MissingPrefix)?;
        if hex.len() != 64 {
            return Err(DigestError::Length);
        }
        if hex.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(DigestError::Uppercase);
        }
        let mut bytes = [0u8; 32];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_digit(pair[0]).ok_or(DigestError::InvalidHex)?;
            let lo = hex_digit(pair[1]).ok_or(DigestError::InvalidHex)?;
            bytes[index] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.to_hex())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl FromStr for Sha256Digest {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Sha256Digest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Sha256Digest".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^sha256:[0-9a-f]{64}$",
        })
    }
}

/// Git 对象 ID，40 位小写十六进制。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitOid([u8; 20]);

impl GitOid {
    pub fn parse(value: &str) -> Result<Self, DigestError> {
        if value.len() != 40 {
            return Err(DigestError::Length);
        }
        if value.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(DigestError::Uppercase);
        }
        let mut bytes = [0u8; 20];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let hi = hex_digit(pair[0]).ok_or(DigestError::InvalidHex)?;
            let lo = hex_digit(pair[1]).ok_or(DigestError::InvalidHex)?;
            bytes[index] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&to_hex(&self.0))
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl FromStr for GitOid {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for GitOid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for GitOid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for GitOid {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "GitOid".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{40}$",
        })
    }
}

/// `owner/repo`。
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoSlug(String);

impl RepoSlug {
    pub fn parse(value: &str) -> Result<Self, SlugError> {
        let mut parts = value.split('/');
        let owner = parts.next().unwrap_or_default();
        let repo = parts.next().unwrap_or_default();
        if parts.next().is_some() || owner.is_empty() || repo.is_empty() {
            return Err(SlugError::Shape);
        }
        if owner.len() > 100 || repo.len() > 100 {
            return Err(SlugError::Length);
        }
        if !valid_slug_part(owner) || !valid_slug_part(repo) {
            return Err(SlugError::InvalidChar);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_slug_part(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl fmt::Display for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RepoSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoSlug").field(&self.0).finish()
    }
}

impl FromStr for RepoSlug {
    type Err = SlugError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for RepoSlug {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RepoSlug {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for RepoSlug {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "RepoSlug".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z0-9._-]{1,100}/[A-Za-z0-9._-]{1,100}$",
        })
    }
}

/// 审批 challenge 的 nonce（base64url 等不透明字符串）。
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Nonce(String);

impl Nonce {
    pub fn parse(value: &str) -> Result<Self, NonceError> {
        if !(16..=128).contains(&value.len()) {
            return Err(NonceError::Length);
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(NonceError::InvalidChar);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Nonce").field(&self.0).finish()
    }
}

impl FromStr for Nonce {
    type Err = NonceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Nonce {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Nonce {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Nonce {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Nonce".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^[A-Za-z0-9_-]{16,128}$",
        })
    }
}

/// 审批主体摘要：PR 类动作为 head SHA；其余为 `sha256:` 摘要。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubjectDigest {
    Git(GitOid),
    Sha256(Sha256Digest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectKind {
    GitOid,
    Sha256,
}

impl SubjectDigest {
    pub fn kind(self) -> SubjectKind {
        match self {
            Self::Git(_) => SubjectKind::GitOid,
            Self::Sha256(_) => SubjectKind::Sha256,
        }
    }

    pub fn git(oid: GitOid) -> Self {
        Self::Git(oid)
    }

    pub fn sha256(digest: Sha256Digest) -> Self {
        Self::Sha256(digest)
    }
}

impl fmt::Display for SubjectDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(oid) => oid.fmt(f),
            Self::Sha256(digest) => digest.fmt(f),
        }
    }
}

impl fmt::Debug for SubjectDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string())
    }
}

impl Serialize for SubjectDigest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SubjectDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if s.starts_with("sha256:") {
            Sha256Digest::parse(&s)
                .map(Self::Sha256)
                .map_err(serde::de::Error::custom)
        } else {
            GitOid::parse(&s)
                .map(Self::Git)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl JsonSchema for SubjectDigest {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SubjectDigest".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^(sha256:[0-9a-f]{64}|[0-9a-f]{40})$",
        })
    }
}

/// 审批签名载荷；`delivery_id` 无值时省略该键（不输出 `null`）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalPayload {
    pub action: ApprovalAction,
    pub approval_id: ApprovalId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<DeliveryId>,
    pub device_id: DeviceId,
    pub expires_at: Timestamp,
    pub nonce: Nonce,
    pub subject_digest: SubjectDigest,
    pub target: ApprovalTarget,
    pub v: u32,
}

impl ApprovalPayload {
    /// 校验版本、动作形状、target 与 subject 形式；失败时不产生签名字节。
    pub fn validate(&self) -> Result<(), CanonError> {
        if self.v != APPROVAL_PAYLOAD_VERSION {
            return Err(CanonError::UnsupportedVersion(self.v));
        }
        let shape = self
            .action
            .shape()
            .ok_or(CanonError::ApprovalShapeUndefined)?;
        if shape.target_kind != self.target.kind() {
            return Err(CanonError::ApprovalTargetMismatch);
        }
        if shape.subject_kind != self.subject_digest.kind() {
            return Err(CanonError::ApprovalSubjectMismatch);
        }
        Ok(())
    }

    /// `"gantry/approval/v1\n"` ‖ JCS(payload)。
    pub fn signing_bytes(&self) -> Result<Vec<u8>, CanonError> {
        self.validate()?;
        let canonical = serde_jcs::to_vec(self).map_err(|e| CanonError::Jcs(e.to_string()))?;
        let mut bytes = Vec::with_capacity(APPROVAL_SIGNING_PREFIX.len() + canonical.len());
        bytes.extend_from_slice(APPROVAL_SIGNING_PREFIX);
        bytes.extend_from_slice(&canonical);
        Ok(bytes)
    }
}

/// Outbox 幂等键：`gk1_` + 26 位小写 base32。
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// base32(first16(sha256(JCS(["gantry/outbox/v1", project, delivery, kind, target,
    /// hex(sha256(JCS(intent))), epoch]))))
    pub fn derive(
        project: &ProjectId,
        delivery: &DeliveryId,
        kind: ForgeActionKind,
        target: &str,
        intent: &Value,
        epoch: u32,
    ) -> Result<Self, CanonError> {
        let intent_digest = sha256(&jcs(intent)?).to_hex();
        let input = serde_json::json!([
            OUTBOX_KEY_VERSION,
            project.to_string(),
            delivery.to_string(),
            kind.as_str(),
            target,
            intent_digest,
            epoch
        ]);
        let digest = sha256(&jcs(&input)?);
        let encoded = base32_lower_nopad(&digest.as_bytes()[..16]);
        Ok(Self(format!("{OUTBOX_KEY_PREFIX}{encoded}")))
    }

    pub fn parse(value: &str) -> Result<Self, IdempotencyKeyError> {
        let body = value
            .strip_prefix(OUTBOX_KEY_PREFIX)
            .ok_or(IdempotencyKeyError::Prefix)?;
        if body.len() != 26 {
            return Err(IdempotencyKeyError::Length);
        }
        if !body.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7')) {
            return Err(IdempotencyKeyError::Alphabet);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IdempotencyKey").field(&self.0).finish()
    }
}

impl FromStr for IdempotencyKey {
    type Err = IdempotencyKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for IdempotencyKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for IdempotencyKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for IdempotencyKey {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "IdempotencyKey".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": "^gk1_[a-z2-7]{26}$",
        })
    }
}

pub fn sha256(bytes: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Sha256Digest(hasher.finalize().into())
}

/// RFC 8785 JSON 规范化。
pub fn jcs(value: &Value) -> Result<Vec<u8>, CanonError> {
    serde_jcs::to_vec(value).map_err(|e| CanonError::Jcs(e.to_string()))
}

pub fn jcs_digest(value: &Value) -> Result<Sha256Digest, CanonError> {
    Ok(sha256(&jcs(value)?))
}

#[derive(Debug, thiserror::Error)]
pub enum CanonError {
    #[error("JSON canonicalization failed: {0}")]
    Jcs(String),
    #[error("approval payload version {0} is not supported")]
    UnsupportedVersion(u32),
    #[error("approval action has no defined target/subject shape (fail-closed)")]
    ApprovalShapeUndefined,
    #[error("approval target kind does not match the action shape")]
    ApprovalTargetMismatch,
    #[error("approval subject digest kind does not match the action shape")]
    ApprovalSubjectMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("digest is missing the sha256: prefix")]
    MissingPrefix,
    #[error("digest has an invalid length")]
    Length,
    #[error("digest must use lowercase hexadecimal")]
    Uppercase,
    #[error("digest contains a non-hexadecimal character")]
    InvalidHex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SlugError {
    #[error("repo slug must be owner/repo")]
    Shape,
    #[error("repo slug part is too long")]
    Length,
    #[error("repo slug contains an invalid character")]
    InvalidChar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NonceError {
    #[error("nonce must be 16..=128 characters")]
    Length,
    #[error("nonce may only contain base64url characters")]
    InvalidChar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdempotencyKeyError {
    #[error("idempotency key must start with gk1_")]
    Prefix,
    #[error("idempotency key body must be 26 characters")]
    Length,
    #[error("idempotency key body must be lowercase base32")]
    Alphabet,
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn base32_lower_nopad(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | *byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_roundtrip() {
        let digest = sha256(b"abc");
        assert_eq!(
            digest.to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest.to_string().parse::<Sha256Digest>().unwrap(), digest);
    }

    #[test]
    fn idempotency_key_golden_vector() {
        let project = "prj_01JBA1C4M8XQ0Z5R7T2V9E3HNK".parse().unwrap();
        let delivery = "dlv_01JBA1D7P2KS6W8Y0A4C1F5GJM".parse().unwrap();
        let intent = serde_json::json!({
            "base": "main",
            "head": "gantry/42-fix-pagination",
            "title": "Fix pagination off-by-one in /orders",
            "body": "Fixes the off-by-one when page size divides total count.",
            "draft": false,
            "closes": [42],
        });
        let key = IdempotencyKey::derive(
            &project,
            &delivery,
            ForgeActionKind::CreatePr,
            "pr:gantry/42-fix-pagination->main",
            &intent,
            0,
        )
        .unwrap();
        assert_eq!(key.as_str(), "gk1_anbfwcip7r36is6wsqcuxxfd34");
    }
}
