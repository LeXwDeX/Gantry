//! 模块私有表（store.md §7.10，OQ-4）。
//!
//! 其他模块在 `Store::open*` 时以 [`ModuleSpec`] 声明自己的表，之后经
//! `WriteTx::module_conn` 在**同一事务**里读写（与事件追加、Outbox 写入一起提交或
//! 回滚），事务外经 `ReadSnapshot::module_conn` 只读。
//!
//! 形状是"按表的有序键值"：每张表是 `key（文本，字节序）→ JSON 文档`，支持点查、
//! 插入、覆盖、删除与按前缀的有序扫描。内存实现用 `BTreeMap`；SQLite 实现映射为
//! `CREATE TABLE <table> (key TEXT PRIMARY KEY, value TEXT NOT NULL
//! CHECK (json_valid(value))) STRICT`，两者的排序与语义一致。复合主键由模块自己
//! 拼成键（各段不含分隔符时，前缀扫描即"按前几列查"）；唯一性等跨行约束由模块在
//! 事务内先查后写（单写者下没有竞态，§7.10 末条）。
//!
//! 模块投影器（`Projector`）与 SQL 迁移不在本切片：`ModuleSpec` 用构造函数而不是
//! 结构体字面量，后续加字段不破坏调用方。

use std::collections::BTreeMap;
use std::ops::Bound;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::StoreError;

/// 内置投影器占用的模块名；模块不能注册这些名字。
const RESERVED_MODULES: &[&str] = &["core", "store", "meta", "sqlite"];

/// 单次 `scan` 的上限，与分页上限一致。
pub const MODULE_SCAN_MAX: u32 = 200;

/// 其他模块交给 `Store::open*` 的注册项（store.md §6.1）。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ModuleSpec {
    /// 模块名，也是表名前缀：`<module>_*`。
    pub module: &'static str,
    /// 本模块的私有表；每个名字都必须以 `<module>_` 开头。
    pub tables: &'static [&'static str],
}

impl ModuleSpec {
    pub const fn new(module: &'static str, tables: &'static [&'static str]) -> Self {
        Self { module, tables }
    }
}

/// 打开时校验过的注册表；`Store` 生命周期内不变。
#[derive(Debug, Default)]
pub(crate) struct ModuleRegistry {
    /// 表名 → 所属模块。
    tables: BTreeMap<&'static str, &'static str>,
}

impl ModuleRegistry {
    /// 已注册的全部表名。
    pub(crate) fn tables(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.tables.keys().copied()
    }

    pub(crate) fn new(specs: &[ModuleSpec]) -> Result<Self, StoreError> {
        let mut modules = BTreeMap::new();
        let mut tables = BTreeMap::new();
        for spec in specs {
            if !is_ident(spec.module) || spec.module.contains('_') {
                return Err(invalid(format!(
                    "module name {:?} must match [a-z][a-z0-9]*",
                    spec.module
                )));
            }
            if RESERVED_MODULES.contains(&spec.module) {
                return Err(invalid(format!(
                    "module name {:?} is reserved",
                    spec.module
                )));
            }
            if modules.insert(spec.module, ()).is_some() {
                return Err(invalid(format!(
                    "module {:?} registered twice",
                    spec.module
                )));
            }
            if spec.tables.is_empty() {
                return Err(invalid(format!(
                    "module {:?} declares no tables",
                    spec.module
                )));
            }
            for table in spec.tables {
                let suffix = table
                    .strip_prefix(spec.module)
                    .and_then(|rest| rest.strip_prefix('_'));
                if !matches!(suffix, Some(rest) if !rest.is_empty()) || !is_ident(table) {
                    return Err(invalid(format!(
                        "table {table:?} must be {}_<name> with [a-z0-9_]",
                        spec.module
                    )));
                }
                if tables.insert(*table, spec.module).is_some() {
                    return Err(invalid(format!("table {table:?} declared twice")));
                }
            }
        }
        Ok(Self { tables })
    }

    fn check(&self, module: &str, table: &str) -> Result<(), StoreError> {
        match self.tables.get(table) {
            Some(owner) if *owner == module => Ok(()),
            Some(owner) => Err(StoreError::PreconditionFailed(format!(
                "table {table} belongs to module {owner}, not {module}"
            ))),
            None => Err(StoreError::PreconditionFailed(format!(
                "table {table} is not registered (module {module})"
            ))),
        }
    }
}

fn is_ident(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

fn invalid(message: String) -> StoreError {
    StoreError::InvalidModuleSpec(message)
}

/// 全部模块表的内容：表名 → (键 → 文档)。随 `Inner` 克隆，与事务一起提交。
pub(crate) type ModuleTables = BTreeMap<&'static str, BTreeMap<String, Value>>;

/// 一次扫描的结果项：`(key, 文档)`，按 `key` 字节序升序。
pub type ModuleRow<T> = (String, T);

/// 事务内的模块表读写（`WriteTx::module_conn`）。
///
/// 只能访问 `module` 自己注册的表；写入留在事务里，`commit` 才生效，回滚或
/// 丢弃事务则全部撤销。
pub struct ModuleConn<'a> {
    pub(crate) module: &'static str,
    pub(crate) registry: &'a ModuleRegistry,
    pub(crate) tables: &'a mut ModuleTables,
}

impl ModuleConn<'_> {
    pub fn get<T: DeserializeOwned>(
        &self,
        table: &str,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        self.reader().get(table, key)
    }

