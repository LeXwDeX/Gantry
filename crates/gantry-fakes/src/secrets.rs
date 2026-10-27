//! `MemorySecrets`：按使用方隔离的内存 `SecretSource` + `SecretSink`。
//!
//! 与真实文件实现保持同样的可观察语义：版本从 1 起、`expect` 比较并写入、
//! `stage` 对读者不可见、`commit` 才生效、`abort` 无痕、删除留墓碑（墓碑占用
//! `c + 1`，之后 `put` 从 `c + 2` 继续，secrets.md §7.4），空值被拒
//! （`EmptyValue`），`check_refs` 把墓碑与暂存视为缺失。
//!
//! 值只在 `SecretValue` 中传递；`Debug` 不打印值。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use gantry_core::ports::secrets::{
    Expect, PutOutcome, Secret, SecretMeta, SecretSink, SecretSource, SecretValue, SecretVersion,
    SecretsError, StagedSecret,
};
use gantry_core::ports::ActorCtx;
use gantry_core::secret::{Consumer, SecretName};
use gantry_core::time::Timestamp;

const RETAIN_VERSIONS: usize = 2;
const DENIED_NAME: &str = "instance";

// ---------------------------------------------------------------------------
// 状态
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SecretRecord {
    version: u64,
    value: Vec<u8>,
}

#[derive(Clone)]
struct Entry {
    versions: Vec<SecretRecord>,
    max_version: u64,
    tombstoned: bool,
    updated_at: Timestamp,
}

impl Entry {
    fn visible(&self) -> Option<&SecretRecord> {
        if self.tombstoned {
            return None;
        }
        self.versions.iter().max_by_key(|record| record.version)
    }

    fn current(&self) -> Option<u64> {
        self.visible().map(|record| record.version)
    }

    fn trim(&mut self) {
        self.versions.sort_by_key(|record| record.version);
        while self.versions.len() > RETAIN_VERSIONS {
            self.versions.remove(0);
        }
    }

    fn meta(&self, consumer: Consumer, name: &SecretName) -> Option<SecretMeta> {
        let record = self.visible()?;
        Some(SecretMeta {
            consumer,
            name: name.clone(),
            version: version(record.version),
            updated_at: self.updated_at,
        })
    }
}

#[derive(Clone)]
struct Staged {
    version: u64,
    value: Vec<u8>,
}

struct SecretsState {
    now: Timestamp,
    consumers: BTreeSet<Consumer>,
    entries: BTreeMap<(Consumer, SecretName), Entry>,
    staged: BTreeMap<(Consumer, SecretName), Staged>,
}

impl SecretsState {
    fn new(now: Timestamp) -> Self {
        Self {
            now,
            consumers: BTreeSet::new(),
            entries: BTreeMap::new(),
            staged: BTreeMap::new(),
        }
    }

    fn expect_ok(&self, key: &(Consumer, SecretName), expect: &Expect) -> Option<u64> {
        let current = self.entries.get(key).and_then(|entry| entry.current());
        match expect {
            Expect::Any => Some(current.unwrap_or(0)),
            Expect::Absent if current.is_none() => Some(0),
            Expect::Version(want) if current == Some(want.get()) => Some(want.get()),
            _ => None,
        }
    }

    fn visible(&self, key: &(Consumer, SecretName)) -> Option<&Entry> {
        self.entries
            .get(key)
            .filter(|entry| entry.visible().is_some())
    }
}

fn version(value: u64) -> SecretVersion {
    SecretVersion::new(value).expect("secret versions start at 1")
}

fn denied(who: Consumer) -> SecretsError {
    SecretsError::Denied {
        who,
        name: SecretName::parse(DENIED_NAME).expect("static denied name must be valid"),
    }
}

// ---------------------------------------------------------------------------
// 共享存储
// ---------------------------------------------------------------------------

pub struct MemorySecretStore {
    state: Arc<Mutex<SecretsState>>,
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f.debug_struct("MemorySecretStore")
            .field("consumers", &state.consumers)
            .field("entries", &state.entries.len())
            .finish()
    }
}

impl MemorySecretStore {
    pub fn new(now: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(SecretsState::new(now))),
        }
    }

    pub fn allow(&self, who: Consumer) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .consumers
            .insert(who);
    }

    pub fn set_now(&self, now: Timestamp) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .now = now;
    }

    pub fn advance_ms(&self, ms: i64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.now = Timestamp::from_millis(state.now.as_millis().saturating_add(ms));
    }

    pub fn reader(&self, who: Consumer) -> Result<MemorySecretReader, SecretsError> {
        if !self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .consumers
            .contains(&who)
        {
            return Err(denied(who));
        }
        Ok(MemorySecretReader {
            who,
            state: Arc::clone(&self.state),
        })
    }

    pub fn scoped(&self, who: Consumer) -> Result<MemorySecrets, SecretsError> {
        if !self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .consumers
            .contains(&who)
        {
            return Err(denied(who));
        }
        Ok(MemorySecrets {
            who,
            state: Arc::clone(&self.state),
        })
    }
}

