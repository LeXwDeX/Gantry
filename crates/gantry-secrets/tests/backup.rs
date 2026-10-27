//! 加密备份与恢复（secrets.md §12.1"加密备份"、§12.4）。

#![cfg(unix)]

mod common;

use common::{cfg, contains, ctx, fresh, name, sentinel, Key, TempDir};
use gantry_core::ports::secrets::{
    Expect, SecretSink, SecretSource, SecretValue, SecretVersion, SecretsError,
};
use gantry_core::secret::Consumer;
use gantry_secrets::{
    BackupEntryKind, BackupIdentity, ImportMode, ImportReport, SecretAdminOps, Secrets,
};
use sha2::{Digest, Sha256};

fn val(s: &str) -> SecretValue {
    SecretValue::from_bytes(s.as_bytes().to_vec())
}

fn v(n: u64) -> SecretVersion {
    SecretVersion::new(n).unwrap()
}

const BOTH: [Consumer; 2] = [Consumer::Forge, Consumer::Pki];

/// 源实例：forge 里两条（其中一条轮换过两次、保留了旧版本），一条已删除；pki 里一条；另有一条暂存项。
struct Source {
    _tmp: TempDir,
    secrets: Secrets,
    sentinels: Vec<String>,
}

fn source() -> Source {
    let (tmp, _key, secrets) = fresh(&BOTH);
    let forge = secrets.scoped(Consumer::Forge).unwrap();
    let pki = secrets.scoped(Consumer::Pki).unwrap();
    let c = ctx();
    let s: Vec<String> = (0..5).map(|_| sentinel()).collect();
    forge
        .put(&name("github.app.private_key"), val(&s[0]), Expect::Any, &c)
        .unwrap();
    forge
        .put(&name("github.app.private_key"), val(&s[1]), Expect::Any, &c)
        .unwrap();
    forge
        .put(&name("github.app.private_key"), val(&s[2]), Expect::Any, &c)
        .unwrap();
    forge
        .put(&name("github.webhook_secret"), val("w"), Expect::Any, &c)
        .unwrap();
    forge
        .put(&name("removed"), val("gone"), Expect::Any, &c)
        .unwrap();
    forge.delete(&name("removed"), Expect::Any, &c).unwrap();
    pki.put(&name("ca.intermediate.key"), val(&s[3]), Expect::Any, &c)
        .unwrap();
    forge
        .stage(&name("pending"), val(&s[4]), Expect::Any, &c)
        .unwrap();
    Source {
        _tmp: tmp,
        secrets,
        sentinels: s,
    }
}

fn export(secrets: &Secrets, ids: &[&BackupIdentity]) -> (Vec<u8>, gantry_secrets::BackupManifest) {
    let recipients: Vec<_> = ids.iter().map(|id| id.recipient()).collect();
    let mut out = Vec::new();
    let manifest = secrets
        .admin()
        .export_backup(&recipients, &mut out)
        .unwrap();
    (out, manifest)
}

/// 新实例：不同的主密钥、不同的目录。
fn target() -> (TempDir, Key, Secrets) {
    fresh(&BOTH)
}

fn files_in(secrets: &Secrets) -> usize {
    BOTH.iter()
        .map(|c| {
            std::fs::read_dir(secrets.dir().join(c.as_str()))
                .unwrap()
                .count()
        })
        .sum()
}