    pub fn scan<T: DeserializeOwned>(
        &self,
        table: &str,
        prefix: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ModuleRow<T>>, StoreError> {
        self.reader().scan(table, prefix, after, limit)
    }

    /// 只在键不存在时插入；返回是否插入（已存在则不改动，返回 `false`）。
    pub fn insert<T: Serialize>(
        &mut self,
        table: &'static str,
        key: &str,
        value: &T,
    ) -> Result<bool, StoreError> {
        let value = encode(value)?;
        let rows = self.rows_mut(table, key)?;
        if rows.contains_key(key) {
            return Ok(false);
        }
        rows.insert(key.to_owned(), value);
        Ok(true)
    }

    /// 插入或覆盖；返回此前是否已有该键。
    pub fn put<T: Serialize>(
        &mut self,
        table: &'static str,
        key: &str,
        value: &T,
    ) -> Result<bool, StoreError> {
        let value = encode(value)?;
        Ok(self
            .rows_mut(table, key)?
            .insert(key.to_owned(), value)
            .is_some())
    }

    /// 删除；返回此前是否存在。
    pub fn delete(&mut self, table: &'static str, key: &str) -> Result<bool, StoreError> {
        Ok(self.rows_mut(table, key)?.remove(key).is_some())
    }

    fn reader(&self) -> ModuleReader<'_> {
        ModuleReader {
            module: self.module,
            registry: self.registry,
            tables: self.tables,
        }
    }

    fn rows_mut(
        &mut self,
        table: &'static str,
        key: &str,
    ) -> Result<&mut BTreeMap<String, Value>, StoreError> {
        self.registry.check(self.module, table)?;
        check_key(key)?;
        Ok(self.tables.entry(table).or_default())
    }
}

/// 快照上的模块表只读访问（`ReadSnapshot::module_conn`）。
pub struct ModuleReader<'a> {
    pub(crate) module: &'static str,
    pub(crate) registry: &'a ModuleRegistry,
    pub(crate) tables: &'a ModuleTables,
}

impl ModuleReader<'_> {
    pub fn get<T: DeserializeOwned>(
        &self,
        table: &str,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        self.registry.check(self.module, table)?;
        check_key(key)?;
        self.tables
            .get(table)
            .and_then(|rows| rows.get(key))
            .map(decode)
            .transpose()
    }

    /// 键以 `prefix` 开头、且严格大于 `after`（键集分页）的前 `limit` 行，按键升序。
    /// `limit` 取 1..=[`MODULE_SCAN_MAX`]。
    pub fn scan<T: DeserializeOwned>(
        &self,
        table: &str,
        prefix: &str,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ModuleRow<T>>, StoreError> {
        self.registry.check(self.module, table)?;
        if limit == 0 || limit > MODULE_SCAN_MAX {
            return Err(StoreError::PreconditionFailed(format!(
                "module scan limit must be 1..={MODULE_SCAN_MAX}"
            )));
        }
        let Some(rows) = self.tables.get(table) else {
            return Ok(Vec::new());
        };
        let start = match after {
            Some(after) if after >= prefix => Bound::Excluded(after),
            _ => Bound::Included(prefix),
        };
        rows.range::<str, _>((start, Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(prefix))
            .take(limit as usize)
            .map(|(key, value)| Ok((key.clone(), decode(value)?)))
            .collect()
    }
}

fn check_key(key: &str) -> Result<(), StoreError> {
    // NUL 在 SQLite 文本里会截断比较；空键没有意义。
    if key.is_empty() || key.contains('\0') {
        return Err(StoreError::PreconditionFailed(
            "module table key must be non-empty and contain no NUL".to_owned(),
        ));
    }
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Value, StoreError> {
    serde_json::to_value(value)
        .map_err(|error| StoreError::ModuleValue(format!("encode failed: {error}")))
}

fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, StoreError> {
    T::deserialize(value)
        .map_err(|error| StoreError::ModuleValue(format!("decode failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_rejects_bad_specs() {
        let ok = ModuleSpec::new("policy", &["policy_challenges", "policy_consumed"]);
        assert!(ModuleRegistry::new(std::slice::from_ref(&ok)).is_ok());
        for bad in [
            ModuleSpec::new("Policy", &["Policy_x"]),
            ModuleSpec::new("po_licy", &["po_licy_x"]),
            ModuleSpec::new("core", &["core_x"]),
            ModuleSpec::new("policy", &[]),
            ModuleSpec::new("policy", &["challenges"]),
            ModuleSpec::new("policy", &["policy_"]),
            ModuleSpec::new("policy", &["policyx_a"]),
            ModuleSpec::new("policy", &["policy_A"]),
            ModuleSpec::new("policy", &["policy_a", "policy_a"]),
        ] {
            assert!(
                matches!(
                    ModuleRegistry::new(std::slice::from_ref(&bad)),
                    Err(StoreError::InvalidModuleSpec(_))
                ),
                "{bad:?} accepted"
            );
        }
        assert!(matches!(
            ModuleRegistry::new(&[ok.clone(), ok]),
            Err(StoreError::InvalidModuleSpec(_))
        ));
    }
}