// ---------------------------------------------------------------------------
// 只读句柄
// ---------------------------------------------------------------------------

pub struct MemorySecretReader {
    who: Consumer,
    state: Arc<Mutex<SecretsState>>,
}

impl fmt::Debug for MemorySecretReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecretReader")
            .field("consumer", &self.who)
            .finish()
    }
}

impl MemorySecretReader {
    fn lock(&self) -> std::sync::MutexGuard<'_, SecretsState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SecretSource for MemorySecretReader {
    fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError> {
        read_secret(&self.lock(), self.who, name)
    }

    fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError> {
        read_version(&self.lock(), self.who, name)
    }

    fn present(&self, name: &SecretName) -> Result<bool, SecretsError> {
        Ok(self.get(name)?.is_some())
    }

    fn get_version(
        &self,
        name: &SecretName,
        want: SecretVersion,
    ) -> Result<Option<Secret>, SecretsError> {
        read_version_value(&self.lock(), self.who, name, want)
    }
}

// ---------------------------------------------------------------------------
// 读写句柄
// ---------------------------------------------------------------------------

pub struct MemorySecrets {
    who: Consumer,
    state: Arc<Mutex<SecretsState>>,
}

impl fmt::Debug for MemorySecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySecrets")
            .field("consumer", &self.who)
            .finish()
    }
}

impl MemorySecrets {
    fn lock(&self) -> std::sync::MutexGuard<'_, SecretsState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn key(&self, name: &SecretName) -> (Consumer, SecretName) {
        (self.who, name.clone())
    }
}

impl SecretSource for MemorySecrets {
    fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError> {
        read_secret(&self.lock(), self.who, name)
    }

    fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError> {
        read_version(&self.lock(), self.who, name)
    }

    fn present(&self, name: &SecretName) -> Result<bool, SecretsError> {
        Ok(self.get(name)?.is_some())
    }

    fn get_version(
        &self,
        name: &SecretName,
        want: SecretVersion,
    ) -> Result<Option<Secret>, SecretsError> {
        read_version_value(&self.lock(), self.who, name, want)
    }
}