#[test]
fn export_contains_no_plaintext_and_restores_fresh_with_versions() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, manifest) = export(&src.secrets, &[&id]);

    for s in &src.sentinels {
        assert!(!contains(&archive, s.as_bytes()), "备份流里出现明文");
        assert!(!format!("{manifest:?}").contains(s.as_str()));
    }
    // 清单：只有当前版本与墓碑，不含暂存项与旧版本。
    let mut entries: Vec<_> = manifest
        .entries
        .iter()
        .map(|e| (e.consumer, e.name.as_str().to_owned(), e.version, e.kind))
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            (
                Consumer::Forge,
                "github.app.private_key".into(),
                3,
                BackupEntryKind::Secret
            ),
            (
                Consumer::Forge,
                "github.webhook_secret".into(),
                1,
                BackupEntryKind::Secret
            ),
            (
                Consumer::Forge,
                "removed".into(),
                2,
                BackupEntryKind::Tombstone
            ),
            (
                Consumer::Pki,
                "ca.intermediate.key".into(),
                1,
                BackupEntryKind::Secret
            ),
        ]
    );
    let digest: [u8; 32] = Sha256::digest(&archive).into();
    assert_eq!(manifest.ciphertext_sha256.as_bytes(), &digest);

    let (_tmp, _key, restored) = target();
    let report = restored
        .admin()
        .import_backup(&id, &mut archive.as_slice(), ImportMode::Fresh)
        .unwrap();
    assert_eq!(
        report,
        ImportReport {
            written: 4,
            skipped: 0
        }
    );

    let forge = restored.scoped(Consumer::Forge).unwrap();
    let pki = restored.reader(Consumer::Pki).unwrap();
    let key = forge.get(&name("github.app.private_key")).unwrap().unwrap();
    assert_eq!(
        (key.value.expose(), key.version),
        (src.sentinels[2].as_bytes(), v(3))
    );
    let webhook = forge.get(&name("github.webhook_secret")).unwrap().unwrap();
    assert_eq!((webhook.value.expose(), webhook.version), (&b"w"[..], v(1)));
    let ca = pki.get(&name("ca.intermediate.key")).unwrap().unwrap();
    assert_eq!(
        (ca.value.expose(), ca.version),
        (src.sentinels[3].as_bytes(), v(1))
    );
    assert!(!forge.present(&name("removed")).unwrap());
    assert!(restored.admin().staged_all().unwrap().is_empty());

    // 墓碑随备份恢复：再 put 不复用已用版本号。
    let again = forge
        .put(&name("removed"), val("new"), Expect::Absent, &ctx())
        .unwrap();
    assert_eq!(again.meta.version, v(3));
    // 恢复后的实例用本机主密钥加密：磁盘上也没有明文。
    for c in BOTH {
        for entry in std::fs::read_dir(restored.dir().join(c.as_str())).unwrap() {
            let data = std::fs::read(entry.unwrap().path()).unwrap();
            for s in &src.sentinels {
                assert!(!contains(&data, s.as_bytes()));
            }
        }
    }
}

#[test]
fn every_recipient_can_restore_and_wrong_identity_cannot() {
    let src = source();
    let a = BackupIdentity::generate();
    let b = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&a, &b]);
    for id in [&a, &b] {
        let (_tmp, _key, restored) = target();
        restored
            .admin()
            .import_backup(id, &mut archive.as_slice(), ImportMode::Fresh)
            .unwrap();
    }
    let (_tmp, _key, restored) = target();
    let wrong = BackupIdentity::generate();
    assert!(matches!(
        restored
            .admin()
            .import_backup(&wrong, &mut archive.as_slice(), ImportMode::Fresh),
        Err(SecretsError::KeyMismatch)
    ));
    assert_eq!(files_in(&restored), 0, "失败的恢复不写任何文件");
}

#[test]
fn truncated_or_tampered_archive_writes_nothing() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&id]);
    let len = archive.len();
    let mut cases: Vec<Vec<u8>> = vec![
        archive[..len - 1].to_vec(),
        archive[..len / 2].to_vec(),
        archive[..len - 17].to_vec(),
    ];
    for offset in [len / 3, len / 2, len - 20, len - 1] {
        let mut data = archive.clone();
        data[offset] ^= 0x01;
        cases.push(data);
    }
    for (i, data) in cases.iter().enumerate() {
        let (_tmp, _key, restored) = target();
        let result = restored
            .admin()
            .import_backup(&id, &mut data.as_slice(), ImportMode::Fresh);
        assert!(result.is_err(), "case {i} must fail");
        assert_eq!(files_in(&restored), 0, "case {i}: 不写任何文件");
    }
}

#[test]
fn fresh_requires_empty_instance() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&id]);
    let (_tmp, _key, restored) = target();
    restored
        .scoped(Consumer::Pki)
        .unwrap()
        .put(&name("local"), val("x"), Expect::Any, &ctx())
        .unwrap();
    assert!(restored
        .admin()
        .import_backup(&id, &mut archive.as_slice(), ImportMode::Fresh)
        .is_err());
    assert!(!restored
        .reader(Consumer::Forge)
        .unwrap()
        .present(&name("github.webhook_secret"))
        .unwrap());
}

