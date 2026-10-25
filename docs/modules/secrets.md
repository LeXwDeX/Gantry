# 密钥存储（`secrets`）

> 状态：设计 v0.1 · 里程碑：M1（M3 增加轮换工作流） · 泳道：`sec` · crate / 目录：`crates/gantry-secrets`

服务端有几类必须长期保存、泄露即失守的凭据：GitHub App 私钥与 webhook secret、CA 中间证书私钥、模型 key、私有包仓库凭据、APNs 认证密钥（M4）。`secrets` 是保存它们的**库**，不是服务：每个进程（`gantryd`、`egress`）各开自己的实例，目录、操作系统用户、主密钥都不同，所以 `gantryd` 被攻破也读不到 Egress 里的模型 key（decisions P-16、P-38，architecture §4、§6）。

读者：实现 `secrets` 的 agent；以及 `forge`、`pki`、`egress`、`api`、`deploy` 的实现者（读 §6 与 §7.5 即可）。

标注 **[提议]** 的是骨架没定、本文给的默认；**OQ-n** 见 §13，不要自行拍板。契约优先级见 [AGENTS.md](../../AGENTS.md)。

## 1. 职责

1. **静态加密存储**：把凭据值以认证加密（Authenticated Encryption with Associated Data, AEAD）写成本地文件，仅服务进程用户可读（architecture §6："本地密钥文件（加密）"）。
2. **按使用方隔离读取**：读取句柄绑定一个使用方（`Consumer`，定义在 [core §6.1](core.md#61-标识符时间基础值)）——`forge`：App 私钥与 webhook secret（含轮换窗口里的上一个）；`pki`：中间 CA 私钥；`notify`：APNs 认证密钥（M4，P-59）；`egress`：模型 key 与私有包仓库凭据（architecture §4 `secrets` 行）。跨使用方读取一律失败。
3. **版本化**：同名写入产生新版本，版本号单调递增、**删除后也不回退**；版本号回报给 settings 的 `secret_changes[].version`（[config.md §2.3–§2.4](../interfaces/config.md#24-凭据secret_ref)）。
4. **可与事件原子配合的写入**：`stage` → `commit` / `abort` 两阶段，使"secret 写入与 `settings.updated` 要么都成功要么都不成功"（config.md §2.4"原子性"）在单进程内可实现。
5. **删除与回收**：墓碑（tombstone）记录删除，保证版本号不复用；保留最近 `retain_versions` 个版本，其余清理。
6. **轮换**（M3）：读取方在下一次使用时拿到新值；旧版本在窗口内仍可按版本读取；主密钥换封（rewrap）。
7. **加密备份与恢复**（architecture §6"需要（加密备份）"）：导出为独立于主机主密钥的加密归档，恢复时保留版本号。
8. **存在性与健康**：`present`（只回答布尔值）、`check_refs`（供 settings 的 `unresolved_secret_refs`，config.md §2.8）。
9. **内存与输出卫生**：密钥值类型零化、不可序列化、不可 `Debug` 输出；进程加固辅助（禁用 core dump）。
10. **测试实现**：`MemorySecrets`（同一组 trait、同一套契约测试）。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 冻结 §6 的 trait 签名（`SecretSource` / `SecretSink` 进 `core::ports`，跨泳道，P-51）并交付 `MemorySecrets` 作为替身（decisions P-81，已列入 milestones 替身表）；`core` 已提供 `SecretName` / `Consumer` / `SecretWrite`。`forge`、`pki`、`egress`、`api` 在 M1 与本模块并行时用它 |
| M1 | milestones 明确："加密存储、按使用方隔离读取、加密备份"。本文展开为：§1 的 1–5、7–10 全部（含 `reader` / `scoped` / `admin` 三级句柄、`stage` / `commit` / `abort`、`check_refs`、`put` 覆盖写（版本递增）、`FileSecrets` 与 `MemorySecrets`（含故障注入）、备份 / 恢复、`harden_process`）；引导期主机写入入口（第一台设备配对前，P-66）所需的本地 `put` |
| M2 | milestones 未列；无新增 |
| M3 | milestones 明确："密钥轮换（App 私钥、中间 CA、模型 key）"。本文展开为：`SecretHandle`（自动刷新）、旧版本保留窗口与 `get_version`、`rewrap`、与 `forge` / `pki` / `egress` 重载联动的约定；各自的轮换流程归 `forge`、`pki`、`egress` |
| M4 | `notify` 使用方（APNs 密钥，P-59）；无新增接口 |

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| 谁有权写 / 改密钥（角色、设备签名） | `api`（settings 路由，[config.md §2.5](../interfaces/config.md#25-修改权限设备角色)）；`secrets` 不认识设备与角色 |
| 校验 `secret_ref` 的引用关系（V03）、防误填（V02） | `core` 的校验器 + `api` |
| `api`（`gantryd` 进程）把模型 key **转交**给 Egress 进程的通道，以及跨进程的原子性 | Egress 控制面（`/run/gantry/egress.sock`，`EgressControl::{secret_present, stage_secret, activate_secret, discard_secret, staged_secrets}`，只写不读，P-50）；`secrets` 只提供各实例的本地 API，Egress 实例在控制面背后调用它 |
| 把凭据注入出站请求 | `egress`；JWT 签名与换 installation token：`forge`；证书签发：`pki` |
| CA 根私钥 | 离线保存，服务端不常驻（architecture §6） |
| 客户端设备私钥 | `desktop-shell`（I6） |
| GitHub installation token | `forge` 进程内存，不入 `secrets`（[forge-actions §9.4](../interfaces/forge-actions.md#94-令牌与最小化)） |
| 主密钥的生成、发放、保管；备份的调度、传输、接收方密钥保管；引导期主机命令（`gantry github setup` 等）的 CLI 形态 | `deploy`（OQ-2、OQ-3；P-65、P-66） |
| 把凭据写进沙盒的环境变量 / 挂载 | **没有这条路径**（I1）；`secrets` 不提供任何"导出到环境 / 文件"的 API |
| 审计事件 | 调用方：`api` 在 `settings.updated.secret_changes[]` 里记 `{ref, op, version}`；`secrets` 无事件日志依赖 |

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `core` | `SecretName`、`Consumer`、`Timestamp`、`Sha256Digest`（备份清单） | [core.md §6.1](core.md#61-标识符时间基础值)、config.md §2.4 |

外部库（实现者选型，约束如下）：AEAD（XChaCha20-Poly1305 实现）、HKDF-SHA256、`zeroize` / `secrecy`、`subtle`（常量时间比较）、`rustix` 或等价（`flock`、`openat`、`renameat2`、`fsync`、`prctl`、`setrlimit`）、备份用 `age` 或等价（OQ-3）。**不依赖** `store`、`tokio`（同步 API；调用方需要时自行 `spawn_blocking`）、任何网络库。

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| 凭据明文值 | `api`（settings 写入，经 mTLS 来自 approver 设备；模型 key 经 Egress 控制面转交给 Egress 实例）；引导期主机命令与 App Manifest 回调（`forge` 的 `/setup/github/callback`，第一台设备配对前，P-65、P-66）；`deploy` 的安装流程（CA 中间私钥）；`pki` | `SecretValue`，≤ `max_value_bytes`；内容不透明 | 最高敏感；来自可信模块但**值本身**不被解释 |
| 主密钥 | `deploy` 配置的 `MasterKeySource` | 32 字节 | 最高敏感 |
| 磁盘上的密钥文件 | 本地文件系统 | §7.2 信封格式 | **完整性不可信**：任何篡改 → AEAD 失败 → `Corrupt` |
| 读取 / 写入请求 | `forge`、`pki`、`notify`（M4）、`egress`、`api` | `SecretSource::get` / `SecretSink::put`… | 调用方可信，但受 `Consumer` 约束 |
| 备份接收方公钥 / 恢复身份 | `deploy` / 运维 | §7.6 | 公钥可信；恢复身份最高敏感 |
| 备份归档（恢复时） | 运维 | §7.6 | 经 AEAD 校验后才被采纳 |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| 凭据明文 | 调用方进程内存（`SecretValue`） | **只在进程内**；类型不可序列化 |
| 元数据 `SecretMeta` | `api`（`present`、`secret_changes`、status） | `{consumer, name, version, updated_at}`，**无值、无长度、无哈希** |
| `PutOutcome` | `api` | `{meta, previous: Option<SecretVersion>}`，调用方据此判定 `op = set / rotate` |
| 暂存句柄 `StagedSecret` | `api`（两阶段写入） | `{consumer, name, version}`，无值 |
| 加密备份流 | `deploy` 的备份脚本 | §7.6 |
| 未解析引用清单 | `api` → `/v1/settings/status` 的 `unresolved_secret_refs` | `Vec<SecretName>` |
| 日志与指标 | `ops` | §11；**不含值** |

## 6. 对外接口

只写签名与语义。同步 API。

```rust
// 密钥值：刻意不可 Clone / Serialize / Display；Debug 输出 "SecretValue(***)"；Drop 时零化；PartialEq 用常量时间比较。
pub struct SecretValue(/* Zeroizing<Vec<u8>> */);
impl SecretValue {
    pub fn from_bytes(b: Vec<u8>) -> Self;
    pub fn expose(&self) -> &[u8];
    pub fn expose_str(&self) -> Result<&str, std::str::Utf8Error>;   // PEM、token 等文本型凭据
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)] pub struct SecretVersion(u64);   // 从 1 起

pub struct SecretMeta { pub consumer: Consumer, pub name: SecretName, pub version: SecretVersion, pub updated_at: Timestamp }
pub struct Secret { pub value: SecretValue, pub version: SecretVersion }
pub enum Expect { Any, Absent, Version(SecretVersion) }              // 比较并写入，防丢失更新

// ── 三个能力层级，由窄到宽。业务模块只拿自己需要的最窄的那个。
// SecretSource / SecretSink 及其类型定义在 core::ports（使用方分属不同泳道，P-51、P-81）；第三级管理句柄只在本 crate。

// 读：仅限一个使用方的命名空间。forge（App 私钥、webhook secret）、egress（运行期取模型 key）拿到这个。
pub trait SecretSource: Send + Sync {
    fn get(&self, name: &SecretName) -> Result<Option<Secret>, SecretsError>;
    fn version(&self, name: &SecretName) -> Result<Option<SecretVersion>, SecretsError>;   // 轻量：只看目录，不解密
    fn present(&self, name: &SecretName) -> Result<bool, SecretsError>;                     // = settings 的 present；EgressControl::secret_present 在 Egress 实例里调用它
    fn get_version(&self, name: &SecretName, v: SecretVersion) -> Result<Option<Secret>, SecretsError>; // 轮换窗口内读旧版本（M3）
}

// 读写：仍限于一个使用方的命名空间。pki（续签叶子证书时写新私钥）、api 的转交路径、egress 实例里接收转交的代码拿到这个。
pub trait SecretSink: SecretSource {
    fn put(&self, name: &SecretName, value: SecretValue, expect: Expect) -> Result<PutOutcome, SecretsError>;
    fn stage(&self, name: &SecretName, value: SecretValue, expect: Expect) -> Result<StagedSecret, SecretsError>;
    fn commit(&self, s: StagedSecret) -> Result<SecretMeta, SecretsError>;       // api.md 占位名 activate
    fn abort(&self, s: StagedSecret) -> Result<(), SecretsError>;                // api.md 占位名 discard
    fn staged(&self) -> Result<Vec<StagedSecret>, SecretsError>;                 // 启动对账用；暂存版本对读者不可见
    fn delete(&self, name: &SecretName, expect: Expect) -> Result<Option<SecretMeta>, SecretsError>;
    fn list(&self) -> Result<Vec<SecretMeta>, SecretsError>;                     // 仅元数据
    fn check_refs(&self, refs: &[SecretName]) -> Result<Vec<SecretName>, SecretsError>; // 返回缺失的
}

// 跨使用方的管理操作：备份、恢复、主密钥换封、列出全部暂存项。只交给组装代码与 deploy 的入口。
pub trait SecretAdminOps: Send + Sync {
    fn export_backup(&self, to: &[BackupRecipient], out: &mut dyn Write) -> Result<BackupManifest, SecretsError>;
    fn import_backup(&self, id: &BackupIdentity, input: &mut dyn Read, mode: ImportMode) -> Result<ImportReport, SecretsError>;
    fn rewrap(&self, new_master: MasterKey) -> Result<RewrapReport, SecretsError>;   // M3
    fn staged_all(&self) -> Result<Vec<StagedSecret>, SecretsError>;
}
pub struct PutOutcome { pub meta: SecretMeta, pub previous: Option<SecretVersion> }
pub struct StagedSecret { pub consumer: Consumer, pub name: SecretName, pub version: SecretVersion }
pub enum ImportMode { Fresh, Merge, Overwrite }   // Fresh：实例必须为空；Merge：仅 backup 版本更高的条目；Overwrite：显式危险
impl From<SecretString> for SecretValue {}         // core 的 SecretWrite::Set(SecretString) 转入

// ── 文件实现
pub trait MasterKeySource: Send + Sync { fn load(&self) -> Result<MasterKey, SecretsError>; }
pub struct SystemdCredential { pub name: String }          // 读 $CREDENTIALS_DIRECTORY/<name>
pub struct KeyFile { pub path: PathBuf }                   // 0400，属主 = 当前用户，位于 secrets 目录之外

pub struct SecretsConfig {
    pub dir: PathBuf, pub consumers: Vec<Consumer>,        // 本实例承载的使用方
    pub master_key: Box<dyn MasterKeySource>,
    pub retain_versions: u32, pub max_value_bytes: usize,
}
pub struct Secrets { /* 持有各使用方的 DEK；不持有主密钥 */ }
impl Secrets {
    pub fn init(cfg: SecretsConfig) -> Result<(), SecretsError>;        // 创建目录（0700）与 secrets.meta；目录已存在则拒绝
    pub fn open(cfg: SecretsConfig) -> Result<Secrets, SecretsError>;   // 校验权限与 key_id；不创建
    pub fn reader(&self, who: Consumer) -> Result<SecretReader, SecretsError>;   // 实例未承载该使用方 → Denied
    pub fn scoped(&self, who: Consumer) -> Result<ScopedSecrets, SecretsError>;  // 同上，读写
    pub fn admin(&self) -> SecretAdmin;
}
pub struct SecretReader { /* 只含 who 的 DEK */ }     impl SecretSource for SecretReader {}
pub struct ScopedSecrets { /* 只含 who 的 DEK */ }    impl SecretSink for ScopedSecrets {}   // 同时是 SecretSource
impl SecretReader { pub fn handle(&self, name: &SecretName) -> SecretHandle; }   // M3：见 §7.7
pub struct SecretAdmin;                                impl SecretAdminOps for SecretAdmin {}

// 进程加固：PR_SET_DUMPABLE=0、RLIMIT_CORE=0。gantryd 与 egress 的 main 在读取任何密钥之前调用。
pub fn harden_process() -> Result<(), SecretsError>;

// ── 测试实现
pub struct MemorySecrets;   // 与 FileSecrets 同样提供 reader / scoped / admin，语义一致（含 Denied、版本、墓碑、stage/commit）；
                            // 另有故障注入：读不到、写失败、stage 之后"崩溃"（丢弃进程内状态）。pki.md 称它为 InMemorySecrets，同名别名即可
pub type InMemorySecrets = MemorySecrets;
```

**与其他规格书里占位名的对应**（那些规格书注明"以 `secrets` 规格为准"）：

| 其他规格书的占位 | 本文 |
| --- | --- |
| `api.md` 的 `SecretOwner { Egress, Forge }` | `core::Consumer { Forge, Pki, Notify, Egress }`（`Pki` 不经 settings 写入；`Notify` 为 M4 APNs 密钥，P-59） |
| Egress 实例：`EgressControl::{secret_present, stage_secret, activate_secret, discard_secret, staged_secrets}`（P-50） | Egress 进程内调用 `SecretSink::{present, stage, commit, abort, staged}`；`activate_secret` 返回的版本即 `SecretMeta.version` |
| `gantryd` 实例（`forge` 的 App 私钥 / webhook secret） | `api` 直接用 `ScopedSecrets(Forge)` 的同名方法 |
| `api.md` 的 `SecretString` | `SecretValue`（从 `core::SecretString` 转入）；`SecretError` ≡ `SecretsError` |
| `pki.md` 的 `SecretStore`（`get / put / delete`，按使用方隔离） | `ScopedSecrets`（`SecretSink`）；`InMemorySecrets` ≡ `MemorySecrets` |

`api` 与 Egress 实例之间经 Egress 控制面传递（P-50，见 [egress.md §6.2](egress.md)、[interfaces/egress-control.md](../interfaces/egress-control.md)）；控制面只写不读，没有读取值的路由。

```rust
pub enum SecretsError {
    Denied { who: Consumer, name: SecretName },       // 跨使用方读写、实例未承载该使用方
    NotFound,                                         // 仅用于必须存在的操作（commit / abort / expect 失败的 delete）
    Conflict { expected: Expect, actual: Option<SecretVersion> },
    Corrupt { path: PathBuf, reason: CorruptReason },  // AEAD 校验失败、头部非法、AAD 不符
    KeyMismatch,                                       // 主密钥与 secrets.meta 的 key_id 不符
    KeyUnavailable(String),
    InsecurePermissions { path: PathBuf, mode: u32 },
    UnsupportedFormat { found: u8 },
    TooLarge { limit: usize },
    NotStaged, AlreadyStaged,
    Io(std::io::Error),
}   // 所有 Display / Debug 都不含值
```

## 7. 内部设计

### 7.1 目录布局与权限

```
<secrets.dir>/                     目录 0700，属主 = 当前进程的有效 uid
  secrets.meta                      明文 JSON：{format:1, instance_id:<ULID>, key_id:<hex8>, consumers:[…], created_at}（无机密）
  .lock                             flock：写操作期间持有（互斥）；读不加锁
  <consumer>/                       0700（forge / pki / notify / egress）
    <name>.v<N>.sec                 已提交版本，0600，写入后不可变
    <name>.v<N>.staged              暂存版本；读者与 list 都不可见
    <name>.v<N>.tomb                墓碑：删除标记，保证版本号不回退
```

- 文件名从右侧解析 `.v<数字>.<sec/staged/tomb>`，剩余部分是 `SecretName`（名字里允许 `.`，所以必须从右解析）。
- **当前版本** = 该名字下最大版本号的 `.sec` 或 `.tomb`；是 `.tomb` 则视为不存在。
- `open` 时检查：目录与文件的属主 = 有效 uid、目录无组 / 其他位、文件为 `0600`；任何一项不符 → `InsecurePermissions`，**不自动修复**。所有文件操作用 `openat` + `O_NOFOLLOW`，不跟随符号链接。
- 沙盒用户、`egress` 用户、`gantryd` 用户互不相同；目录各自独立（具体路径由 `deploy` 在引导配置里给出）。这是跨进程隔离的**真正边界**（见 §8）。

### 7.2 信封格式

每个 `.sec` / `.staged` / `.tomb` 文件：

```
偏移  长度  内容
0     4     魔数 "GSEC"
4     1     格式版本 = 1
5     1     算法 = 1（XChaCha20-Poly1305）
6     1     种类：1 = 密钥，2 = 墓碑
7     1     保留 = 0
8     4     key_id（主密钥标识，用于在 AEAD 失败之前先报 KeyMismatch）
12    8     created_at（毫秒，大端）
20    24    nonce（随机）
44    …     密文 ‖ 16 字节认证标签（墓碑的明文为空）
AAD = 头部前 20 字节 ‖ consumer ‖ 0x00 ‖ name ‖ 0x00 ‖ version（u64 大端）
```

AAD 把密文绑定到 `(使用方, 名字, 版本, 头部)`：把一个文件改名、复制到别的使用方目录或换版本号，解密都会失败（`Corrupt`）。未知格式版本 → `UnsupportedFormat`，不尝试解析。

### 7.3 密钥层级

```
主密钥 MasterKey（32 字节，每个实例一把，由 deploy 提供）
  └─ DEK(consumer) = HKDF-SHA256(ikm = 主密钥, salt = instance_id, info = "gantry/secrets/v1/dek/" ‖ consumer)
key_id = SHA-256("gantry/secrets/v1/keyid" ‖ 主密钥) 的前 4 字节
```

- `open` 时为本实例承载的每个使用方派生 DEK，然后**立即零化主密钥**；运行期只持有 DEK。`SecretReader(who)` 与 `ScopedSecrets(who)` 只拿到 `who` 的 DEK，所以持有 `forge` 句柄的代码即使有缺陷也解不开 `pki` 的文件。`Secrets` 对象本身持有全部 DEK，只应由组装代码（`gantryd` / `egress` 的 `main`）持有，业务模块只收到 `SecretReader`（只读，如 `forge`、`egress`）或 `ScopedSecrets`（读写，如 `pki`、`api` 的转交路径），跨使用方的 `SecretAdmin` 只交给 `deploy` 的入口。
- 两个进程用不同的主密钥与不同的目录：拿到 `gantryd` 的主密钥与文件，解不开 Egress 的文件。

### 7.4 写入与原子性

- **写入一个文件**：在目标目录内写 `.tmp.<随机>`（`0600`）→ `fsync` 文件 → `renameat2(RENAME_NOREPLACE)` 到目标名 → `fsync` 目录。目标名已存在则失败（版本号永不覆盖）。
- **`put`**（一次完成）：持 `.lock` → 取当前版本 `c`（含墓碑）→ 按 `expect` 比较 → 新版本 `c + 1`（从未存在则 1）写 `.sec` → 清理超出 `retain_versions` 的旧 `.sec`（尽力而为，失败只记 `warn`）。返回的 `PutOutcome.previous` 是写入前的当前版本（`None` 表示首次写入或之前已删除）。
- **`stage` / `commit` / `abort`**：`stage` 在持锁下取版本号 `c + 1` 写 `.staged`（同一名字同时只允许一个暂存版本，否则 `AlreadyStaged`）；`commit` 持锁，核对 `.staged` 存在且版本号仍大于当前版本，`renameat2` 为 `.sec`，再做保留清理；`abort` 删除 `.staged`。暂存版本**对读者完全不可见**。
- **与 `settings.updated` 的配合顺序**（`api` 调用方遵守，对应 config.md §2.4"原子性"）：
  1. `stage` → 得到 `StagedSecret{version}`；
  2. 在 `store` 的一个 `WriteTx` 里追加 `settings.updated`，`secret_changes[]` 带上 `{ref, op, version}`，提交；
  3. `commit`。
  
  步骤 1 或 2 失败 → `abort`。步骤 2 提交后崩溃（尚未 `commit`）→ 启动时组装代码调用 `SecretSink::staged()`（或跨使用方的 `staged_all()`），对每个暂存项到事件日志里查"是否存在带 `(ref, version)` 的 `settings.updated`"（`store` 的 `EventLog::by_secret_change`，见 store.md §6.2）：存在则 `commit`，不存在则 `abort`。**`secrets` 自己不自动提交或丢弃暂存项**：自动丢弃会让已提交的配置指向不存在的版本。运行期（非启动时）的对账可以给宽限，避免丢弃一个事件即将提交的在途暂存项——`api` 的规格书取 10 分钟，由 `api` 执行，不在本库里。
- **`delete`**：持锁，写 `v(c+1).tomb`，再清理旧 `.sec`。之后 `put` 从 `c + 2` 继续，版本号不回退。
- **并发**：写操作互斥（`.lock`，flock，`put` / `stage` / `commit` / `delete` 期间持有，毫秒级）；读不加锁——已提交文件不可变，读者要么看到版本 `n` 要么看到 `n+1`，都是完整的。多进程同目录的写入也因此安全，但**部署上仍是一个目录一个进程**。

### 7.5 读取

`get(name)`：列出该使用方目录下名字匹配的文件 → 取当前版本 → 打开、读入预分配好容量的 `Zeroizing<Vec<u8>>`（避免扩容留下未零化的副本）→ 校验头部与 `key_id` → AEAD 解密 → 返回。任何一步失败都不返回部分数据，也不回退到旧版本（旧版本只能显式 `get_version`）。`version(name)` 只做目录列表，不解密，供读取方便宜地检查是否有新版本。

### 7.6 备份与恢复 **[提议，OQ-3]**

- `export_backup(recipients, out)`：持锁读取本实例**每个使用方的当前版本**与墓碑，生成 `manifest.json` 与各条目，整体用 `age`（X25519 接收方，可多个）加密为一个流。清单：`{format:1, instance_id, created_at, entries:[{consumer, name, version, kind}]}`。**备份里没有明文落盘**；备份接收方的私钥（恢复身份）由用户离线保管，不在服务器上（与根 CA 同样处理，architecture §7.1）。
- `import_backup(identity, input, mode)`：解密 → 校验清单与每个条目 → 以**原版本号**写 `v<N>.sec` / `.tomb`（保证 `secret_changes[].version` 与事件日志一致）。`Fresh` 要求实例为空；`Merge` 只写版本号高于本地的条目；`Overwrite` 显式覆盖，记 `warn`。恢复后的实例继续用**本机的**主密钥重新加密（备份里的明文不依赖旧主机的主密钥）。
- 每个实例各自导出：`gantryd` 的实例（`forge`、`pki`）与 Egress 的实例分别备份，由 `deploy` 的备份脚本以各自的用户身份调用。

### 7.7 轮换（M3）

- 轮换 = 对同名 `put`（版本 +1）。`retain_versions >= 2` 保证旧版本在窗口内仍可 `get_version`（例如 Egress 的在途请求、App 私钥在 GitHub 端新旧并存的过渡）。
- webhook secret 的轮换在 config.md §10 里用**两个独立 ref**（`webhook_secret` 与 `webhook_secret_previous`）表达，与本库的"同名多版本"是两回事：`forge` 同时读两个 ref。
- `SecretHandle`：`SecretReader::handle(name)` 返回懒刷新句柄，`current() -> Arc<Secret>` 最多每 `refresh_interval_s` 检查一次 `version()`，版本变化才重新解密；`invalidate()` 供信号处理（Egress 收到重载信号，config.md §2.4"轮换"）强制刷新。M1 的使用方可以自己缓存。
- `rewrap(new_master)`：用现有 DEK 解密、用新主密钥派生的 DEK 重新加密所有文件（先写新文件再替换，逐个原子），更新 `secrets.meta.key_id`。中途崩溃会留下新旧 `key_id` 混合的目录：`open` 报 `KeyMismatch` 并提示 `rewrap` 未完成；重新执行 `rewrap`（同时给出旧、新主密钥）即可完成，`rewrap` 对已换封的文件跳过，可重入。

### 7.8 内存与进程卫生

- `SecretValue`、`MasterKey`、DEK 都是 `Zeroizing`；不实现 `Clone` / `Serialize` / `Display`；`Debug` 输出固定占位；`PartialEq` 用 `subtle` 常量时间比较。需要共享时用 `Arc<Secret>`。
- 所有错误值的 `Display` / `Debug` 只含名字、使用方、版本、路径，永不含值或值的哈希 / 长度。
- `harden_process()`：`prctl(PR_SET_DUMPABLE, 0)` 与 `setrlimit(RLIMIT_CORE, 0)`；`deploy` 的 systemd 单元另设 `LimitCORE=0`（进程内与单元层都做，互为兜底）。
- 日志里**不记录读取到的内容**；`get` 的成功路径只更新计数器，不逐条写日志（Egress 每个请求都会读）。

## 8. 不变式与安全要求

| 不变式 / 要求 | `secrets` 如何保证 |
| --- | --- |
| I1 沙盒只计算：无长期凭据 | 库里**没有**导出到环境变量 / 挂载 / 文件的 API；沙盒用户对任何 `secrets` 目录无权限（目录 `0700`、属主为服务用户，`open` 检查）；模型 key 只存在 Egress 实例 |
| I6 客户端不持凭据 | `SecretMeta` / `PutOutcome` / 事件里只有名字与版本；值类型不可序列化，无法被放进响应、事件或日志；`present` 来自 `present()`（只回答布尔值） |
| 使用方隔离 | `SecretReader(who)` / `ScopedSecrets(who)` 只持有 `who` 的 DEK；密文经 AAD 绑定使用方 + 名字 + 版本；跨使用方读 → `Denied`，把文件复制到别的使用方目录 → `Corrupt` |
| 失败即闭合 | 权限异常、AEAD 失败、主密钥不符、格式未知，一律返回错误，不降级为明文、不返回旧值、不自动修复 |
| 防篡改 | AEAD 覆盖密文与头部（含 `created_at`、`key_id`）；使用方、名字、版本号绑定进 AAD，所以把旧版本文件改名冒充新版本、或跨名字 / 跨使用方复制，解密都会失败 |
| 不泄露 | 值不入日志、事件、指标标签、错误信息、panic 信息、备份明文、core dump（`harden_process`） |
| 诚实的威胁模型 | 主密钥对运行中的进程用户是可读的（`KeyFile` 或 systemd credential 都如此），所以静态加密防的是**磁盘 / 备份泄露、其他用户、误提交**，不防"已拿到服务进程用户代码执行"。跨进程隔离靠**操作系统用户分离**，而不是靠加密；回滚攻击（拿到目录写权限后换回旧文件）也不在防护范围 |

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 目录 / 文件权限不符 | `InsecurePermissions`，`open` 失败，实例不可用；使用方功能置 `unhealthy`（config.md §2.4"缺失"：不崩溃，在 status 显示） |
| 主密钥不可得 / 与 `key_id` 不符 | `KeyUnavailable` / `KeyMismatch`，`open` 失败；**不生成新密钥覆盖**（避免静默丢数据） |
| AEAD 失败 / 头部非法 | `Corrupt`，读取失败，记 `error`；不删除文件，留给运维从备份恢复 |
| 读到不存在的名字 | `get` 返回 `Ok(None)`；调用方按"缺失"处理（`api` 的 V03 `secret_ref_missing`；启动时缺失 → 功能 `unhealthy`） |
| `expect` 不满足 | `Conflict`；调用方重读后决定 |
| `stage` / `put` 中途崩溃 | 只可能留下 `.tmp.*`（启动时清理）或 `.staged`（`staged()` / `staged_all()` 报告，由调用方对账，见 §7.4）；`.sec` 永远是完整文件 |
| `commit` 时 `.staged` 不见了 | `NotStaged` |
| 值超过 `max_value_bytes` | `TooLarge`，不写入 |
| 磁盘满 | `Io`，写入失败；已存在的 `.sec` 不受影响 |
| `rewrap` / `import_backup` 中途崩溃 | 可重入（逐文件原子；`Merge` 模式只补缺）；重跑即可 |
| 多处同时写同名 | 由 `.lock` 串行化；后到者按 `expect` 得到 `Conflict` 或在新版本上继续 |

## 10. 配置

`secrets` 的配置是**引导配置**（`deploy` 拥有，重启生效；[config.md §1.1](../interfaces/config.md#11-不属于本文的内容)："`secrets/` 路径"），不是 settings，不进事件日志，值本身永远不在配置文件里。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `secrets.dir` | 无默认（`deploy` 给出；`gantryd` 与 `egress` 各一个，位于不同用户的目录） | 实例目录 |
| `secrets.consumers` | `gantryd`：`["forge","pki","notify"]`（`notify` 自 M4）；`egress`：`["egress"]` | 本实例承载的使用方；与 architecture §4 的 `secrets` 行一致（P-59） |
| `secrets.master_key.source` | 无默认（OQ-2）：`systemd_credential` / `key_file` | 主密钥来源 |
| `secrets.master_key.name` / `.path` | 无默认 | credential 名 / 密钥文件路径（后者必须在 `secrets.dir` 之外、`0400`） |
| `secrets.retain_versions` | `2` **[建议值，待调优]** | 保留最近几个已提交版本 |
| `secrets.max_value_bytes` | `65536` **[建议值，待调优]** | 单个值上限（RSA 4096 位私钥 PEM 约 3 KiB） |
| `secrets.refresh_interval_s` | `5` **[建议值，待调优]** | `SecretHandle` 检查新版本的最短间隔（M3） |
| `secrets.backup.recipients_file` | 无默认（`deploy`） | 备份接收方公钥文件 |
| 目录 / 文件权限 | `0700` / `0600` | 常量，不可配置 |

## 11. 可观测性

`secrets` **不发出领域事件**（无事件日志依赖）。审计由调用方负责：`api` 写 `settings.updated`，其 `secret_changes[]` 为 `{ref, op: set / rotate / delete, version}`，**无值、无哈希**（config.md §2.3）。

- 日志（`tracing`，字段只含 `consumer`、`name`、`version`、`op`、`result`）：`info`——`init` / `open` / `put` / `commit` / `delete` / `export_backup` / `import_backup` / `rewrap`；`warn`——`Denied`、`Overwrite` 导入、保留清理失败；`error`——`Corrupt`、`KeyMismatch`、`InsecurePermissions`。**不记录** `get` 的成功路径与任何值。
- 指标：`secrets_ops_total{op,consumer,result}`、`secrets_denied_total{consumer}`、`secrets_corrupt_total`、`secrets_staged_pending`、`secrets_refs{consumer}`、`secrets_oldest_age_seconds{consumer}`（轮换卫生）、`secrets_backup_last_success_timestamp`。
- 健康：`check_refs` 的结果由 `api` 汇入 `/v1/settings/status` 的 `modules.<name>.unresolved_secret_refs`（config.md §2.8）。

## 12. 测试与验收

所有测试用临时目录与随机主密钥；文件与内存两种实现共用同一套契约测试（`secrets_contract_tests!`），保证 `MemorySecrets` 与 `FileSecrets` 语义一致。

### 12.1 M1 退出标准中 `secrets` 负责的部分

- [ ] **沙盒内读不到任何模型 key**（与 `exec` 泳道联合验收）：库内没有任何导出到环境 / 挂载 / 文件的公开函数（用 `cargo public-api` 或等价检查，加测试断言公开 API 清单）；以另一个操作系统用户身份打开 `secrets.dir` / 读取其中任何文件失败；模型 key 只出现在 Egress 实例目录。
- [ ] **写入模型凭据需要 approver 设备签名**（`api` 泳道执行）：`secrets` 侧的配合验收——`stage` 后若 `settings.updated` 未提交，`abort` 使 `staged_all()` 为空且 `get` 仍返回旧值 / 无值；提交后 `commit`，`get` 返回新值、版本 +1。
- [ ] **引导期写入**（P-66）：第一台设备配对之前，主机命令以 `ScopedSecrets(Forge)` 写入 App 私钥与 webhook secret 成功，调用方追加审计事件；`secrets` 侧只校验与普通 `put` 相同的版本与权限规则（配对之后的拦截在 `api` / 主机命令一侧验收）。
- [ ] **加密备份**：`export_backup` 的输出中搜索任意已知哨兵明文，零命中；用正确身份 `import_backup(Fresh)` 到新实例（不同主密钥）后，所有条目的值与版本号逐一相同；错误身份无法解密。

### 12.2 存储与隔离

- [ ] 往返：`put` → `get` 值相同；`version` 从 1 起，覆盖写 +1；`present` 与 `get().is_some()` 一致。
- [ ] 篡改检测：翻转密文 / 头部 / 标签任意一个字节 → `Corrupt`；把 `forge` 目录下的文件复制到 `pki` 目录同名 → `Corrupt`（AAD 不符）；把 `v1.sec` 改名为 `v2.sec` → `Corrupt`。
- [ ] 隔离：只承载 `[Forge, Pki]` 的实例上 `reader(Egress)` → `Denied`；`reader(Forge).get` 取 `pki` 里存在的名字 → `Ok(None)`（命名空间不同）；`scoped(Egress)` 在该实例上 → `Denied`；`scoped(Forge).put` 不能写 `pki` 命名空间（接口里没有 `who` 参数，写入只落在自己的命名空间）；用 `gantryd` 实例的主密钥打开 Egress 的目录 → `KeyMismatch`。
- [ ] 权限：目录 `0755`、文件 `0644`、属主不符、符号链接目录 → `InsecurePermissions` / 拒绝；`open` 不修复。
- [ ] 版本：删除后再 `put`，版本号 > 删除前最大版本；`Expect::Absent` / `Version(n)` 冲突返回 `Conflict`；`retain_versions = 2` 时第三次写入后最旧版本被清理，`get_version` 对已清理版本返回 `None`。
- [ ] `stage` / `commit` / `abort`：暂存版本对 `get` / `present` / `list` 不可见；同名第二次 `stage` → `AlreadyStaged`；`commit` 后版本可见；`abort` 后无痕；崩溃注入（子进程 `SIGKILL`）在 `.tmp` 写完、`rename` 前后、`commit` 前后各一次，重开后 `.sec` 都是完整文件，`staged()` 报告遗留暂存项，`secrets` 不自动提交或丢弃。
- [ ] 并发：多线程同时 `put` 同名（带 `Expect::Any`），版本号恰好是 1..N 无重复无跳变；读者并发读取始终得到完整的某个版本，从不报 `Corrupt`。
- [ ] 值大小：恰好 `max_value_bytes` 通过，+1 → `TooLarge`；空值是否允许见 OQ-6，测试固化最终决定。

### 12.3 不泄露与内存

- [ ] 哨兵测试：用已知哨兵字符串作为值，执行 `put` / `get` / `stage` / `commit` / `delete` / `export_backup` / 全部错误路径；捕获 `tracing` 输出、所有错误的 `Debug` / `Display`、`panic` 信息、`SecretMeta` / `PutOutcome` / `StagedSecret` 的 `Debug`，断言均不含哨兵。
- [ ] `SecretValue` 的 `Debug` 为 `SecretValue(***)`；`trybuild` 失败用例：`SecretValue` 不能 `Clone` / `Serialize` / `format!("{}")`。
- [ ] `SecretValue` 丢弃后内存被零化（用测试用分配器 / 读取原指针的 `unsafe` 测试代码验证，测试代码允许 `unsafe`，库代码不允许）。
- [ ] `harden_process` 之后 `RLIMIT_CORE == 0`、`PR_GET_DUMPABLE == 0`。

### 12.4 备份、恢复、轮换

- [ ] 导出 → 恢复保留版本号；`Merge` 只写版本更高的条目；`Fresh` 对非空实例拒绝；墓碑随备份恢复，恢复后 `put` 不会复用已用版本号。
- [ ] 备份流被截断 / 篡改一个字节 → 恢复失败且不写入任何文件（先全部解密校验再落盘）。
- [ ] `check_refs` 返回恰好缺失的名字；暂存项、墓碑视为缺失。
- [ ] （M3）`SecretHandle`：版本变化后最迟 `refresh_interval_s` 内 `current()` 返回新值；`invalidate()` 立即生效；旧 `Arc<Secret>` 仍可用且最终零化。
- [ ] （M3）`rewrap`：之后旧主密钥无法 `open`；中途崩溃后重跑完成；所有值与版本不变。
- [ ] （M3）轮换窗口内 `get_version(old)` 可读，窗口后（超出 `retain_versions`）读不到。

### 12.5 契约一致性

- [ ] `MemorySecrets` 与 `FileSecrets` 通过同一套契约测试。
- [ ] `PutOutcome.previous` 使调用方能区分 `set`（`None`）与 `rotate`（`Some`）；`delete` 之后再 `put` 的 `previous` 为 `None`。

## 13. 开放问题

骨架没定、需要上报的点。"临时处理"只让本文自洽，不构成决定。

| # | 问题 | 本文临时处理 | 影响 |
| --- | --- | --- | --- |
| OQ-1 | **已决：见裁决 C4 / P-50**（经 Egress 控制面 `egress.sock` 写入 secret（只写不读）、查询 `present`；两阶段 `stage_secret` / `activate_secret` / `discard_secret` 与 `settings.updated` 配合）。原问题：**`api`（`gantryd`）→ Egress 的转交通道与原子性**。config.md §2.4 要求"同一次 `PUT` 里的 secret 写入与 `settings.updated` 要么都成功要么都不成功"，同时模型 key"只存在 Egress 的密钥存储、仅 Egress 进程用户可读，`api` 写入时只转交、不留存"。两个独立进程、不同用户之间如何传值、如何做两阶段提交、`present` 如何在 `gantryd` 里得知 Egress 实例里 ref 的存在、启动对账怎么做，**没有任何契约**。另：M0 替身表没有 `secrets`，而 `forge` / `pki` / `egress` 同期并行 | 已落实到 §2、§6；M0 替身另见 OQ-10 | `api`、`egress`、`deploy`、M1 全部 `sec` / `exec` 泳道 |
| OQ-2 | **主密钥保管方式与默认值**：systemd credential（可选 TPM 绑定）、仅属主可读的密钥文件、启动时口令。后两者对"已拿到服务进程用户代码执行"的攻击者都不构成保护 | 提供 `SystemdCredential` 与 `KeyFile` 两个实现，默认不定；口令方式不适合无人值守，不提供 | `deploy` |
| OQ-3 | **备份加密方案与接收方密钥保管**：本文提议 `age`（X25519 接收方）、接收方私钥离线保管（与根 CA 同样处理）。architecture §6 只写"加密备份" | 如提议；备份流格式见 §7.6 | `deploy`、运维手册 |
| OQ-4 | **已决：见裁决 C19 / C20（P-65、P-66）**（第一台设备配对之前，主机命令（需 root）可写入 `github.*` 等"仅 approver"配置并写审计事件；App Manifest 经 `gantry github setup` 与同一入口的 `/setup/github/callback` 写入；之后改为需要审批签名）。CLI 细节仍由 `deploy` 定。原问题：**写入入口**：`api` 的 settings 路由可写；但 `deploy` 的安装流程要在没有任何设备之前写入 App 私钥、webhook secret（App Manifest 回调）和 CA 中间私钥，需要一个主机侧入口（例如 `gantry secrets …` 子命令）。skeleton 只定义了 `gantry pair` 与 `gantry device recover` | 本库提供 `init` / `put`；离线操作（`init`、`import_backup`、`rewrap`）需在对应进程停止后执行，或经守护进程的管理入口 | `deploy`、`pki`、`forge` |
| OQ-5 | **ref 与使用方的对应**：config.md §2.4 允许"多个字段引用同一个 ref"，而本库按使用方隔离（同名 ref 在不同使用方命名空间里是不同的密钥）。是否在校验层禁止跨使用方共用同名 ref（core.md OQ-14）；`github.app.*` → `Forge`、`models.providers.*` 与 `egress.injection.*` → `Egress`、中间 CA 私钥 → `Pki` 的映射是否确认 | 按左侧映射；跨使用方同名视为校验错误 | `core`、`api` |
| OQ-6 | 空值与非 UTF-8 值：是否允许存空凭据；二进制凭据是否需要 | 拒绝空值；允许任意字节（PEM 恰好是 UTF-8） | `core`（V02/V03）、`api` |
| OQ-7 | **回收未被引用的 ref**（config.md §2.4 [提议]）由谁触发：引用关系分散在 settings 文档里，`secrets` 看不到 | `secrets` 只提供 `delete`；`api` 在 `settings.updated` 之后计算引用并调用 | `api` |
| OQ-8 | Egress 的**重载信号**（config.md §2.4"收到重载信号后换新"，M3）：信号形态（SIGHUP、管理接口、轮询 `version()`）未定 | `SecretHandle` 轮询 + `invalidate()`；信号绑定由 `egress` 做 | `egress`（M3） |
| OQ-9 | （引导期主机写入的审计**已决：见裁决 C20 / P-66**——写审计事件，由调用方追加；读取与 `deploy` 其余安装步骤是否进审计仍未决）secrets 读写是否要进入审计事件（事件目录里没有 `secret.*` 类型；`settings.updated.secret_changes[]` 只覆盖经 settings 的写入，不覆盖 `deploy` 安装流程与读取） | 不进事件日志；只有日志与指标 | `core`（事件目录）、`policy`（审计） |
| OQ-10 | ~~`secrets` 的 M0 替身~~ | 已决：见 decisions.md P-81（`MemorySecrets` 进 M0 替身清单，读写 trait 在 M0 冻结） | — |
