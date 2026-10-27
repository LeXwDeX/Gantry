//! pki.md §12.2「配对」中与存储无关的部分：令牌、会话表、确认码、首台角色与引导期（D-18）。

use std::collections::HashSet;
use std::time::Duration;

use gantry_core::entities::{DevicePlatform, DeviceRole};
use gantry_core::ids::DeviceId;
use gantry_core::testing::SeqEntropy;
use gantry_core::time::Timestamp;
use gantry_pki::pairing::{
    bootstrap_period, confirmation_code, initial_role, new_token, token_hash, token_log_id,
    SessionError, SessionState, SessionTable, TokenInvalid, TokenInvalidReason, TOKEN_PREFIX,
};

const T0: i64 = 1_798_761_600_000;
const TTL: Duration = Duration::from_secs(600);
const MAX_FAILURES: u32 = 5;

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millis(T0 + ms)
}

fn device() -> DeviceId {
    "dev_01J0000000000000000000000A".parse().unwrap()
}

#[test]
fn token_shape_has_128_bits_and_is_stored_only_as_hash() {
    let entropy = SeqEntropy::new(1);
    let token = new_token(&entropy);
    let text = token.expose_secret();
    assert!(text.starts_with(TOKEN_PREFIX));
    // 16 字节 → 22 个 base64url 字符（无填充）。
    assert_eq!(text.len(), TOKEN_PREFIX.len() + 22);
    assert!(text[TOKEN_PREFIX.len()..]
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    let tokens: HashSet<String> = (0..256)
        .map(|_| new_token(&entropy).expose_secret().to_owned())
        .collect();
    assert_eq!(tokens.len(), 256);

    let mut table = SessionTable::new();
    let opened = table.open(at(0), TTL, 0, &entropy);
    let session = table.get(opened.id).unwrap();
    assert_eq!(
        session.token_sha256,
        token_hash(opened.token.expose_secret())
    );
    // 明文令牌不出现在表的 Debug 输出里，只有哈希前 8 位。
    let debug = format!("{table:?}");
    assert!(!debug.contains(opened.token.expose_secret()));
    assert!(debug.contains(&token_log_id(&session.token_sha256)));
    assert_eq!(token_log_id(&session.token_sha256).len(), 8);
    assert_eq!(format!("{:?}", opened.token), "SecretString(***)");
}

#[test]
fn token_is_single_use() {
    let entropy = SeqEntropy::new(2);
    let mut table = SessionTable::new();
    let opened = table.open(at(0), TTL, 0, &entropy);
    let id = table
        .check(opened.token.expose_secret(), at(1000), MAX_FAILURES)
        .unwrap();
    assert_eq!(id, opened.id);
    // 校验失败（如 CSR 不合格）不消耗：可以再次核验。
    assert_eq!(
        table.check(opened.token.expose_secret(), at(2000), MAX_FAILURES),
        Ok(opened.id)
    );
    table.redeem(id, device(), "012345".into()).unwrap();
    let session = table.get(id).unwrap();
    assert_eq!(session.state, SessionState::Redeemed);
    assert_eq!(session.redeemed_device_id, Some(device()));
    assert_eq!(session.confirmation_code.as_deref(), Some("012345"));
    assert!(table
        .check(opened.token.expose_secret(), at(3000), MAX_FAILURES)
        .is_err());
    assert_eq!(
        table.redeem(id, device(), "1".into()),
        Err(SessionError::NotOpen)
    );
    assert_eq!(table.window(at(3000)), None);
}

#[test]
fn token_expires_after_ttl() {
    let entropy = SeqEntropy::new(3);
    let mut table = SessionTable::new();
    let opened = table.open(at(0), TTL, 0, &entropy);
    assert!(table.window(at(599_999)).is_some());
    assert_eq!(
        table.check(opened.token.expose_secret(), at(600_000), MAX_FAILURES),
        Err(TokenInvalid {
            reason: TokenInvalidReason::Expired
        })
    );
    assert_eq!(table.get(opened.id).unwrap().state, SessionState::Expired);
    assert_eq!(table.window(at(600_000)), None);
}

#[test]
fn five_failures_void_the_open_session() {
    let entropy = SeqEntropy::new(4);
    let mut table = SessionTable::new();
    let opened = table.open(at(0), TTL, 0, &entropy);
    for attempt in 1..MAX_FAILURES {
        assert_eq!(
            table.check("gp1_wrong", at(attempt as i64), MAX_FAILURES),
            Err(TokenInvalid {
                reason: TokenInvalidReason::Mismatch
            })
        );
        assert_eq!(table.get(opened.id).unwrap().failed_attempts, attempt);
    }
    assert_eq!(
        table.check("gp1_wrong", at(10), MAX_FAILURES),
        Err(TokenInvalid {
            reason: TokenInvalidReason::Voided
        })
    );
    assert_eq!(table.get(opened.id).unwrap().state, SessionState::Voided);
    // 作废后合法令牌也失效。
    assert_eq!(
        table.check(opened.token.expose_secret(), at(11), MAX_FAILURES),
        Err(TokenInvalid {
            reason: TokenInvalidReason::NoSession
        })
    );
    assert_eq!(table.window(at(11)), None);
}

#[test]
fn new_session_voids_the_old_one() {
    let entropy = SeqEntropy::new(5);
    let mut table = SessionTable::new();
    let first = table.open(at(0), TTL, 0, &entropy);
    let second = table.open(at(1000), TTL, 0, &entropy);
    assert_eq!(second.voided, vec![first.id]);
    assert_eq!(table.get(first.id).unwrap().state, SessionState::Voided);
    assert!(table
        .check(first.token.expose_secret(), at(2000), MAX_FAILURES)
        .is_err());
    assert_eq!(
        table.check(second.token.expose_secret(), at(2000), MAX_FAILURES),
        Ok(second.id)
    );
    assert_eq!(table.window(at(2000)).unwrap().session_id, second.id);
}

#[test]
fn every_invalid_case_is_the_same_error_type() {
    // 无效 / 已用 / 过期 / 作废对外统一为 TokenInvalid；api 把它映射成同一个 401 响应。
    let entropy = SeqEntropy::new(6);
    let mut table = SessionTable::new();
    let none = table.check("gp1_x", at(0), MAX_FAILURES).unwrap_err();

    let used = table.open(at(0), TTL, 0, &entropy);
    let id = table
        .check(used.token.expose_secret(), at(1), MAX_FAILURES)
        .unwrap();
    table.redeem(id, device(), "000000".into()).unwrap();
    let reused = table
        .check(used.token.expose_secret(), at(2), MAX_FAILURES)
        .unwrap_err();

    let expiring = table.open(at(10), TTL, 0, &entropy);
    let expired = table
        .check(
            expiring.token.expose_secret(),
            at(10 + 600_000),
            MAX_FAILURES,
        )
        .unwrap_err();

    let _voiding = table.open(at(700_000), TTL, 0, &entropy);
    let mut voided = None;
    for i in 0..MAX_FAILURES {
        voided = table
            .check("gp1_bad", at(700_001 + i as i64), MAX_FAILURES)
            .err();
    }
    let voided = voided.unwrap();
    for error in [none, reused, expired, voided] {
        assert_eq!(error.to_string(), "pairing token invalid");
    }
}

#[test]
fn cancel_closes_the_window() {
    let entropy = SeqEntropy::new(7);
    let mut table = SessionTable::new();
    let opened = table.open(at(0), TTL, 0, &entropy);
    table.cancel(opened.id).unwrap();
    assert_eq!(table.window(at(1)), None);
    assert_eq!(table.get(opened.id).unwrap().state, SessionState::Voided);
    table.cancel(opened.id).unwrap();
    let missing = table.open(at(2), TTL, 0, &entropy).id;
    table.prune(at(10_000_000));
    assert!(table.get(opened.id).is_none(), "ended session pruned");
    assert!(table.get(missing).is_some(), "open session kept");
}

#[test]
fn confirmation_code_is_six_digits_and_spreads_over_the_range() {
    let entropy = SeqEntropy::new(8);
    let mut buckets = [0u32; 10];
    for _ in 0..2000 {
        let code = confirmation_code(&entropy);
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
        buckets[(code.as_bytes()[0] - b'0') as usize] += 1;
    }
    // 首位数字大致均匀：每个桶 200 左右，远离 0。
    assert!(buckets.iter().all(|&n| n > 100), "{buckets:?}");
}

#[test]
fn first_desktop_device_is_approver_and_phones_never_are() {
    use DevicePlatform::{Ios, Macos, Windows};
    // 库为空，第一台是桌面 → approver。
    assert_eq!(initial_role(&Macos, false), DeviceRole::Approver);
    assert_eq!(initial_role(&Windows, false), DeviceRole::Approver);
    // 之后的桌面设备 → operator（含"唯一桌面设备已被吊销"的情形，OQ-K11）。
    assert_eq!(initial_role(&Windows, true), DeviceRole::Operator);
    // 手机永远是 operator，即使是第一台（D-18、P-76）。
    assert_eq!(initial_role(&Ios, false), DeviceRole::Operator);
    assert_eq!(initial_role(&Ios, true), DeviceRole::Operator);
}

#[test]
fn bootstrap_period_lasts_until_the_first_desktop_device() {
    // 库为空 → 引导期；只配过手机 → 仍是引导期（D-18）；配过桌面设备（哪怕已吊销）→ 结束（P-66）。
    assert!(bootstrap_period(false));
    assert!(!bootstrap_period(true));
}

/// 按配对顺序折叠"是否有过桌面设备"，模拟同一写事务里读到的状态（D-18、OQ-K11）。
fn pair_sequence(platforms: &[DevicePlatform]) -> Vec<DeviceRole> {
    let mut desktop_ever_paired = false;
    platforms
        .iter()
        .map(|platform| {
            let role = initial_role(platform, desktop_ever_paired);
            if matches!(platform, DevicePlatform::Windows | DevicePlatform::Macos) {
                desktop_ever_paired = true;
            }
            role
        })
        .collect()
}

#[test]
fn phone_first_then_desktop_makes_the_desktop_approver() {
    use DevicePlatform::{Ios, Macos, Windows};
    use DeviceRole::{Approver, Operator};
    // 先配两台手机：都是 operator，引导期仍在持续。
    assert_eq!(pair_sequence(&[Ios, Ios]), vec![Operator, Operator]);
    assert!(bootstrap_period(false));
    // 再配桌面：第一台桌面设备成为 approver，之后的桌面与手机都是 operator。
    assert_eq!(
        pair_sequence(&[Ios, Macos, Windows, Ios]),
        vec![Operator, Approver, Operator, Operator]
    );
    assert!(!bootstrap_period(true));
    // 唯一的桌面 approver 被吊销后，"有过桌面设备"仍为真：新桌面设备是 operator，只能 recover --promote。
    assert_eq!(initial_role(&Windows, true), Operator);
}
