//! pki.md §7.6 / §12.2：`revocations.journal` 的追加、复制、读取与重放筛选（P-67、P-80）。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::entities::{Actor, ActorKind};
use gantry_core::ids::DeviceId;
use gantry_core::time::Timestamp;
use gantry_pki::journal::{JournalEntry, JournalError, RevocationJournal, JOURNAL_VERSION};
use gantry_pki::CertSerial;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "pki-journal-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(device: &str, at_ms: i64) -> JournalEntry {
    JournalEntry {
        v: JOURNAL_VERSION,
        device_id: device.parse::<DeviceId>().unwrap(),
        cert_serials: vec!["1a2b".parse::<CertSerial>().unwrap()],
        approval_pubkey_fpr: None,
        revoked_at: Timestamp::from_millis(at_ms),
        actor: Actor::new(ActorKind::Device, "dev_01J00000000000000000000001"),
        via: None,
        log_id: "01J00000000000000000000LOG".into(),
        log_epoch: 1,
    }
}

const A: &str = "dev_01J0000000000000000000000A";
const B: &str = "dev_01J0000000000000000000000B";

#[test]
fn append_writes_one_line_to_local_and_backup() {
    let dir = temp_dir("append");
    let journal =
        RevocationJournal::new(dir.join("revocations.journal"), dir.join("backup.journal"));
    journal.append(&entry(A, 1_000)).unwrap();
    journal.append(&entry(B, 2_000)).unwrap();
    for path in [journal.local_path(), journal.backup_path()] {
        let text = fs::read_to_string(path).unwrap();
        assert_eq!(text.lines().count(), 2, "{path:?}");
        assert!(text.ends_with('\n'));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    let first: serde_json::Value = serde_json::from_str(
        fs::read_to_string(journal.local_path())
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["v"], 1);
    assert_eq!(first["device_id"], A);
    assert_eq!(first["cert_serials"][0], "1a2b");
    assert!(first.get("via").is_none(), "via omitted when absent");
    assert_eq!(
        journal.read_all().unwrap(),
        vec![entry(A, 1_000), entry(B, 2_000)]
    );
}

#[test]
fn append_fails_when_backup_target_is_unwritable() {
    let dir = temp_dir("backup-fail");
    // 备份目标的父目录不存在：复制失败 → 整个追加失败（调用方据此不提交吊销）。
    let journal = RevocationJournal::new(
        dir.join("revocations.journal"),
        dir.join("missing").join("backup.journal"),
    );
    let err = journal.append(&entry(A, 1_000)).unwrap_err();
    assert!(matches!(err, JournalError::Append { .. }), "{err:?}");
    let local = RevocationJournal::new(dir.join("missing-local").join("j"), dir.join("b"));
    assert!(matches!(
        local.append(&entry(A, 1_000)).unwrap_err(),
        JournalError::Append { .. }
    ));
    assert!(
        !dir.join("b").exists(),
        "backup not written when local append fails"
    );
}

#[test]
fn read_all_unions_both_copies_and_dedupes_lines() {
    let dir = temp_dir("union");
    let local = dir.join("revocations.journal");
    let backup = dir.join("backup.journal");
    let journal = RevocationJournal::new(&local, &backup);
    journal.append(&entry(A, 1_000)).unwrap();
    // 模拟恢复：本地文件被备份里的旧版本覆盖之前，备份目标上多出一条（或反之）。
    let only_backup = RevocationJournal::new(dir.join("scratch"), &backup);
    only_backup.append(&entry(B, 2_000)).unwrap();
    let entries = journal.read_all().unwrap();
    assert_eq!(entries, vec![entry(A, 1_000), entry(B, 2_000)]);

    // 文件不存在视为空。
    let empty = RevocationJournal::new(dir.join("nope1"), dir.join("nope2"));
    assert!(empty.read_all().unwrap().is_empty());
}

#[test]
fn entries_after_filters_by_backup_time() {
    let dir = temp_dir("after");
    let journal = RevocationJournal::new(dir.join("j"), dir.join("b"));
    journal.append(&entry(A, 1_000)).unwrap();
    journal.append(&entry(B, 3_000)).unwrap();
    assert_eq!(
        journal
            .entries_after(Timestamp::from_millis(2_000))
            .unwrap(),
        vec![entry(B, 3_000)]
    );
    assert_eq!(
        journal
            .entries_after(Timestamp::from_millis(3_000))
            .unwrap(),
        vec![]
    );
}

#[test]
fn bad_lines_stop_the_read() {
    let dir = temp_dir("bad");
    let journal = RevocationJournal::new(dir.join("j"), dir.join("b"));
    journal.append(&entry(A, 1_000)).unwrap();
    let mut text = fs::read_to_string(dir.join("j")).unwrap();
    text.push_str("{not json}\n");
    fs::write(dir.join("j"), &text).unwrap();
    match journal.read_all().unwrap_err() {
        JournalError::BadLine { line, .. } => assert_eq!(line, 2),
        other => panic!("{other:?}"),
    }

    for bad in [
        r#"{"v":2,"device_id":"dev_01J0000000000000000000000A","cert_serials":["1"],"approval_pubkey_fpr":null,"revoked_at":"2027-01-01T00:00:00.000Z","actor":{"kind":"system","id":"pki"},"log_id":"x","log_epoch":1}"#,
        r#"{"v":1,"device_id":"dev_01J0000000000000000000000A","cert_serials":[],"approval_pubkey_fpr":null,"revoked_at":"2027-01-01T00:00:00.000Z","actor":{"kind":"system","id":"pki"},"log_id":"x","log_epoch":1}"#,
        r#"{"v":1,"device_id":"dev_01J0000000000000000000000A","cert_serials":["0abc"],"approval_pubkey_fpr":null,"revoked_at":"2027-01-01T00:00:00.000Z","actor":{"kind":"system","id":"pki"},"log_id":"x","log_epoch":1}"#,
        r#"{"v":1,"device_id":"dev_01J0000000000000000000000A","cert_serials":["1"],"approval_pubkey_fpr":null,"revoked_at":"2027-01-01T00:00:00.000Z","actor":{"kind":"system","id":"pki"},"log_id":"x","log_epoch":1,"extra":true}"#,
    ] {
        let dir = temp_dir("bad-one");
        fs::write(dir.join("j"), format!("{bad}\n")).unwrap();
        let journal = RevocationJournal::new(dir.join("j"), dir.join("b"));
        assert!(
            matches!(
                journal.read_all(),
                Err(JournalError::BadLine { line: 1, .. })
            ),
            "{bad}"
        );
    }
}