impl SecretSink for MemorySecrets {
    fn put(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<PutOutcome, SecretsError> {
        if value.expose().is_empty() {
            return Err(SecretsError::EmptyValue);
        }
        let mut state = self.lock();
        let key = self.key(name);
        let current = state.entries.get(&key).and_then(|entry| entry.current());
        if state.expect_ok(&key, &expect).is_none() {
            return Err(SecretsError::Conflict {
                expected: expect,
                actual: current.map(version),
            });
        }
        let now = state.now;
        let next = state
            .entries
            .get(&key)
            .map(|entry| entry.max_version)
            .unwrap_or(0)
            + 1;
        let entry = state.entries.entry(key).or_insert(Entry {
            versions: Vec::new(),
            max_version: 0,
            tombstoned: false,
            updated_at: now,
        });
        entry.max_version = next;
        entry.tombstoned = false;
        entry.updated_at = now;
        entry.versions.push(SecretRecord {
            version: next,
            value: value.expose().to_vec(),
        });
        entry.trim();
        let meta = entry
            .meta(self.who, name)
            .expect("a just-written secret must be visible");
        Ok(PutOutcome {
            meta,
            previous: current.map(version),
        })
    }

    fn stage(
        &self,
        name: &SecretName,
        value: SecretValue,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<StagedSecret, SecretsError> {
        if value.expose().is_empty() {
            return Err(SecretsError::EmptyValue);
        }
        let mut state = self.lock();
        let key = self.key(name);
        if state.staged.contains_key(&key) {
            return Err(SecretsError::AlreadyStaged);
        }
        let current = state.entries.get(&key).and_then(|entry| entry.current());
        if state.expect_ok(&key, &expect).is_none() {
            return Err(SecretsError::Conflict {
                expected: expect,
                actual: current.map(version),
            });
        }
        let next = state
            .entries
            .get(&key)
            .map(|entry| entry.max_version)
            .unwrap_or(0)
            + 1;
        state.staged.insert(
            key,
            Staged {
                version: next,
                value: value.expose().to_vec(),
            },
        );
        Ok(StagedSecret {
            consumer: self.who,
            name: name.clone(),
            version: version(next),
        })
    }

    fn commit(&self, staged: &StagedSecret, _ctx: &ActorCtx) -> Result<SecretMeta, SecretsError> {
        let mut state = self.lock();
        if staged.consumer != self.who {
            return Err(denied(staged.consumer));
        }
        let key = self.key(&staged.name);
        let pending = state.staged.remove(&key).ok_or(SecretsError::NotStaged)?;
        let expected = state
            .entries
            .get(&key)
            .map(|entry| entry.max_version)
            .unwrap_or(0)
            + 1;
        if pending.version != expected {
            return Err(SecretsError::Conflict {
                expected: Expect::Version(version(pending.version)),
                actual: state
                    .entries
                    .get(&key)
                    .and_then(|entry| entry.current())
                    .map(version),
            });
        }
        let now = state.now;
        let entry = state.entries.entry(key).or_insert(Entry {
            versions: Vec::new(),
            max_version: 0,
            tombstoned: false,
            updated_at: now,
        });
        entry.max_version = pending.version;
        entry.tombstoned = false;
        entry.updated_at = now;
        entry.versions.push(SecretRecord {
            version: pending.version,
            value: pending.value,
        });
        entry.trim();
        entry
            .meta(self.who, &staged.name)
            .ok_or(SecretsError::NotStaged)
    }

    fn abort(&self, staged: &StagedSecret, _ctx: &ActorCtx) -> Result<(), SecretsError> {
        let mut state = self.lock();
        if staged.consumer != self.who {
            return Err(denied(staged.consumer));
        }
        state
            .staged
            .remove(&self.key(&staged.name))
            .map(|_| ())
            .ok_or(SecretsError::NotStaged)
    }

    fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError> {
        let state = self.lock();
        Ok(state
            .staged
            .iter()
            .filter(|((consumer, _), _)| *consumer == self.who)
            .map(|((consumer, name), staged)| StagedSecret {
                consumer: *consumer,
                name: name.clone(),
                version: version(staged.version),
            })
            .collect())
    }

    fn delete(
        &self,
        name: &SecretName,
        expect: Expect,
        _ctx: &ActorCtx,
    ) -> Result<Option<SecretMeta>, SecretsError> {
        let mut state = self.lock();
        let key = self.key(name);
        let current = state.entries.get(&key).and_then(|entry| entry.current());
        if state.expect_ok(&key, &expect).is_none() {
            if !state.entries.contains_key(&key) && !matches!(&expect, Expect::Any) {
                return Err(SecretsError::NotFound);
            }
            return Err(SecretsError::Conflict {
                expected: expect,
                actual: current.map(version),
            });
        }
        let now = state.now;
        let Some(entry) = state.entries.get_mut(&key) else {
            return Ok(None);
        };
        let previous = entry.meta(self.who, name);
        entry.versions.clear();
        entry.max_version += 1;
        entry.tombstoned = true;
        entry.updated_at = now;
        Ok(previous)
    }

    fn list(&self) -> Result<Vec<SecretMeta>, SecretsError> {
        let state = self.lock();
        Ok(state
            .entries
            .iter()
            .filter(|((consumer, _), entry)| *consumer == self.who && entry.visible().is_some())
            .filter_map(|((_, name), entry)| entry.meta(self.who, name))
            .collect())
    }

    fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError> {
        let state = self.lock();
        Ok(refs
            .iter()
            .filter(|name| state.visible(&self.key(name)).is_none())
            .cloned()
            .collect())
    }
}

// ---------------------------------------------------------------------------
// 读辅助
// ---------------------------------------------------------------------------

fn read_secret(
    state: &SecretsState,
    who: Consumer,
    name: &SecretName,
) -> Result<Option<Secret>, SecretsError> {
    let Some(entry) = state.visible(&(who, name.clone())) else {
        return Ok(None);
    };
    let record = entry.visible().expect("visible entry has a visible record");
    Ok(Some(Secret {
        value: SecretValue::from_bytes(record.value.clone()),
        version: version(record.version),
    }))
}

fn read_version(
    state: &SecretsState,
    who: Consumer,
    name: &SecretName,
) -> Result<Option<SecretVersion>, SecretsError> {
    Ok(state
        .visible(&(who, name.clone()))
        .and_then(|entry| entry.current())
        .map(version))
}

fn read_version_value(
    state: &SecretsState,
    who: Consumer,
    name: &SecretName,
    want: SecretVersion,
) -> Result<Option<Secret>, SecretsError> {
    let Some(entry) = state.visible(&(who, name.clone())) else {
        return Ok(None);
    };
    Ok(entry
        .versions
        .iter()
        .find(|record| record.version == want.get())
        .map(|record| Secret {
            value: SecretValue::from_bytes(record.value.clone()),
            version: version(record.version),
        }))
}

// ---------------------------------------------------------------------------
// 一致性用例
// ---------------------------------------------------------------------------

/// 版本、`expect`、暂存、墓碑与缺失引用；真实 `SecretSink` 实现也应通过。
///
/// 前置条件：`name` 在 `sink` 的命名空间内不存在，且没有暂存项。
pub fn secrets_conformance(
    sink: &dyn SecretSink,
    name: &SecretName,
    ctx: &ActorCtx,
) -> Result<(), String> {
    let first = sink
        .put(
            name,
            SecretValue::from_bytes(b"one".to_vec()),
            Expect::Absent,
            ctx,
        )
        .map_err(|error| format!("first put failed: {error}"))?;
    if first.meta.version.get() != 1 || first.previous.is_some() {
        return Err("the first put must create version 1 with no previous value".into());
    }
    if sink
        .put(
            name,
            SecretValue::from_bytes(b"x".to_vec()),
            Expect::Absent,
            ctx,
        )
        .is_ok()
    {
        return Err("Expect::Absent must reject an existing value".into());
    }
    let value = sink
        .get(name)
        .map_err(|error| format!("get failed: {error}"))?
        .ok_or_else(|| "the value must be readable after put".to_owned())?;
    if value.value.expose() != b"one".as_slice() {
        return Err("the stored value does not round-trip".into());
    }
    let staged = sink
        .stage(
            name,
            SecretValue::from_bytes(b"two".to_vec()),
            Expect::Version(version(1)),
            ctx,
        )
        .map_err(|error| format!("stage failed: {error}"))?;
    let still_one = sink
        .get(name)
        .map_err(|error| format!("get during stage failed: {error}"))?
        .ok_or_else(|| "the old value must stay visible during stage".to_owned())?;
    if still_one.value.expose() != b"one".as_slice() {
        return Err("a staged value must not be visible to readers".into());
    }
    sink.abort(&staged, ctx)
        .map_err(|error| format!("abort failed: {error}"))?;
    if sink
        .get(name)
        .map_err(|error| format!("get after abort failed: {error}"))?
        .map(|secret| secret.value.expose().to_vec())
        != Some(b"one".to_vec())
    {
        return Err("abort must leave the previous value unchanged".into());
    }
    let staged = sink
        .stage(
            name,
            SecretValue::from_bytes(b"two".to_vec()),
            Expect::Version(version(1)),
            ctx,
        )
        .map_err(|error| format!("second stage failed: {error}"))?;
    let committed = sink
        .commit(&staged, ctx)
        .map_err(|error| format!("commit failed: {error}"))?;
    if committed.version.get() != 2 {
        return Err("commit must publish the staged version".into());
    }
    let value = sink
        .get(name)
        .map_err(|error| format!("get after commit failed: {error}"))?
        .ok_or_else(|| "the committed value must be readable".to_owned())?;
    if value.value.expose() != b"two".as_slice() {
        return Err("the committed value does not round-trip".into());
    }
    let deleted = sink
        .delete(name, Expect::Version(version(2)), ctx)
        .map_err(|error| format!("delete failed: {error}"))?;
    if deleted.is_none() || sink.present(name).unwrap_or(true) {
        return Err("delete must tombstone the value".into());
    }
    let recreated = sink
        .put(
            name,
            SecretValue::from_bytes(b"three".to_vec()),
            Expect::Absent,
            ctx,
        )
        .map_err(|error| format!("put after delete failed: {error}"))?;
    if recreated.meta.version.get() != 4 || recreated.previous.is_some() {
        return Err("the tombstone takes version 3, so the next put must be version 4".into());
    }
    if !matches!(
        sink.put(name, SecretValue::from_bytes(Vec::new()), Expect::Any, ctx),
        Err(SecretsError::EmptyValue)
    ) {
        return Err("an empty value must be rejected with EmptyValue".into());
    }
    if !matches!(
        sink.stage(name, SecretValue::from_bytes(Vec::new()), Expect::Any, ctx),
        Err(SecretsError::EmptyValue)
    ) {
        return Err("an empty staged value must be rejected with EmptyValue".into());
    }
    let missing = SecretName::parse("missing-conformance")
        .map_err(|error| format!("invalid test name: {error}"))?;
    if !sink
        .check_refs(std::slice::from_ref(&missing))
        .map_err(|error| format!("check_refs failed: {error}"))?
        .contains(&missing)
    {
        return Err("check_refs must report missing names".into());
    }
    Ok(())
}

// 编译期检查。
const _: fn() = || {
    fn assert_source<T: SecretSource>() {}
    fn assert_sink<T: SecretSink>() {}
    assert_source::<MemorySecrets>();
    assert_sink::<MemorySecrets>();
    assert_source::<MemorySecretReader>();
};

pub type InMemorySecrets = MemorySecrets;
