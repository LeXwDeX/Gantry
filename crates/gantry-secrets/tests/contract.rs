//! 契约测试（secrets.md §12.2 / §12.4 / §12.5）：`FileSecrets`（本 crate）与
//! `MemorySecrets`（`gantry-fakes` 的 M0 替身）跑同一套用例，保证语义一致。

#![cfg(unix)]

mod common;

use common::{ctx, fresh, name};
use gantry_core::ports::secrets::{
    Expect, SecretSink, SecretValue, SecretVersion, SecretsError, StagedSecret,
};
use gantry_core::secret::Consumer;
use gantry_core::time::Timestamp;
use gantry_fakes::secrets::{secrets_conformance, MemorySecretStore};

/// 一个实现的实例：给出 `Forge` 与 `Pki` 两个命名空间的读写句柄。
trait Harness {
    fn sink(&self, who: Consumer) -> Box<dyn SecretSink>;
}

struct FileHarness {
    _tmp: common::TempDir,
    secrets: gantry_secrets::Secrets,
}

impl Harness for FileHarness {
    fn sink(&self, who: Consumer) -> Box<dyn SecretSink> {
        Box::new(self.secrets.scoped(who).unwrap())
    }
}

fn file() -> Box<dyn Harness> {
    let (tmp, _key, secrets) = fresh(&[Consumer::Forge, Consumer::Pki]);
    Box::new(FileHarness { _tmp: tmp, secrets })
}

struct MemoryHarness(MemorySecretStore);

impl Harness for MemoryHarness {
    fn sink(&self, who: Consumer) -> Box<dyn SecretSink> {
        Box::new(self.0.scoped(who).unwrap())
    }
}

fn memory() -> Box<dyn Harness> {
    let store = MemorySecretStore::new(Timestamp::from_millis(1_700_000_000_000));
    store.allow(Consumer::Forge);
    store.allow(Consumer::Pki);
    Box::new(MemoryHarness(store))
}

fn val(s: &str) -> SecretValue {
    SecretValue::from_bytes(s.as_bytes().to_vec())
}

fn v(n: u64) -> SecretVersion {
    SecretVersion::new(n).unwrap()
}

fn read(sink: &dyn SecretSink, n: &str) -> Option<(Vec<u8>, u64)> {
    sink.get(&name(n))
        .unwrap()
        .map(|s| (s.value.expose().to_vec(), s.version.get()))
}

// ---- 用例 ------------------------------------------------------------------

fn round_trip_and_versions(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("github.app.private_key");
    assert_eq!(s.get(&n).unwrap().map(|x| x.version), None);
    assert!(!s.present(&n).unwrap());
    let first = s.put(&n, val("one"), Expect::Any, &c).unwrap();
    assert_eq!(first.meta.version, v(1));
    assert_eq!(first.meta.consumer, Consumer::Forge);
    assert_eq!(first.previous, None, "首次写入 = set");
    assert_eq!(
        read(&*s, "github.app.private_key"),
        Some((b"one".to_vec(), 1))
    );
    assert!(s.present(&n).unwrap());
    assert_eq!(s.version(&n).unwrap(), Some(v(1)));
    let second = s.put(&n, val("two"), Expect::Any, &c).unwrap();
    assert_eq!(second.meta.version, v(2));
    assert_eq!(second.previous, Some(v(1)), "覆盖写 = rotate");
    assert_eq!(
        read(&*s, "github.app.private_key"),
        Some((b"two".to_vec(), 2))
    );
    assert_eq!(
        s.present(&n).unwrap(),
        s.get(&n).unwrap().is_some(),
        "present 与 get().is_some() 一致"
    );
    let list = s.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].version, v(2));
}

fn expect_conflicts(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("k");
    s.put(&n, val("a"), Expect::Absent, &c).unwrap();
    assert!(matches!(
        s.put(&n, val("b"), Expect::Absent, &c),
        Err(SecretsError::Conflict { actual: Some(a), .. }) if a == v(1)
    ));
    assert!(matches!(
        s.put(&n, val("b"), Expect::Version(v(7)), &c),
        Err(SecretsError::Conflict { .. })
    ));
    s.put(&n, val("b"), Expect::Version(v(1)), &c).unwrap();
    assert!(matches!(
        s.delete(&n, Expect::Version(v(1)), &c),
        Err(SecretsError::Conflict { .. })
    ));
    assert!(matches!(
        s.delete(&name("never"), Expect::Version(v(1)), &c),
        Err(SecretsError::NotFound)
    ));
    assert_eq!(s.delete(&name("never"), Expect::Any, &c).unwrap(), None);
}