#[test]
fn merge_only_writes_newer_versions() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&id]);
    let (_tmp, _key, restored) = target();
    let forge = restored.scoped(Consumer::Forge).unwrap();
    let c = ctx();
    // 本地 webhook 已到 v2（高于备份的 v1）；私钥只到 v1（低于备份的 v3）。
    forge
        .put(
            &name("github.webhook_secret"),
            val("local-1"),
            Expect::Any,
            &c,
        )
        .unwrap();
    forge
        .put(
            &name("github.webhook_secret"),
            val("local-2"),
            Expect::Any,
            &c,
        )
        .unwrap();
    forge
        .put(
            &name("github.app.private_key"),
            val("local-old"),
            Expect::Any,
            &c,
        )
        .unwrap();
    let report = restored
        .admin()
        .import_backup(&id, &mut archive.as_slice(), ImportMode::Merge)
        .unwrap();
    assert_eq!(
        report,
        ImportReport {
            written: 3,
            skipped: 1
        }
    );
    let webhook = forge.get(&name("github.webhook_secret")).unwrap().unwrap();
    assert_eq!(
        (webhook.value.expose(), webhook.version),
        (&b"local-2"[..], v(2))
    );
    let key = forge.get(&name("github.app.private_key")).unwrap().unwrap();
    assert_eq!(
        (key.value.expose(), key.version),
        (src.sentinels[2].as_bytes(), v(3))
    );
    // 重跑 Merge 可重入：全部跳过。
    let report = restored
        .admin()
        .import_backup(&id, &mut archive.as_slice(), ImportMode::Merge)
        .unwrap();
    assert_eq!(
        report,
        ImportReport {
            written: 0,
            skipped: 4
        }
    );
}

#[test]
fn overwrite_replaces_local_entries() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&id]);
    let (_tmp, _key, restored) = target();
    let forge = restored.scoped(Consumer::Forge).unwrap();
    let c = ctx();
    for i in 0..4 {
        forge
            .put(
                &name("github.webhook_secret"),
                val(&format!("local-{i}")),
                Expect::Any,
                &c,
            )
            .unwrap();
    }
    restored
        .admin()
        .import_backup(&id, &mut archive.as_slice(), ImportMode::Overwrite)
        .unwrap();
    let webhook = forge.get(&name("github.webhook_secret")).unwrap().unwrap();
    assert_eq!((webhook.value.expose(), webhook.version), (&b"w"[..], v(1)));
}

#[test]
fn consumer_not_hosted_is_denied_before_writing() {
    let src = source();
    let id = BackupIdentity::generate();
    let (archive, _) = export(&src.secrets, &[&id]);
    // 只承载 forge 的实例不能接收 pki 的条目。
    let tmp = TempDir::new("forge-only");
    let key = Key::random();
    let dir = tmp.path().join("inst");
    Secrets::init(cfg(&dir, &[Consumer::Forge], &key)).unwrap();
    let restored = Secrets::open(cfg(&dir, &[Consumer::Forge], &key)).unwrap();
    assert!(matches!(
        restored
            .admin()
            .import_backup(&id, &mut archive.as_slice(), ImportMode::Fresh),
        Err(SecretsError::Denied {
            who: Consumer::Pki,
            ..
        })
    ));
    assert_eq!(std::fs::read_dir(dir.join("forge")).unwrap().count(), 0);
}

#[test]
fn export_requires_a_recipient_and_identities_are_redacted() {
    let src = source();
    let mut out = Vec::new();
    assert!(src.secrets.admin().export_backup(&[], &mut out).is_err());
    let id = BackupIdentity::generate();
    assert_eq!(format!("{id:?}"), "BackupIdentity(***)");
    assert!(format!("{:?}", id.recipient()).starts_with("BackupRecipient(age1"));
    assert!(BackupIdentity::parse("not-a-key").is_err());
    assert!(gantry_secrets::BackupRecipient::parse("age1invalid").is_err());
}