fn tombstone_keeps_versions(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("model.key");
    s.put(&n, val("a"), Expect::Any, &c).unwrap();
    s.put(&n, val("b"), Expect::Any, &c).unwrap();
    let deleted = s.delete(&n, Expect::Version(v(2)), &c).unwrap().unwrap();
    assert_eq!(deleted.version, v(2));
    assert_eq!(s.get(&n).unwrap().map(|x| x.version), None);
    assert!(!s.present(&n).unwrap());
    assert_eq!(s.version(&n).unwrap(), None);
    assert!(s.list().unwrap().is_empty());
    assert_eq!(s.get_version(&n, v(2)).unwrap().map(|x| x.version), None);
    let again = s.put(&n, val("c"), Expect::Absent, &c).unwrap();
    assert!(again.meta.version > v(2), "删除后版本号不回退");
    assert_eq!(again.previous, None, "删除后再 put 的 previous 为 None");
}

fn stage_commit_abort(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("webhook_secret");
    s.put(&n, val("old"), Expect::Any, &c).unwrap();
    let staged = s.stage(&n, val("new"), Expect::Version(v(1)), &c).unwrap();
    assert_eq!(staged.version, v(2));
    assert_eq!(staged.consumer, Consumer::Forge);
    // 暂存版本对读者不可见。
    assert_eq!(read(&*s, "webhook_secret"), Some((b"old".to_vec(), 1)));
    assert_eq!(s.version(&n).unwrap(), Some(v(1)));
    assert_eq!(s.list().unwrap()[0].version, v(1));
    assert_eq!(s.get_version(&n, v(2)).unwrap().map(|x| x.version), None);
    assert!(matches!(
        s.stage(&n, val("other"), Expect::Any, &c),
        Err(SecretsError::AlreadyStaged)
    ));
    assert_eq!(s.staged().unwrap(), vec![staged.clone()]);
    s.abort(&staged, &c).unwrap();
    assert!(s.staged().unwrap().is_empty());
    assert!(matches!(s.abort(&staged, &c), Err(SecretsError::NotStaged)));
    assert!(matches!(
        s.commit(&staged, &c),
        Err(SecretsError::NotStaged)
    ));
    assert_eq!(read(&*s, "webhook_secret"), Some((b"old".to_vec(), 1)));

    let staged = s.stage(&n, val("new"), Expect::Any, &c).unwrap();
    let meta = s.commit(&staged, &c).unwrap();
    assert_eq!(meta.version, staged.version);
    assert_eq!(
        read(&*s, "webhook_secret"),
        Some((b"new".to_vec(), staged.version.get()))
    );
    assert!(s.staged().unwrap().is_empty());

    // 首次写入也可以走两阶段。
    let fresh_name = name("fresh");
    assert!(!s.present(&fresh_name).unwrap());
    let staged = s.stage(&fresh_name, val("x"), Expect::Absent, &c).unwrap();
    assert!(!s.present(&fresh_name).unwrap());
    assert_eq!(s.commit(&staged, &c).unwrap().version, v(1));
}

fn staged_from_other_consumer_is_denied(h: Box<dyn Harness>) {
    let forge = h.sink(Consumer::Forge);
    let pki = h.sink(Consumer::Pki);
    let c = ctx();
    let staged = forge.stage(&name("k"), val("x"), Expect::Any, &c).unwrap();
    assert!(matches!(
        pki.commit(&staged, &c),
        Err(SecretsError::Denied { .. })
    ));
    assert!(matches!(
        pki.abort(&staged, &c),
        Err(SecretsError::Denied { .. })
    ));
    assert!(pki.staged().unwrap().is_empty());
    let forged = StagedSecret {
        consumer: Consumer::Pki,
        ..staged.clone()
    };
    assert!(matches!(
        pki.commit(&forged, &c),
        Err(SecretsError::NotStaged)
    ));
    forge.commit(&staged, &c).unwrap();
}

fn namespaces_are_isolated(h: Box<dyn Harness>) {
    let forge = h.sink(Consumer::Forge);
    let pki = h.sink(Consumer::Pki);
    let c = ctx();
    pki.put(
        &name("ca.intermediate.key"),
        val("pki-only"),
        Expect::Any,
        &c,
    )
    .unwrap();
    assert_eq!(read(&*forge, "ca.intermediate.key"), None);
    assert!(!forge.present(&name("ca.intermediate.key")).unwrap());
    assert!(forge.list().unwrap().is_empty());
    forge
        .put(
            &name("ca.intermediate.key"),
            val("forge-own"),
            Expect::Absent,
            &c,
        )
        .unwrap();
    assert_eq!(
        read(&*pki, "ca.intermediate.key"),
        Some((b"pki-only".to_vec(), 1))
    );
    assert_eq!(
        read(&*forge, "ca.intermediate.key"),
        Some((b"forge-own".to_vec(), 1))
    );
}

fn retains_two_versions(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("rotating");
    for value in ["a", "b", "c"] {
        s.put(&n, val(value), Expect::Any, &c).unwrap();
    }
    assert_eq!(s.get_version(&n, v(1)).unwrap().map(|x| x.version), None);
    let old = s.get_version(&n, v(2)).unwrap().unwrap();
    assert_eq!(old.value.expose(), b"b");
    assert_eq!(
        s.get_version(&n, v(3)).unwrap().unwrap().value.expose(),
        b"c"
    );
}

fn check_refs_reports_missing(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    s.put(&name("present"), val("x"), Expect::Any, &c).unwrap();
    s.put(&name("deleted"), val("x"), Expect::Any, &c).unwrap();
    s.delete(&name("deleted"), Expect::Any, &c).unwrap();
    s.stage(&name("staged-only"), val("x"), Expect::Any, &c)
        .unwrap();
    let refs = [
        name("present"),
        name("deleted"),
        name("staged-only"),
        name("absent"),
    ];
    assert_eq!(
        s.check_refs(&refs).unwrap(),
        vec![name("deleted"), name("staged-only"), name("absent")]
    );
}

/// 规格 §7.4：墓碑占用 `c + 1`，之后 `put` 从 `c + 2` 继续。
fn tombstone_takes_its_own_version(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("k");
    s.put(&n, val("a"), Expect::Any, &c).unwrap();
    s.put(&n, val("b"), Expect::Any, &c).unwrap();
    s.delete(&n, Expect::Any, &c).unwrap();
    assert_eq!(
        s.put(&n, val("c"), Expect::Absent, &c)
            .unwrap()
            .meta
            .version,
        v(4)
    );
}

/// OQ-6 已决：空值 → `EmptyValue`，不留任何痕迹；任意二进制字节照常往返。
fn empty_value_is_rejected(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let c = ctx();
    let n = name("empty");
    let empty = || SecretValue::from_bytes(Vec::new());
    assert!(matches!(
        s.put(&n, empty(), Expect::Any, &c),
        Err(SecretsError::EmptyValue)
    ));
    assert!(matches!(
        s.stage(&n, empty(), Expect::Any, &c),
        Err(SecretsError::EmptyValue)
    ));
    assert!(!s.present(&n).unwrap());
    assert!(s.staged().unwrap().is_empty());
    s.put(&n, val("x"), Expect::Any, &c).unwrap();
    assert!(matches!(
        s.put(&n, empty(), Expect::Any, &c),
        Err(SecretsError::EmptyValue)
    ));
    assert_eq!(s.version(&n).unwrap(), Some(v(1)));
}

fn binary_values_round_trip(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    let binary: Vec<u8> = (0..=255).collect();
    s.put(
        &name("bin"),
        SecretValue::from_bytes(binary.clone()),
        Expect::Any,
        &ctx(),
    )
    .unwrap();
    assert_eq!(
        s.get(&name("bin")).unwrap().unwrap().value.expose(),
        &binary[..]
    );
}

/// `gantry-fakes` 自带的一致性用例；两种实现都要通过。
fn fakes_conformance(h: Box<dyn Harness>) {
    let s = h.sink(Consumer::Forge);
    secrets_conformance(&*s, &name("conformance"), &ctx()).unwrap();
}

macro_rules! secrets_contract_tests {
    ($($case:ident),* $(,)?) => {
        mod file_impl {
            $( #[test] fn $case() { super::$case(super::file()); } )*
        }
        mod memory_impl {
            $( #[test] fn $case() { super::$case(super::memory()); } )*
        }
    };
}

secrets_contract_tests!(
    round_trip_and_versions,
    expect_conflicts,
    tombstone_keeps_versions,
    stage_commit_abort,
    staged_from_other_consumer_is_denied,
    namespaces_are_isolated,
    retains_two_versions,
    check_refs_reports_missing,
    tombstone_takes_its_own_version,
    empty_value_is_rejected,
    binary_values_round_trip,
    fakes_conformance,
);
