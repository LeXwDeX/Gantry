# 证书与配对（`pki`）

> 状态：设计 v0.1 · 里程碑：M1（主体，含设备证书续期路由与 `revocations.journal`）→ M3（密钥轮换、进程内即时断流）→ M4（iOS 配对）· 泳道：`sec` · crate / 目录：`crates/gantry-pki`

`pki` 把"这台设备是谁、还被不被信任"变成密码学事实：私有证书颁发机构（CA，离线根 + 在线中间 CA）签发服务端与设备证书；一次性令牌 + 离线根 CA 指纹固定完成首次配对，并登记设备的两把公钥（mTLS 客户端公钥与审批签名公钥 `approval_pubkey`，P-47）；握手时按吊销表拒绝；吊销另记一份不随备份回滚的 `revocations.journal`；设备丢了有主机应急命令兜底。它不处理任何业务 HTTP，也不替客户端保管私钥。

## 1. 职责

1. **CA 层次**：离线根 CA（安装时生成、导出到离线介质后从服务端删除）+ 在线中间 CA（私钥只经 `secrets` 由本模块读取）；提供安装时的引导工具（`bootstrap`，命令 `gantry pki bootstrap-ca`，名称 [提议]，deploy.md §7.3）供 `deploy` 调用（architecture §7.1 步骤 1）。
2. **服务端证书**：签发服务端叶子证书，到期前自动续签并热加载，不断开现有连接。
3. **配对**：`gantry pair` 的令牌与配对窗口；配对端点 `POST /v1/pair` 的全部业务逻辑（令牌核验、证书签名请求（CSR）校验、`approval_pubkey` 校验与登记、签发、创建 Device、确认码）；第一台设备为 `approver`、之后默认 `operator`（P-23）。HTTP 监听器由 `api` 持有，业务由本模块实现（[api.md §1.3](../interfaces/api.md)）。
4. **设备注册表**：Device 实体（名称、平台、`pubkey_fpr`、`approval_pubkey` / `approval_pubkey_fpr`、角色、状态、`last_seen`）、证书表、吊销表；事件 `device.paired` / `device.updated` / `device.revoked`。
5. **握手校验**：提供 rustls 的 `ClientCertVerifier`——证书链、有效期、增强密钥用途、算法、`cert_serial` 是否在册且未吊销（无证书吊销列表（CRL）与在线证书状态协议（OCSP），握手时直接查库）。
6. **角色与吊销**：`set_role`、`revoke`、重命名；对 `api` 暴露"在调用方事务内执行"的接口，使"消费审批 + 写业务"同事务（api.md §6.6）。
7. **吊销生效**：新握手立即被拒；已建立的事件流在 M1 靠 `api` 心跳内的设备状态检查在 ≤ 60 s 内断开，M3 起提供进程内即时通知（P-30）。
8. **吊销日志 `revocations.journal`**（P-67）：每次吊销除 `device.revoked` 事件外，再追加写入事件日志之外、只追加的 `revocations.journal`（`pki` 目录下的独立文件，不随备份回滚）；从备份恢复后重放晚于备份的吊销，被吊销的设备不会因恢复而复活（§7.6）。
9. **证书轮换与续期**：服务端叶子证书自动续签（M1）；中间 CA 轮换（M3，需要离线根）；设备证书续期 `POST /v1/devices/self/renew` 的业务（M1，P-64；客户端在剩余 1/3 有效期时自动发起）。
10. **主机应急命令** `gantry device recover`（P-39）：需主机 root，吊销设备或把一台设备提升为 approver，写审计事件。
11. **向 `policy` 提供验签公钥**：经 `api` 传递设备登记的**审批签名公钥** `approval_pubkey`（P-256，[api.md §6.3](../interfaces/api.md)，P-47）；mTLS 客户端公钥只用于握手，不用于审批验签。
12. **引导期判定**（P-66）：提供"是否仍处于引导期（从未配对过任何设备）"的查询，供主机命令判断能否不经签名写入"仅 approver"键（§6.2 `bootstrap_period`）。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 不交付实现。`TestCA`（测试用证书与设备）由 `contract` 泳道交付，供 `api`、`fe-desktop` 集成测试（milestones 替身表）；`schemas/events/device.*.json`。本模块在 M0 只落 crate 骨架与 trait |
| M1 | 全部主体：`bootstrap`（离线根 + 在线中间 CA + 服务端证书）、握手校验器、配对（令牌、窗口、CSR 与 `approval_pubkey` 校验、签发链 `[叶子, 中间 CA, 根]`、确认码、首台 approver / 之后 operator）、设备证书续期（`renew` 业务 + `POST /v1/devices/self/renew` 对接，P-64）、`revocations.journal` 与恢复后重放（P-67）、设备注册表与 `device.*` 事件、`set_role` / `revoke` / 重命名、`last_seen` 节流、服务端证书自动续签与热加载、`gantry pair`、`gantry device recover`、启动自检（根私钥不得在服务端） |
| M2 | 无新增功能；回归 |
| M3 | 中间 CA 轮换（需要离线根解锁）；`RevocationNotifier` 被 `api` 用于进程内即时断开事件流（M1 的心跳检查保留为兜底） |
| M4 | `platform = ios` 的配对（配对串需适合二维码；`approval_pubkey` 可省略，P-76）；与 `fe-mobile` 联调；手机档案设备永远不能成为 approver（P-76）：配对时角色至多 `operator`，`set_role` 与 `recover --promote` 对 `ios` 设备设 approver 一律拒绝（本模块强制，`api` 另在路由层拒绝） |

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| HTTP 监听、路由、限流、`Idempotency-Key`、角色 / 档案的路由级判定 | `api`；配对监听器的端口、开关由 `api` 按本模块给出的配对窗口执行 |
| 审批的创建、挑战（challenge）、验签、状态机、`subject_digest` 一致性 | `policy`。本模块只提供设备的审批签名公钥与角色 |
| `device_role_change` / `device_revoke` 审批的签名校验与消费 | `policy`（`consume_internal`）；`api` 在同一事务里调用本模块的 `set_role` / `revoke` |
| 客户端两把私钥（mTLS 客户端密钥、审批签名密钥）的生成与保管、CSR 生成、系统级确认、到期自动续期的发起 | `desktop-shell`、`mobile`（I6：服务端从不生成客户端私钥） |
| 密钥的加密存储与按使用方隔离读取（中间 CA 私钥、服务端叶子私钥） | `secrets`（本模块是其 `pki` 使用方，只能读自己的密钥） |
| 安装脚本、systemd 单元、根 CA 离线备份的操作引导与介质选择；备份 / 恢复命令的编排（恢复后调用本模块的 `replay_revocations`） | `deploy`（本模块提供可调用的 `bootstrap` 库函数与恢复后重放入口） |
| Webhook 入口的 TLS | `forge` / `deploy`（独立监听，不属于本 PKI 信任链的客户端侧，api.md §1.1） |
| 设备管理界面、续期提示、"证书将过期"的展示 | `desktop-shell` / `desktop-ui` |
| 事件流的实际断开动作 | `api` |
| 设备的 settings（证书有效期等是否做成运行期设置） | 不做（config.md OQ-18）：全部为引导配置 |

## 3. 依赖

本文中不带路径的 `api.md`、`config.md`、`forge-actions`、`worker-protocol` 均指 `docs/interfaces/` 下的契约；模块规格写作 `modules/<id>.md`，本规格自身称"本文"。

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `store` | `Store::begin_write()` → `WriteTx`（同一事务内 `append` `device.*` 事件 + `module_conn("pki")` 读写本模块表）；`snapshot()` → `ReadSnapshot`（`ProjectionRead::device`、`device_by_cert_serial`、`devices`）；`ModuleSpec { module: "pki", migrations }`。Device 的事件派生列（`name`、`platform`、`role`、`status`、`cert_serial`、`pubkey_fpr`）由 `store` 的 `devices` 投影维护（默认内置投影器，store.md §7.2、OQ-2），`last_seen` 等非事件列归本模块的表（store.md OQ-4） | [store.md §6、§7.10](store.md)；[data-model §3 Device、§5](../data-model.md) |
| `secrets` | 以使用方 `Consumer::Pki` 取 `ScopedSecrets`（`SecretSink`：`get` / `put` / `stage` / `commit` / `abort`）：中间 CA 私钥、服务端叶子私钥；加密存储、按使用方隔离 | [secrets.md §6](secrets.md)；architecture §4、§6；decisions P-38 |
| `core` | `Device`、`DeviceId`、`DeviceRole`（`Ord`：`approver > operator > viewer`）、`DevicePlatform`、`DeviceStatus`、`Actor`、事件枚举、`Clock`、`Entropy`、`Consumer` | [core.md §6.1、§6.3、§6.5](core.md)；[data-model §3 Device](../data-model.md) |
| 外部 crate（建议） | `rcgen`（签发）、`rustls` + `rustls-webpki`（握手校验）、`x509-parser`（解析与断言）、`p256`、`sha2`、`zeroize`、`subtle`、`qrcode`（终端二维码）、`arc-swap` | architecture §10 |
| 非依赖 | **不依赖** `api`、`policy`（反向被调用）；不依赖 `egress`、`forge` | architecture §4 |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| `PairRequest { token, csr_pem, approval_pubkey?, device{name, platform} }`（`approval_pubkey` 仅 `ios` 可省略，P-76） | `api` 的配对监听器（单向 TLS，未认证） | [api.md §1.3](../interfaces/api.md) | **完全不可信**：令牌、CSR、`approval_pubkey`、名称都要严格校验（§7.4） |
| `RenewRequest { csr_pem }` + 握手出示的证书序列号 | `api`（`POST /v1/devices/self/renew`，mTLS） | api.md §4.10 | 身份可信（已握手 + 在册）；CSR 内容不可信，按 §7.4.4 校验 |
| 客户端证书链（握手） | `api` 的双向 TLS（mTLS）监听器 | X.509，TLS 1.3 | 不可信，验证后才映射为 Device |
| `gantry pair` / `gantry device recover` 命令 | 主机管理员（终端） | §7.8；recover 需 root | 主机 root 可信；命令参数仍严格校验 |
| 调用方事务 `tx` + 操作者 `Actor` | `api`（`set_role` / `revoke`，审批已在同一 tx 被 `policy` 消费） | §6 | 可信（已通过 `api` 的角色与审批校验）；本模块对"不能改自己"等不变量再做一次防御性检查 |
| 引导配置 | `deploy` 写入的配置文件 | §10 | 可信 |
| `revocations.journal`（恢复后重放时读取） | 本模块自己写的本地文件 | §7.6 | 可信（服务用户独占、只追加）；逐行校验，坏行拒绝重放并告警 |
| 离线根 CA 私钥（仅 M3 轮换、仅一次性） | 管理员从离线介质提供（口令加密） | PKCS#8（加密） | 可信但高敏感：只在内存中使用，用后 `zeroize` |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `PairResponse`（`201`：`device`、`certificate_chain = [叶子, 中间 CA, 根]`、`api_base_url`、`ca_fingerprint`（离线根 CA 指纹）、`confirmation_code`） | `api` → 未配对设备 | api.md §1.3；P-64 |
| `RenewResponse`（`200`：`certificate_chain = [叶子, 中间 CA, 根]`、`not_after`） | `api` → 已配对设备 | api.md §4.10；P-64 |
| `Device` 表示（`id`、`name`、`platform`、`cert_serial`、`pubkey_fpr`、`approval_pubkey_fpr`、`role`、`status`、`last_seen`；API 只返回两把公钥的指纹） | `api`（`/v1/me`、`/v1/devices`） | api.md §4.2、§4.10；data-model Device |
| 事件 `device.paired` / `device.updated` / `device.revoked` | `store` | data-model §5.2；`data` Schema 在 `schemas/events/`（M0）；`device.*` 为全局事件，`project_id = null` |
| rustls `ClientCertVerifier`、`ResolvesServerCert`、根证书 PEM、CA 指纹 | `api`（构造 `ServerConfig`） | §6.4 |
| 设备审批签名公钥（`approval_pubkey`） | `api` → `policy` | `p256::ecdsa::VerifyingKey`（P-47） |
| `revocations.journal` 行 | 本地文件（不随备份回滚） | §7.6 |
| `RevocationNotifier`（M3） | `api` | §6.5 |
| 终端输出（配对串、二维码、确认码、设备名与指纹） | 管理员 | §7.4、§7.8 |
| 指标、日志 | 运维 | §11 |

## 6. 对外接口

Rust 签名草案。`Pki` 是对外的总入口，`gantryd` 装配时取各子 trait 分发给 `api` 等调用方。

### 6.1 总入口

```rust
pub struct Pki { /* 内部句柄，见 §7 */ }

impl Pki {
    /// 打开已引导的 PKI：载入根证书（公钥）、当前 / 上一个中间 CA、服务端证书（首次启动时从 `pki.ca_dir` 的 PEM
    /// 登记进 pki_ca / pki_server_cert，deploy.md §7.3 把它们放在 /etc/gantry/ca/）；
    /// 启动自检（§8）：服务端上发现与根证书公钥匹配的私钥 → Err(RootKeyPresent)，拒绝启动。
    pub async fn open(cfg: PkiConfig, store: Store, secrets: ScopedSecrets,   // = secrets.scoped(Consumer::Pki)
                      clock: Arc<dyn Clock>, entropy: Arc<dyn Entropy>) -> Result<Pki, PkiError>;

    pub fn directory(&self) -> Arc<dyn DeviceDirectory>;
    pub fn admin(&self) -> Arc<dyn DeviceAdmin>;
    pub fn pairing(&self) -> Arc<dyn PairingService>;
    pub fn tls(&self) -> TlsMaterial;
    pub fn revocations(&self) -> RevocationNotifier;          // M3 起被 api 使用
    /// 后台任务：服务端证书续签、配对会话到期（更新 window() 的 watch）、过期令牌清理
    pub fn spawn_background(&self, shutdown: CancellationToken) -> JoinSet<()>;
}
```

### 6.2 设备目录与管理

```rust
#[async_trait]
pub trait DeviceDirectory: Send + Sync {
    /// 鉴权热路径（api.md §2.1 步骤 2）：按 cert_serial 直接读库，不缓存
    /// （`ReadSnapshot::device_by_cert_serial` + 本模块 `pki_device_state.last_seen`）。
    /// 序列号不在册（哪怕链有效，如从旧备份恢复）→ None。
    async fn by_cert_serial(&self, serial: &CertSerial) -> Result<Option<Device>, PkiError>;
    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, PkiError>;
    async fn list(&self, page: PageReq) -> Result<Page<Device>, PkiError>;       // 含已吊销，键集（keyset）分页
    /// 审批验签用：返回配对时登记的审批签名公钥 approval_pubkey（P-47），不是 mTLS 证书里的公钥；
    /// 设备已吊销 → None。
    async fn verifying_key(&self, id: &DeviceId) -> Result<Option<VerifyingKey>, PkiError>;
    /// 引导期（P-66）：库里从未有过任何设备（含已吊销）时为 true。主机命令据此决定能否不经签名
    /// 写入 core::bootstrap_writable 允许的"仅 approver"键；第一台设备配对后恒为 false。
    async fn bootstrap_period(&self) -> Result<bool, PkiError>;   // 第一台设备是手机时引导期延续到第一台桌面设备配对（decisions D-18）
    /// 节流写入（≤ 每设备每 60 s 一次），不产生事件
    async fn touch_last_seen(&self, id: &DeviceId);
}

#[async_trait]
pub trait DeviceAdmin: Send + Sync {
    /// 1–64 字符，无控制字符；自成事务；事件 device.updated（changed: {name}）。operator 起（api 判）
    async fn rename(&self, id: &DeviceId, name: &str, actor: &Actor) -> Result<Device, DeviceError>;
    /// 在调用方事务内执行。actor.id == id → Err(SelfRoleChange)（api 映射 409 conflict / self_role_change）。
    /// 目标设备 platform = ios 且 role = Approver → Err(MobileCannotApprove)（409 conflict / mobile_cannot_approve，P-76）。
    /// 事件 device.updated（changed: {role: {from, to}}）。新角色对该设备的下一个请求立即生效。
    async fn set_role(&self, tx: &mut WriteTx, id: &DeviceId, role: DeviceRole, actor: &Actor)
        -> Result<Device, DeviceError>;
    /// 在调用方事务内执行；吊销该设备全部证书序列号。已吊销 → Ok(AlreadyRevoked)，不重复发事件。
    /// 允许吊销自己。事件 device.revoked；同时在提交前追加并 fsync 一行 revocations.journal（P-67，§7.6）。
    /// 不可逆：恢复必须重新配对（新 Device 记录）。
    async fn revoke(&self, tx: &mut WriteTx, id: &DeviceId, actor: &Actor)
        -> Result<RevokeOutcome, DeviceError>;
    /// POST /v1/devices/self/renew（P-64）：presented_serial 是本次握手出示的证书序列号（api 从连接取得）。
    /// 设备须 active、出示的证书有效；CSR 按 §7.4.4 校验（可沿用或更换 mTLS 客户端密钥）；approval_pubkey 不变。
    async fn renew(&self, id: &DeviceId, presented_serial: &CertSerial, req: RenewRequest)
        -> Result<RenewResponse, RenewError>;
    /// 从备份恢复后由 deploy 的恢复命令调用（store::restore_from 之后、gantryd 启动之前）：
    /// 重放 revocations.journal 中库里尚未生效的吊销（幂等）。
    async fn replay_revocations(&self) -> Result<ReplayReport, PkiError>;
}
pub enum RevokeOutcome { Revoked(Device), AlreadyRevoked(Device) }
pub struct RenewRequest { pub csr_pem: String }
pub struct RenewResponse { pub certificate_chain: [String; 3] /* [叶子, 中间 CA, 根] */, pub not_after: Timestamp }
pub enum RenewError { Validation(Vec<FieldError>) /* 422 */, Revoked /* 401 device_revoked */, Internal }
pub struct ReplayReport { pub replayed: Vec<DeviceId>, pub already_revoked: u32, pub unknown_devices: u32 }
```

### 6.3 配对

```rust
#[async_trait]
pub trait PairingService: Send + Sync {
    // ---- 主机侧（gantry pair，root 或服务用户；见 §7.8）----
    /// 创建配对会话并开启窗口。明文 token 只在返回值里出现这一次（库里只存 SHA-256）。
    /// 新会话作废此前所有 open 会话。
    async fn open_session(&self, opts: OpenSession, host: &HostActor) -> Result<PairingTicket, PkiError>;
    async fn cancel_session(&self, id: &PairingSessionId, host: &HostActor) -> Result<(), PkiError>;
    /// CLI 等待结果并打印设备名 / 公钥指纹 / 确认码；超时返回 Expired
    async fn wait_outcome(&self, id: &PairingSessionId, timeout: Duration) -> Result<PairingOutcome, PkiError>;
    /// api 据此开关配对监听器：Some(窗口) = 开，None = 关
    fn window(&self) -> tokio::sync::watch::Receiver<Option<PairingWindow>>;

    // ---- 网络侧（POST /v1/pair）----
    async fn redeem(&self, req: PairRequest, peer: PeerInfo) -> Result<PairResponse, PairError>;
}

pub struct OpenSession { pub ttl: Duration /* 默认 10 min */, pub api_base_url: Url, pub pair_base_url: Url }
pub struct PairingTicket {
    pub session_id: PairingSessionId,
    pub token: SecretString,                    // gp1_…，仅此一次可见
    pub pairing_string: String,                 // §7.4 的"配对串"（格式待契约，OQ-K3）
    pub ca_fingerprint: String,                 // "sha256:" + 64 位十六进制（离线根 CA 证书 DER 的 SHA-256，P-64）
    pub expires_at: Timestamp,
}
pub struct PairRequest {
    pub token: String, pub csr_pem: String,
    pub approval_pubkey: Option<String>,        // P-256 未压缩点的 base64url（无填充）；须与 CSR 公钥不同（P-47）；仅 platform = ios 时可为 None（P-76）
    pub device: NewDevice,
}
pub struct NewDevice { pub name: String, pub platform: DevicePlatform }       // windows | macos | ios
pub struct PairResponse {
    pub device: Device,
    pub certificate_chain: [String; 3],         // [叶子 PEM, 中间 CA PEM, 根 PEM]（P-64）
    pub api_base_url: Url, pub ca_fingerprint: String, pub confirmation_code: String,   // 6 位数字
}
pub enum PairError {
    TokenInvalid,                               // 无效 / 已用 / 过期 / 作废：对外统一 401 pair_token_invalid，不给枚举 oracle
    Validation(Vec<FieldError>),                // 422：csr / approval_pubkey / device.name / device.platform / 未知字段
    Internal,                                   // 500，细节只进日志
}
```

### 6.4 TLS 材料（`api` 构造 `ServerConfig` 用）

```rust
pub struct TlsMaterial {
    pub client_verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,   // §7.3
    pub server_cert_resolver: Arc<dyn rustls::server::ResolvesServerCert>,      // ArcSwap，续签后热替换
    pub root_cert_pem: String,
    pub ca_fingerprint: String,
}
```

`api` 必须用这两件拼 `ServerConfig`，并满足契约 [interfaces/api.md §1.2](../interfaces/api.md) 与模块规格 [modules/api.md §7.2](api.md) 的加固要求（仅 TLS 1.3、禁会话票据、禁零往返数据（0-RTT））。

### 6.5 吊销通知（M3）

```rust
pub struct RevocationNotifier(tokio::sync::broadcast::Sender<DeviceId>);
impl RevocationNotifier { pub fn subscribe(&self) -> broadcast::Receiver<DeviceId>; }
// 由本模块内部订阅 `device.revoked`（`EventLog::tail`，事件提交之后才可见，回滚不会触发）后广播，因此不需要调用方事务的提交钩子。
// 接收方落后（Lagged）时必须退回"全量检查所有流的设备状态"。
```

### 6.6 引导、轮换、应急命令

```rust
pub mod bootstrap {
    /// 对应 `gantry pki bootstrap-ca`（deploy.md §7.3 的步骤 3；命令名 [提议]）。
    /// 在 params.out_dir（必须是内存盘：tmpfs / ramfs，否则拒绝）里一次产出：
    ///   root.key.enc（口令加密 PKCS#8）、root.crt、intermediate.key、intermediate.crt、server.key、server.crt。
    /// 根私钥只在内存里生成与加密，不写其它位置；返回前 zeroize。
    /// 之后的导出到离线介质、清除内存盘、把 intermediate.key / server.key 交给 `secrets`、
    /// 把三张证书放进 pki.ca_dir，由 deploy 的脚本编排（deploy.md §7.3 步骤 4–8）。
    pub fn run(params: BootstrapParams, clock: &dyn Clock, entropy: &dyn Entropy) -> Result<BootstrapReport, PkiError>;
    pub struct BootstrapParams {
        pub server_names: Vec<String>,            // 服务端叶子证书的主体备用名（Subject Alternative Name, SAN；DNS 名 / IP）
        pub root_passphrase: SecretString,        // 加密 root.key.enc
        pub install_id: String, pub out_dir: PathBuf,
    }
    pub struct BootstrapReport { pub root_fingerprint: String, pub intermediate_serial: String, pub server_cert_serial: String }
}
pub mod rotate {        // M3
    pub async fn rotate_intermediate(pki: &Pki, root_key_encrypted: &[u8], passphrase: SecretString)
        -> Result<RotateReport, PkiError>;
}
pub mod recover {       // gantry device recover
    pub enum RecoverCmd { List, Revoke { device_id: DeviceId }, Promote { device_id: DeviceId } }
    /// env.euid 必须为 0，否则 Err(NotRoot)。执行前降权到服务用户后再打开数据库（避免留下 root 属主文件）。
    pub async fn run(cmd: RecoverCmd, env: &HostEnv, store: &Store, clock: &dyn Clock)
        -> Result<RecoverOutcome, RecoverError>;
}
```

### 6.7 代码布局（建议）

`src/{lib.rs, config.rs, ca.rs, bootstrap.rs, issue.rs, csr.rs, pairing.rs, verifier.rs, tls.rs, directory.rs, admin.rs, recover.rs, rotate.rs, revocation.rs, store_schema.rs}`；`tests/` 下按 §12 分文件。

## 7. 内部设计

### 7.1 信任层次与证书参数

信任链：**离线根 CA → 在线中间 CA → 服务端叶子 / 设备叶子**（P-06）。下表所有取值除"强制"外都是**建议值**（骨架只定了层次、P-256 倾向与 TLS 1.3，数值来自 design.md 的示例或本文提议），统一在 `PkiConfig`（§10）里，不散落。

| 项 | 取值 | 说明 |
| --- | --- | --- |
| 公钥算法（强制） | ECDSA P-256（secp256r1），所有 CA 与叶子；`approval_pubkey` 也必须是 P-256 | design.md；api.md 的 ES256（ECDSA P-256 + SHA-256）审批签名要求审批签名公钥是 P-256（§6.3）。CSR 或 `approval_pubkey` 带其他算法 → 拒绝 |
| 签名算法 | `ecdsa-with-SHA256` | — |
| TLS | 仅 TLS 1.3（强制，api.md §1.2）；握手签名方案仅 `ECDSA_NISTP256_SHA256` | 由 `api` 的 `ServerConfig` 固定（modules/api.md §7.2） |
| 根 CA | 自签；有效期 10 年 [建议]；`basicConstraints: CA:TRUE, pathlen:1`（critical）；`keyUsage: keyCertSign, cRLSign`（critical）；`CN = "Gantry Root CA <install_id>"` | 私钥导出到离线介质（口令加密 PKCS#8），服务端删除；**服务端只保留根证书（公钥）** |
| 中间 CA | 根签发；有效期 2 年 [建议]；`CA:TRUE, pathlen:0`（critical）；`keyUsage: keyCertSign, cRLSign`；`CN = "Gantry Intermediate CA <序号>"` | 私钥经 `secrets` 以 `pki` 身份读取，仅签发进程可见；M3 轮换 |
| 服务端叶子 | 中间 CA 签发；有效期 90 天 [建议]，剩余 ≤ 30 天自动续签；`CA:FALSE`；`keyUsage: digitalSignature`；`EKU: serverAuth`；SAN = `PkiConfig.server_names`（DNS / IP，来自 `deploy`），`CN` = 第一个名字 | 私钥由 `secrets` 保管；`notAfter` 不得晚于中间 CA 的 `notAfter` |
| 设备叶子 | 中间 CA 签发；有效期 30 天 [建议，design.md 示例]，客户端在剩余 1/3 有效期时经 `renew` 自动续期（P-64）；`CA:FALSE`；`keyUsage: digitalSignature`；`EKU: clientAuth`；**无 DNS / IP SAN**；`CN = "<device.id>"`（服务端重写，api.md §1.3）；可加 URI SAN `urn:gantry:device:<device.id>` [建议] | **只取 CSR 里的公钥**，CSR 请求的任何扩展、subject、SAN 一律忽略 |
| 生效时间 | `notBefore = 签发时刻 − 5 分钟` [建议] | 两端时钟偏差余量（design.md） |
| 序列号 | 每张证书 128 位 CSPRNG 随机数，最高位清零保证为正，表示为小写十六进制、去前导零 [建议]；全库唯一（冲突则重新生成） | 与 api.md 示例（20 位十六进制）形态兼容；格式由 api.md OQ-10 最终确认（OQ-K2） |
| 公钥指纹 `pubkey_fpr` / `approval_pubkey_fpr` | `"sha256:"` + 小写十六进制（mTLS 客户端公钥 / 审批签名公钥的主体公钥信息（SubjectPublicKeyInfo, SPKI）DER 编码的 SHA-256） | api.md 示例形态；SPKI 还是裸点由 OQ-K2 确认 |
| CA 指纹（配对串里的 `ca_fingerprint`） | `"sha256:"` + 小写十六进制（**离线根 CA** 证书 DER 的 SHA-256） | 客户端固定根而非中间 CA，使中间 CA 轮换对已配对设备无感；配对与续期响应的 `certificate_chain` 为 `[叶子, 中间 CA, 根]`，客户端核对根指纹与固定值一致（P-64） |
| 随机数 | `OsRng`；令牌、序列号、确认码都不得用可预测来源 | — |

服务端在 TLS 握手中发送的链为 `[服务端叶子, 中间 CA, 根]`（含根，供客户端核对根指纹）；设备证书的 `certificate_chain` 响应字段同样为 `[叶子, 中间 CA, 根]`（P-64）。

### 7.2 持久化

事件是事实源。Device 的事件派生列由 `store` 的 `devices` 投影维护（`UNIQUE(cert_serial)`，store.md §7.2）；本模块只拥有下列表（`ModuleSpec { module: "pki", migrations }`，表名前缀 `pki_`，与事件在同一写事务里经 `WriteTx::module_conn("pki")` 读写；私钥不进数据库）：

| 表 | 列（要点） | 说明 |
| --- | --- | --- |
| `pki_ca` | `kind`（`root` / `intermediate`）、`serial`、`fingerprint`、`not_before`、`not_after`、`status`（`current` / `previous` / `retired`）、`cert_pem` | 根只存证书；中间 CA 私钥在 `secrets` |
| `pki_server_cert` | `serial`、`not_before`、`not_after`、`chain_pem`、`status` | 私钥在 `secrets` |
| `pki_device_certs` | `serial`（主键）、`device_id`、`not_before`、`not_after`、`status`（`active` / `superseded` / `revoked`） | 吊销表；握手时按 `serial` 查询。`devices.cert_serial` 只记当前序列号，续期宽限内的旧序列号靠这张表 |
| `pki_device_state` | `device_id`（主键）、`pubkey_spki`（mTLS 客户端公钥 DER，重复公钥检查用）、`last_seen` | 非事件列：`last_seen` 不进事件日志（避免洪泛；备份恢复后可能回退，无业务后果）；`pubkey_spki` 是公开信息，随 `device.paired` 事件的同一事务写入。审批签名公钥 `approval_pubkey` 是 Device 的事件派生列（data-model Device），随 `device.paired` 进入 `devices` 投影，验签从投影读 |
| `pki_pairing_sessions` | `id`、`token_sha256`、`created_at`、`expires_at`、`failed_attempts`、`state`（`open` / `redeemed` / `voided` / `expired`）、`redeemed_device_id?`、`confirmation_code?`、`created_by_uid` | 配对会话；明文 token 永不落库 |

`device.paired` / `device.updated` 事件的 `data` 必须带上 `store` 的 `devices` 投影需要的全部事件派生列（`name`、`platform`、`role`、`cert_serial`、`pubkey_fpr`、`approval_pubkey`、`approval_pubkey_fpr`；续期换证或换钥时 `device.updated` 带新的 `cert_serial` / `pubkey_fpr`），具体 JSON Schema 在 `schemas/events/`（M0）；若 `store` 最终让本模块接管 `devices` 投影（store.md OQ-2），则这些列迁入 `pki_*` 表，行为不变。

### 7.3 握手校验器 `ClientCertVerifier`

按顺序，任一步失败即握手失败（对客户端表现为 TLS 告警，**不产生 HTTP 响应**；失败原因只记本地日志）：

1. **链**：用 `rustls-webpki` 以根证书为信任锚，验证 `[叶子, 中间 CA]`，时间取 rustls 的 `TimeProvider`（由注入的 `Clock` 实现，测试可控）；增强密钥用途须含 `clientAuth`；叶子 `CA:FALSE`；公钥为 P-256，签名算法在白名单内。
2. **中间 CA 钉扎**：出示的中间 CA 的指纹必须属于 `pki_ca` 中 `current` 或 `previous` 的集合（使泄露的中间 CA 可被单独退役，M3）。
3. **身份格式**：叶子 `CN` 形如 `dev_<ULID>`。
4. **吊销 / 在册检查**：用叶子 `serial` 查 `pki_device_certs`（读库，**不加带存活时间（TTL）的缓存**）：行必须存在，状态为 `active`，或为 `superseded` 且未过原 `notAfter`（续期后旧证书到期前仍可用，api.md §4.10）；再经同一 `ReadSnapshot` 读所属设备（`ProjectionRead::device`），`status = active`。**序列号不在册 → 拒绝**（失败即关闭：如从旧备份恢复后，链上有效但库里没有的证书不放行）。
5. 通过后不在 verifier 里放任何业务状态；`api` 从 `peer_certificates()` 取叶子序列号，再用 `DeviceDirectory::by_cert_serial` 实时映射到 Device（每个请求一次，api.md §2.1）。

要求：`offer_client_auth = true`、`client_auth_mandatory = true`（无证书直接失败）；`root_hint_subjects` 给出中间 CA 的 subject。

### 7.4 配对

#### 7.4.1 会话与令牌

- `gantry pair`（主机，§7.8）→ `PairingService::open_session`：创建 `pki_pairing_sessions` 行，`state = open`；新会话**作废**此前仍 `open` 的会话（同一时刻至多一个有效令牌）。
- 令牌：`"gp1_"` + base64url(16 字节 CSPRNG)（128 位熵）[建议；api.md 示例形态 `gp1_…`，令牌是不透明字符串]；库里只存 `SHA-256(token)`；比较用恒定时间；日志只记哈希前 8 位十六进制。
- 有效期：10 分钟 [建议，design.md]；单次有效；**失败 5 次即作废** [建议]：持有未匹配令牌的请求使"当前 open 会话"的 `failed_attempts` +1（无法归因到具体会话时记在当前会话上），达到上限 → `voided`。这也意味着攻击者可用 5 次错误猜测烧掉合法令牌（拒绝服务）；代价只是管理员重新运行 `gantry pair`，换取"不给无限猜测"的安全，属有意取舍。
- 配对窗口：存在 `state = open` 且未过期的会话时，`window()` 返回 `Some`，`api` 开启配对监听器；窗口外该端口关闭（api.md §1.3；是否按需开启 OQ-12 → OQ-K3）。

#### 7.4.2 配对串

"地址 + CA 指纹 + 一次性 token"（architecture §7.1 步骤 4）。**具体编码契约未定**（OQ-K3）；本文建议一个可复制粘贴、也适合二维码的 URL 形式，由主机在终端同时打印文本与终端二维码：

```
gantry+pair://<pair_host>:<pair_port>/?v=1&fp=sha256:<64 位十六进制>&t=<token>
```

`api_base_url` 不在串里：配对响应会返回它（api.md §1.3）。

#### 7.4.3 `redeem` 流程（每一步失败的外部表现见括号）

1. **解析**：请求体严格解析，未知字段 / 类型错 → `Validation`（`422 validation_failed`）；整体大小限制由 `api` 在入口强制（建议 16 KiB）。
2. **令牌**：`SHA-256(token)` 与当前 `open` 会话比对（常量时间）。无会话 / 不匹配 / 已用 / 过期 / 已作废 → 一律 `TokenInvalid`（`401 pair_token_invalid`，**响应字节完全相同**）；不匹配时按 §7.4.1 计失败次数。
3. **审批签名公钥**：`platform ∈ {windows, macos}` 时必带，缺失 → `Validation`；`platform = ios` 时可省略（P-76，省略则 Device 不登记审批公钥，`approval_pubkey_fpr = null`）。给出时为 base64url（无填充）的 P-256 未压缩点（65 字节，首字节 `0x04`），点必须在曲线上；与 CSR 的公钥相同 → `Validation`（指针 `/approval_pubkey`，P-47）；与任一 `active` 设备已登记的 `approval_pubkey` 相同 → `Validation`。
4. **设备字段**：`name` 1–64 字符、不含控制字符与换行；`platform ∈ {windows, macos, ios}`（`ios` 在 M4 前也接受并如实记录）→ 否则 `Validation`。
5. **CSR**：见 §7.4.4。→ 否则 `Validation`（指针 `/csr`）。令牌在 CSR 校验失败时**不**被消耗（令牌只在签发成功后失效），但失败计入第 2 步之外的限流（`api` 按源 IP，5/min）。
6. **单一写事务**：
   1. 角色：同一写事务里 `tx.read().devices(limit 1)` 为空（**含已吊销的设备也算存在**）→ `approver`，否则 `operator`（P-23）；但 `platform = ios` 恒为 `operator`（P-76，即使是第一台；此时安装引导期持续到第一台桌面设备配对，由它成为 approver，decisions D-18）。单写者串行化保证"第一台"唯一（并发两次首配对只会有一台 `approver`）。该口径对"唯一 approver 被吊销后重新配对"的后果见 OQ-K11。
   2. 生成 `dev_<ULID>`、序列号；用中间 CA 私钥签发设备叶子（§7.1）。
   3. 追加 `device.paired`：`data = {device_id, name, platform, role, cert_serial, pubkey_fpr, approval_pubkey, approval_pubkey_fpr}`（`store` 的 `devices` 投影据此落库，`status = active`）；`actor = {kind: "system", id: "pki"}`；`project_id = null`。
   4. 同一事务经 `module_conn("pki")` 写 `pki_device_certs`（`active`）、`pki_device_state`（`pubkey_spki`）；会话置 `redeemed`，写入 `redeemed_device_id` 与 `confirmation_code`。
7. **响应**：`201`，`certificate_chain = [叶子, 中间 CA, 根]`，`ca_fingerprint`（离线根 CA 指纹），`api_base_url`，`confirmation_code`。令牌消耗后不可重放；响应丢失 → 只能重新 `gantry pair`（本端点不支持 `Idempotency-Key`，api.md §1.3）。
8. **主机侧**：`wait_outcome` 看到 `redeemed` 后，终端打印"设备名（已去除控制字符与 ANSI 转义）· 平台 · `pubkey_fpr` · `approval_pubkey_fpr` · 确认码"，管理员与设备屏幕上的确认码核对。

#### 7.4.4 CSR 校验（强制）

| 检查 | 规则 |
| --- | --- |
| 编码 | 恰好一个 PEM `CERTIFICATE REQUEST` 块；大小 ≤ 4 KiB；无尾随垃圾 |
| 自签名 | 验证 CSR 签名（持有私钥证明）；失败即拒绝 |
| 公钥 | 必须是 ECDSA P-256（OID `id-ecPublicKey` + `prime256v1`）；RSA / Ed25519 / P-384 一律拒绝；公钥点必须在曲线上（解析时校验），非未压缩点格式拒绝 |
| 签名算法 | `ecdsa-with-SHA256` |
| 请求的扩展 / subject / 属性 | **全部忽略**（尤其 `basicConstraints`、`keyUsage`、`extKeyUsage`、`subjectAltName`、`challengePassword`）；签发的叶子参数完全由服务端按 §7.1 构造。实现上用公钥 + 固定参数新建证书，**不得**从 CSR 复制 `CertificateParams` |
| 重复公钥 | 同一公钥已属于 `active` 设备 → 拒绝（避免一个密钥两个 Device）；与本请求的 `approval_pubkey` 相同 → 拒绝（两把密钥必须分开，P-47） |

#### 7.4.5 确认码

6 位十进制数，CSPRNG 均匀取值（`0..1_000_000`，左补零），在 `redeem` 事务里生成并存入会话；在 `201` 响应与主机终端各出现一次。它的作用是让管理员确认"主机登记的这台设备就是手里这台"（api.md §1.3、architecture §7.1 步骤 5）。

**按 api.md 现状，设备在 `201` 时即为 `active`，没有 confirm / abort 路由**：核对不一致时的处置是吊销（`gantry device recover Revoke`，或已有 approver 经 API）。是否要"核对后才激活"属开放问题（OQ-K4，即 api.md OQ-12）。

### 7.5 设备管理

- **角色**：`viewer` ⊂ `operator` ⊂ `approver`（api.md §2.2）。`set_role` 与 `revoke` 只在调用方事务里执行；调用方（`api`）已在同一事务里让 `policy` 消费了 `device_role_change` / `device_revoke` 审批。本模块的防御性检查：`actor.id == id` 的 `set_role` → `SelfRoleChange`；目标不存在 → `NotFound`；`revoke` 对已吊销设备幂等。
- **最后一台 approver**：契约允许吊销自己（包括唯一的 approver），结果是无人能签；恢复手段只有 `gantry device recover`（P-39）。本模块不加额外限制，`device.revoked` 事件的 `data` 带 `remaining_approvers` 计数供 UI 提示（OQ-K5）。
- **角色立即生效**：`by_cert_serial` 每次读库返回最新角色，没有任何进程内角色缓存。
- **`last_seen`**：`touch_last_seen` 在内存里记每设备上次写入时间，间隔 < 60 s 则跳过；写库不发事件。
- **重命名**：自成事务，事件 `device.updated`（`changed: {name}`）。

### 7.6 吊销与断流

- `revoke` 事务内：追加 `device.revoked`（`data`：`device_id`、`cert_serials[]`、`remaining_approvers`、可选 `via`；`store` 的 `devices` 投影据此置 `status = revoked`），并把该设备全部 `pki_device_certs.status` 置 `revoked`。事件提交之后由本模块订阅 `device.revoked` 并广播 `RevocationNotifier`。
- **`revocations.journal`**（P-67、P-80）：在 `revoke` 事务提交**之前**，向 `<pki.data_dir>/revocations.journal` 追加一行并 `fsync`，并**同步复制到备份目标**（`deploy` 的引导配置 `backup.journal_target`，同样追加并 `fsync`；复制失败 → 整个吊销失败）：`{v:1, device_id, cert_serials[], approval_pubkey_fpr, revoked_at, actor, via?, log_id, log_epoch}`（JSON Lines，`O_APPEND`，服务用户 0600）。追加失败 → 整个吊销失败（失败即关闭，审批保持 `granted` 可重试）。事务随后回滚的情形下文件里会多出一行：重放时会把这台设备也吊销——偏向安全，且该吊销本就是已签名审批所要的结果。文件只追加、从不改写；**不放进 `store` 的备份副本，也不随恢复被覆盖**（P-80：事务回滚也不删除已写条目；`deploy` 的恢复命令必须保留现有文件，architecture §6）；主机应急命令 `recover --revoke` 同样写入。
- **恢复后重放**：`deploy` 的恢复命令在 `Store::restore_from` 之后、启动 `gantryd` 之前调用 `replay_revocations`（离线模式，§7.8）：逐行读取（本地文件与备份目标上的副本取并集，按行去重），重放 `revoked_at` 晚于备份时刻的条目（P-80；更早的条目已在备份里生效），对恢复后的库里仍为 `active` 的设备执行与 `revoke` 相同的写入（`device.revoked`，`data.via = "journal_replay"`，`actor = {system, pki}`）；库里不存在的设备（备份早于其配对）跳过并计数；已吊销的跳过。重放本身幂等，也会在每次 `Pki::open` 时以只读方式核对一次，发现未生效的吊销则拒绝启动并提示运行恢复命令。
- **新握手**：下一次握手即被拒（§7.3 第 4 步，直接读库，没有缓存可过期）。**会话恢复与 0-RTT 会绕过握手时的证书检查**，所以 `api` 的 `ServerConfig` 必须关闭会话票据与会话缓存、`max_early_data_size = 0`（见 [modules/api.md §7.2](api.md)）；本模块的验收含一个"吊销后无法恢复会话"的联调测试。
- **已建立的连接**：HTTP 请求在每个请求的步骤 2 复查 `Device.status`；事件流 M1 由 `api` 在每次心跳（15 s）读 `directory.get`，≤ 60 s 内断开（P-30）；M3 起 `api` 订阅 `RevocationNotifier` 即时断开，心跳检查保留为兜底。

### 7.7 证书续期与轮换

| 对象 | 机制 | 里程碑 |
| --- | --- | --- |
| 服务端叶子 | 后台任务每 6 小时检查；剩余 ≤ 30 天或 SAN 配置变化时，用中间 CA 签发新叶子，写 `secrets` 与 `pki_server_cert`，`ArcSwap` 替换 `ResolvesServerCert`；旧连接不受影响。客户端固定根，续签对其无感 | M1（不做则系统在 90 天后不可用） |
| 设备叶子（续期） | `DeviceAdmin::renew`，对应 `POST /v1/devices/self/renew`（mTLS，viewer 起，所有档案，P-64）：设备须 `active` 且出示的证书有效；CSR 按 §7.4.4 校验，可沿用或更换 **mTLS 客户端密钥**（换新则更新 `pubkey_spki` / `pubkey_fpr`、发 `device.updated`）；**审批签名密钥不受影响**（`approval_pubkey` 不变）；签发新叶子，响应链 `[叶子, 中间 CA, 根]`；旧证书置 `superseded`，在其原 `notAfter` 之前仍可握手（客户端换证前的在途连接不断）；吊销作用于该设备全部序列号。客户端在剩余 1/3 有效期时自动发起（建议值） | M1 |
| 中间 CA | 需要离线根：管理员提供加密的根私钥导出文件与口令，`rotate_intermediate` 在内存中解开、签发新中间 CA、立即 `zeroize`；旧中间 CA 状态 `previous`，在其已签发证书的最长剩余寿命后（建议设备证书有效期 + 1 天）转 `retired`；紧急情况（中间 CA 泄露）可立即 `retired`，其签发的全部证书失效，设备需重新配对 | M3 |
| 根 CA | 不轮换（有效期 10 年）；到期前重新引导并要求全部设备重新配对 | — |

### 7.8 主机命令与守护进程的会合

`gantry pair` 与 `gantry device recover` 是**主机上的独立进程**。`store` 在数据目录上持有排他文件锁，一个数据目录只能有一个打开它的进程（store.md §7.1，第二个得到 `AlreadyRunning`），所以守护进程运行时 CLI **不能**直接打开数据库。deploy.md §6.1 假定 CLI 经本机控制通道 `/run/gantry/control.sock`（仅 root）与 `gantryd` 通信，并把 `pair` / `device recover` 的具体通道交给本规格（deploy.md OPS-07）。本规格的方案（待 OQ-K7 确认）：

| 模式 | 条件 | 做法 |
| --- | --- | --- |
| 在线 | `control.sock` 可连 | CLI 把命令发给 `gantryd` 内的 `PkiControl` 服务，由守护进程用自己的 `Store` 句柄执行；`pair` 的结果（设备名 / 指纹 / 确认码）经同一连接流回 CLI 打印；配对窗口就是进程内的 `watch`（无需轮询） |
| 离线 | 守护进程未运行（拿得到数据目录的排他锁） | `recover` 直接 `Store::open`（降权到服务用户，避免 root 属主的 WAL / SHM 文件）执行并退出；`pair` 离线无意义（没有配对监听器），报错 |

```rust
/// gantryd 注册到 control.sock 的 pki 命令集；传输与鉴权（SO_PEERCRED 要求 uid 0、文件权限 0600）归 gantryd / deploy
#[async_trait]
pub trait PkiControl: Send + Sync {
    async fn pair_open(&self, opts: OpenSession, host: HostActor) -> Result<PairingTicket, PkiError>;
    async fn pair_wait(&self, id: PairingSessionId, timeout: Duration) -> Result<PairingOutcome, PkiError>;
    async fn pair_cancel(&self, id: PairingSessionId, host: HostActor) -> Result<(), PkiError>;
    async fn device_list(&self) -> Result<Vec<Device>, PkiError>;
    async fn device_recover(&self, cmd: RecoverCmd, host: HostActor) -> Result<RecoverOutcome, RecoverError>;
}
```

两种模式的 `recover` 共用 `recover::run` 的同一份业务实现（离线时直接传 `Store`，在线时由守护进程调用）。控制套接字是主机上仅 root 可达的本地文件套接字，沙盒看不到它，不违反 I1；它与 `/v1/*` 的 mTLS API 无关。

`gantry device recover`：

| 项 | 规则 |
| --- | --- |
| 权限 | 要求 `geteuid() == 0`，否则拒绝（退出码非 0），不触碰数据库或套接字 |
| 子命令（形态建议，骨架只定了命令名；deploy.md §7.13 写作 `--revoke <id>` / `--promote <id>`） | `list`（列出设备 id / 名称 / 平台 / 角色 / 状态 / `last_seen` / 指纹，deploy.md 写作 `gantry device list`）；`--revoke <device_id>`；`--promote <device_id>`（把一台 `active` 的桌面档案设备提升为 `approver`；`ios` 设备 → 拒绝，`mobile_cannot_approve`，P-76） |
| 确认 | 交互终端上要求输入目标设备 id 才执行；`--yes` 跳过（脚本用） |
| 事件 | `revoke` → `device.revoked`；`promote` → `device.updated`（`changed: {role: {from, to}}`）；`actor = {kind: "system", id: "host_recover"}`，`data` 带 `via: "host_recover"`、`host_uid`、`sudo_user`（来自 `SUDO_USER`，仅供审计参考，不作为授权依据）。事件类型是否需要专门的 `device.recovered`：OQ-K7 |
| 幂等 | 已吊销再吊销 → 成功且不重复发事件；已是 approver 再提升 → 成功且不发事件 |
| 不做 | 不签发证书、不创建设备、不碰根 CA；恢复访问 = `promote` 一台已配对设备，或 `gantry pair` 重新配对（此时库中已有设备记录，新设备默认是 `operator`，再 `promote`） |

### 7.9 并发模型

- 所有写入经 `store` 的 `WriteTx`（SQLite 单写者串行化）；读取走 `ReadSnapshot`。无进程内可变业务状态，除：服务端证书的 `ArcSwap`、`last_seen` 节流表、`RevocationNotifier`。
- 握手校验器在 tokio 阻塞池之外运行：只做 CPU 小计算 + 一次本地 SQLite 点查（微秒级）；用独立的只读连接池，避免被写事务阻塞握手。
- 配对 `redeem` 是低频路径；签发（P-256 签名）在 `spawn_blocking` 中执行。

## 8. 不变式与安全要求

本模块要保证 [architecture.md §5](../architecture.md#5-信任边界与不变式) 中的 **I6**（客户端不执行、不持凭据），并为 I3 提供验签所需的设备公钥与实时角色；信任边界上的"桌面端可信但会失窃"（设备私钥不可导出、客户端固定 CA 指纹）靠本模块的吊销与配对设计兜底。

| 不变式 / 要求 | 本模块如何保证 |
| --- | --- |
| **I6 客户端不执行、不持凭据**（核心） | 服务端**从不生成**客户端私钥，只接收 CSR 并只取其中的公钥；设备私钥不可导出、不出设备；配对响应与任何接口都不含 GitHub / 模型凭据或服务端私钥；设备证书不含秘密 |
| **I3（间接）** | 审批签名的验证密钥是配对时单独登记的 `approval_pubkey`（P-47；设备端为必须用户在场、不可导出的审批签名密钥），与用于无提示后台连接的 mTLS 客户端密钥分开：mTLS 密钥被盗用也签不出审批；`verifying_key` 仅对 `active` 设备返回，吊销即失去签名资格；续期不改变审批密钥；角色实时读库，降级 / 吊销对下一个请求立即生效 |
| 根私钥不常驻 | `bootstrap` 只在内存持有根私钥，加密导出后 `zeroize`；`Pki::open` 启动自检：数据目录与 `secrets/` 中若存在与根证书公钥匹配的私钥 → 拒绝启动并提示"根私钥必须离线保存" |
| 中间 CA 私钥隔离 | 只经 `secrets` 以使用方 `pki` 读取，`api`、`engine`、`sandbox` 等读不到；进程内以 `Zeroizing` 持有，日志与 `Debug` 输出不含密钥 |
| 配对面最小化 | 单向 TLS 面只有 `POST /v1/pair` 一条路由；窗口外端口关闭；令牌高熵、短 TTL、单次、失败即作废、只存哈希、恒定时间比较；`401 pair_token_invalid` 统一，不给枚举 oracle；配对失败是安全信号，单独计数告警（api.md §1.3 第 4 点） |
| 防"抢先配对" | 令牌带外传递且单次；确认码 + 终端显示设备名与指纹让管理员核对；首台设备成为 approver 之后，其余设备默认 `operator`（P-23），抢注的设备也不能审批 |
| 防权限提升 | 签发的叶子参数完全由服务端决定（忽略 CSR 的 CA 位 / EKU / SAN）；`CA:FALSE`、`EKU: clientAuth` 固定；不能自助改角色 / 自吊销以外的操作 |
| 吊销失败即关闭 | 握手时序列号不在册 → 拒绝；吊销无缓存；会话恢复 / 0-RTT 禁用（`api`）；吊销先写 `revocations.journal` 再提交，备份恢复不会让被吊销设备复活（P-67） |
| 终端注入 | 设备名由未认证方提供，打印到管理员终端前剥离控制字符与 ANSI 转义；`gantry device recover list` 同样处理 |
| 日志卫生 | 不记录令牌（只记哈希前 8 位）、不记录私钥、不记录 CSR 全文；握手失败的原因只记本地，不回显给对端 |
| 不防的 | 服务端主机失陷（可读中间 CA 私钥、改库）；管理员主动配对恶意设备 |

## 9. 错误与恢复

| 失败 | 处理 |
| --- | --- |
| 令牌无效 / 已用 / 过期 / 作废 | `401 pair_token_invalid`（统一）；`failed_attempts` 计数；达到上限会话 `voided`，管理员重新 `gantry pair` |
| CSR 不合格 | `422 validation_failed`，`errors[].pointer = "/csr"`（或 `/device/name`、`/device/platform`），令牌不消耗 |
| 签发成功但响应丢失 | 令牌已消耗、设备已在册；用户重新 `gantry pair` 并在 UI 删除 / 吊销多余设备；不提供原令牌重试（防重放） |
| `redeem` 事务中途崩溃 | 单一写事务：要么设备 + 证书 + 会话 `redeemed` + `device.paired` 全部可见，要么都不可见；令牌随之仍有效 |
| 握手：链无效 / 过期 / 吊销 / 未在册 | 握手失败，本地日志记 `reason`（`unknown_ca` / `expired` / `not_yet_valid` / `revoked` / `unknown_serial` / `bad_eku` / `bad_algorithm`）；客户端连续多次 TLS 告警时提示"设备可能已被吊销或证书过期"（api.md §2.1） |
| 服务端叶子续签失败 | 保留旧证书继续服务，重试（指数退避）；剩余 ≤ 7 天升级为 `unhealthy` 告警；到期后 `api` 握手失败 |
| 中间 CA 私钥读不到（`secrets` 故障） | 不能签发（配对、续签失败）；已有设备的握手 / 鉴权不受影响（只需公钥与库）；健康 `degraded` |
| 数据库从旧备份恢复 | 之后签发的设备证书序列号不在册 → 握手被拒，需重新配对（失败即关闭，宁可误拒）；备份之后发生的吊销由 `replay_revocations` 从 `revocations.journal` 重放（P-67）；`log_epoch` 由 `store` 加一，客户端据此全量重同步 |
| `revocations.journal` 追加失败 / 磁盘满 / 同步复制到备份目标失败（P-80） | 吊销失败（不提交），`api` 返回 `503 unavailable`，审批保持 `granted`；告警 |
| `revocations.journal` 有坏行 | 重放在坏行处停止并报错，恢复命令失败（宁可不启动，也不漏掉吊销）；运维修复后重跑 |
| 时钟偏差 | 签发 `notBefore` 回拨 5 分钟；服务端时钟异常（早于最近一次签发时间）时拒绝签发并告警 |
| `recover` 非 root | 立即拒绝，不触碰数据库，退出码非 0 |
| `recover` 指向不存在 / 已吊销设备 | 不存在 → 报错；已吊销 → 成功且无副作用 |
| 启动发现根私钥在服务端 | 拒绝启动，日志说明处置（导出到离线介质并删除，或重新引导） |
| 根私钥导出文件 / 口令丢失 | 无法轮换中间 CA；到期后全部设备重新配对（deploy 的备份引导须强调） |

## 10. 配置

`pki` 的参数是**引导配置**（`deploy` 写入的配置文件，重启生效），不是 settings，不经 API 修改（config.md OQ-18 不收录）。下列默认值**全部是建议值**，除标注"强制"外。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `pki.data_dir` | `deploy` 定 | 根证书（公钥）、中间 / 服务端证书 PEM |
| `pki.server_names` | `deploy` 定 | 服务端叶子 SAN；变化触发重新签发 |
| `pki.pair_listen` / `pki.api_listen` / `pki.api_base_url` | `deploy` 定 | 端口由部署决定，api.md 不固定；`api_base_url` 写入配对响应 |
| `pki.root_validity` | 10 年 | 仅 `bootstrap` |
| `pki.intermediate_validity` | 2 年 | `bootstrap` 与 `rotate` |
| `pki.server_cert_validity` / `pki.server_cert_renew_before` | 90 天 / 30 天 | — |
| `pki.client_cert_validity` | 30 天 | 客户端在剩余 1/3 时经 `renew` 自动续期（P-64） |
| `pki.revocations_journal` | `<pki.data_dir>/revocations.journal` | 只追加；不进 `store` 备份、恢复时保留（P-67）；每次写入同步复制到备份目标 `backup.journal_target`（`deploy` 的引导配置，P-80） |
| `pki.not_before_skew` | 5 分钟 | — |
| `pki.superseded_grace` | 旧证书原 `notAfter` | 续期后旧证书在到期前仍可握手（P-64）；中间 CA 退役等强制场景另行吊销 |
| `pki.pair_token_ttl` | 10 分钟 | design.md 示例 |
| `pki.pair_max_failures` | 5 | 失败达到次数令牌作废 |
| `pki.max_csr_bytes` | 4096 | — |
| `pki.device_name_max_chars` | 64 | 与重命名一致（api.md §4.10） |
| `pki.last_seen_throttle` | 60 秒 | — |
| `pki.key_algorithm`（强制） | `ecdsa-p256` | 不可配置为其他值 |
| `pki.tls_min_version`（强制） | `1.3` | 不可配置 |

## 11. 可观测性

发出的事件见 [data-model §5](../data-model.md#5-事件)。

- **事件**（data-model §5.2）：`device.paired`（含 `approval_pubkey`、`approval_pubkey_fpr`）、`device.updated`（改名、改角色、续期换证 / 换 mTLS 密钥）、`device.revoked`（含日志重放 `via: "journal_replay"`）。均为全局事件（`project_id = null`）；`actor`：配对 `{system, pki}`，API 触发为 `{device, dev_…}`，recover 为 `{system, host_recover}`。事件 `data` 不含令牌、私钥、CSR。
- **日志**（结构化）：配对会话创建 / 兑换 / 作废（令牌只记哈希前 8 位）、握手拒绝（原因、序列号，不含证书全文）、证书签发（kind、serial、subject CN、notAfter）、续签结果、`recover` 的执行者 uid 与参数、启动自检结果。
- **指标**：`pki_handshake_total{result,reason}`、`pki_pairing_attempts_total{result}`（`ok` / `token_invalid` / `validation` / `voided`；失败突增 = 安全信号，应告警）、`pki_certs_issued_total{kind}`、`pki_cert_expiry_seconds{kind=root|intermediate|server}` 与设备证书最小剩余（仪表）、`pki_active_devices{role}`、`pki_pairing_window_open`（仪表）、`pki_revocations_total`、`pki_renewals_total{result}`、`pki_journal_replayed_total`。
- **告警建议**：服务端叶子剩余 < 14 天；中间 CA 剩余 < 90 天；配对失败 5 分钟内 > N；启动自检失败；`unknown_serial` 握手拒绝出现（可能是备份恢复或伪造）。
- **健康**：`pki` 不消费 settings，不在 `GET /v1/settings/status` 的 `modules` 列表里；健康（`ok` / `degraded` / `unhealthy`：续签失败、`secrets` 读不到、启动自检失败）由 `deploy` 的运维健康检查读取。

## 12. 测试与验收

### 12.1 测试替身与夹具

| 替身 / 夹具 | 用途 |
| --- | --- |
| 真实 `rcgen` / `rustls` / `rustls-webpki` | 本模块是被测方，**不用 `TestCA` 代替自己**；用真实 CA 材料跑真实 TLS 握手（`tokio-rustls` 的客户端 / 服务端对） |
| `TestCA`（M0，`contract` 泳道） | 给 `api`、`fe-desktop` 用；本模块测试里用它构造"外部 CA 签发""过期""尚未生效""错误 EKU"等**反例证书**，并校验 `TestCA` 签出的叶子能通过本模块的 verifier（二者参数一致）。若 `TestCA` 缺 `issue_expired` / `issue_not_yet_valid` / `issue_from_foreign_ca` / `issue_server_auth_only` 等变体，提替身 PR |
| `Store::open_in_memory` / `open_temp`（store.md §6.1）、`FakeClock`、`SeededEntropy` | 事务、时间、随机数可控；崩溃类测试在提交前注入失败 |
| `MemorySecrets`（secrets.md §6；M0 替身，P-81） | 实现 `SecretSink` / `SecretSource`，可注入"读不到""写失败"故障 |
| 固定 CSR 与 `approval_pubkey` 夹具 | P-256、P-384、RSA-2048、Ed25519 各一份；与 CSR 同钥的 `approval_pubkey`、不在曲线上的点、压缩点；带 `CA:TRUE` / 通配 SAN / `serverAuth` 请求扩展的恶意 CSR；签名被破坏的 CSR；超长 / 多块 PEM |

### 12.2 验收项

**引导与证书参数（M1）**

- [ ] `bootstrap` 产出：根（自签、`CA:TRUE pathlen:1`、`keyCertSign|cRLSign`、10 年）、中间（`pathlen:0`、2 年）、服务端叶子（`serverAuth`、SAN = `server_names`、90 天）；全部 P-256 / `ecdsa-with-SHA256`；用 `x509-parser` 逐项断言。
- [ ] `bootstrap::run` 只在 `out_dir`（内存盘）产出 `root.key.enc`、`root.crt`、`intermediate.key`、`intermediate.crt`、`server.key`、`server.crt`；`out_dir` 不是 tmpfs / ramfs → 拒绝；`root.key.enc` 需口令才能解开且公钥与 `root.crt` 一致。deploy 的清除步骤之后，服务端数据目录与 `secrets/` 中**没有**根私钥（按公钥匹配全盘扫描断言，deploy.md §7.3 步骤 8）；`Pki::open` 在目录里放入匹配根公钥的私钥时拒绝启动。
- [ ] 设备叶子：`CA:FALSE`、`EKU = clientAuth` 仅此一项、无 DNS / IP SAN、`CN = dev_…`、`notBefore` 早于签发时刻 5 分钟、`notAfter ≤ 中间 CA notAfter`、序列号为正且全库唯一；链 `[叶子, 中间]` 可验证到根。
- [ ] `ca_fingerprint` 等于根证书 DER 的 SHA-256；`pubkey_fpr` 等于设备 SPKI DER 的 SHA-256；两者格式为 `sha256:` + 64 位小写十六进制。

**配对（M1）**

- [ ] 成功路径：令牌 + 合法 CSR + `approval_pubkey` → `201`，含 api.md §1.3 列出的全部字段（`certificate_chain` 三张，末张的 DER 指纹等于 `ca_fingerprint`）；Device 的 `approval_pubkey_fpr` 正确；`store` 的 `devices` 投影与 `pki_device_certs`、`pki_device_state` 各一行；一条 `device.paired`；`confirmation_code` 为 6 位数字且与会话行一致。
- [ ] 第一台设备 `role = approver`，第二台起 `operator`；两个并发的首次配对恰好产生一台 `approver`。
- [ ] 令牌：单次有效（第二次兑换 `401`）；过期（`FakeClock` 推进 10 分钟）→ `401`；作废（5 次错误）→ `401`；无效 / 已用 / 过期 / 作废四种情形的 `401` 响应体与头**逐字节相同**。
- [ ] 数据库与日志中搜不到明文令牌（扫描库文件与捕获的 `tracing` 输出）；库里只有 SHA-256。
- [ ] 新会话作废旧会话：旧令牌立即 `401`。
- [ ] CSR 反例逐个拒绝（`422`、令牌不消耗）：RSA、Ed25519、P-384、CSR 签名被破坏、超长、多块 PEM、公钥已属于 `active` 设备。
- [ ] `approval_pubkey` 反例逐个拒绝（`422`，指针 `/approval_pubkey`，令牌不消耗）：桌面平台缺失、与 CSR 公钥相同、非 P-256、点不在曲线上、压缩点、非 base64url（P-47）。
- [ ] `verifying_key` 返回的是 `approval_pubkey`：用 mTLS 客户端私钥签的审批在 `policy` 验签失败，用审批签名私钥签的通过（与 `policy` 联调）。
- [ ] 恶意 CSR（请求 `CA:TRUE`、`keyCertSign`、通配 SAN、`serverAuth`）能被受理，但签出的证书不含这些扩展，subject 被重写为 `CN = <device.id>`。
- [ ] `name` 含控制字符 / 超 64 字符 / 空 → `422`；`platform` 不在枚举 → `422`；未知字段 → `422 unknown_field`。
- [ ] 终端打印：设备名中的 ANSI 转义序列与换行在输出里被剥离。
- [ ] 配对窗口：存在 `open` 会话时 `window()` 为 `Some`；兑换成功 / 作废 / 过期 / 取消后 1 秒内变为 `None`（与 `api` 联调：窗口外配对端口拒绝连接）。
- [ ] 令牌兑换成功后，用返回的证书对 mTLS 监听器做一次握手 + `GET /v1/me` 成功（与 `api` 联调，M1 退出标准的前置）。
- [ ] 配对串编码往返测试（待 OQ-K3 契约后补测试向量）；客户端用 `fp` 钉扎离线根 CA 指纹，错误指纹的服务端被拒（模拟中间人）。
- [ ] 引导期：库为空时 `bootstrap_period() = true`；首台设备配对后为 `false`，吊销全部设备后仍为 `false`（P-66）。

**握手校验（M1）**

- [ ] 有效设备证书 → 握手成功；`api` 取到的 `cert_serial` 与 Device 一致。
- [ ] 以下一律握手失败且不产生任何 HTTP 响应：无证书、过期、尚未生效、外部 CA 签发、未知中间 CA、仅 `serverAuth` 的证书、非 P-256 密钥、`CA:TRUE` 的叶子、`CN` 不是 `dev_…`。
- [ ] 吊销：`revoke` 提交后**下一次握手**即失败（无重启、无缓存刷新）；该设备所有证书序列号均失败。
- [ ] 序列号链有效但库中不存在 → 握手失败（失败即关闭）。
- [ ] 时钟偏差：客户端时钟快 4 分钟的设备仍能握手；快 6 分钟则 `not_yet_valid`。
- [ ] 吊销后无法恢复旧 TLS 会话、无法使用 0-RTT（用支持会话恢复的 rustls 客户端实测；断言在 `api` 的 `ServerConfig` 上，本模块提供联调用例）。
- [ ] 中间 CA 钉扎：用同一根签发的"未登记中间 CA"所签叶子被拒。

**设备管理与吊销（M1）**

- [ ] `set_role` 后同一设备的下一次 `by_cert_serial` 立刻返回新角色；自改角色 → `SelfRoleChange`；把 `ios` 设备设为 approver → `MobileCannotApprove`（P-76）；不存在 → `NotFound`；事件 `device.updated` 的 `changed.role = {from, to}`。
- [ ] `revoke`：设备与全部证书置吊销，发 `device.revoked`；重复吊销 `AlreadyRevoked` 且不重复发事件；可吊销自己；吊销后 `verifying_key` 返回 `None`。
- [ ] `revocations.journal`：每次吊销（API 与 `recover`）恰好追加一行并已 `fsync`；追加失败 → 吊销不提交；**恢复演练**：备份 → 吊销设备 A → 从备份恢复 → `replay_revocations` → A 握手失败、`device.revoked` 带 `via = journal_replay`；不重放就启动 → `Pki::open` 拒绝启动（P-67）。
- [ ] `set_role` / `revoke` 在调用方事务回滚时不留任何副作用、不广播 `RevocationNotifier`。
- [ ] `rename`：1–64 字符；越界 / 控制字符拒绝；发 `device.updated`。
- [ ] `touch_last_seen` 连续调用 100 次只写库 1 次（60 s 内）；时钟推进 61 s 后再写 1 次；不产生事件。
- [ ] 吊销后已建立的事件流在心跳间隔内断开（上限 60 s，M1 退出标准；与 `api` 联调，用 `FakeClock` 驱动心跳）。

**续期（M1：服务端叶子与设备叶子）**

- [ ] 服务端叶子：`FakeClock` 推进到剩余 ≤ 30 天，后台任务签发新叶子并热替换；已建立的连接不断；新握手拿到新叶子；客户端固定的根指纹不变；新叶子 `notAfter ≤ 中间 CA notAfter`。
- [ ] 续签失败（`secrets` 读不到中间 CA 私钥）：继续使用旧叶子，健康 `degraded`，重试；剩余 ≤ 7 天 `unhealthy`。
- [ ] 设备 `renew`（`POST /v1/devices/self/renew`，M1，P-64）：沿用旧密钥 → 新序列号、`pubkey_fpr` 不变；换新 mTLS 密钥 → `pubkey_fpr` 变化并发 `device.updated`；`approval_pubkey` 始终不变；响应链 `[叶子, 中间 CA, 根]` 的根指纹等于配对时的值；旧证书到期后握手失败；吊销作用于新旧全部序列号；已吊销设备续期 → 握手失败。
- [ ] 中间 CA 轮换（M3）后，已配对设备经 `renew` 拿到新中间 CA 签发的叶子，根指纹不变，无需重新配对。

**应急命令（M1）**

- [ ] 非 root 运行 `recover` → 拒绝、退出码非 0、数据库与控制套接字零触碰。
- [ ] root：`revoke` 吊销目标设备，事件 `actor = {system, host_recover}`、`data.via = "host_recover"`；`promote` 把一台 `operator` 提升为 `approver`；`list` 输出已剥离控制字符。
- [ ] 吊销唯一的 approver 后，新配对的设备是 `operator`（不是 approver，OQ-K11），用 `promote` 恢复审批能力；在线模式下被吊销设备的事件流在 ≤ 15 s（心跳）内关闭，无需重启。
- [ ] 两种模式都覆盖：**在线**（守护进程运行，经 `PkiControl` 执行，不打开数据库、不触发 `AlreadyRunning`）与**离线**（守护进程停止，`recover` 直接 `Store::open`，之后数据目录的文件属主仍是服务用户，守护进程可继续读写）；守护进程运行时尝试离线模式 → 得到 `AlreadyRunning` 并提示改用在线模式。
- [ ] 幂等：重复吊销 / 重复提升不重复发事件。

**轮换（M3）**

- [ ] `rotate_intermediate` 用加密的根私钥导出 + 口令：新旧中间 CA 并行期内两条链签发的设备证书都能握手；旧中间 CA 退役后其证书被拒；根指纹不变，客户端无需重新配对；口令错误 → 失败且不改变状态。
- [ ] 轮换过程中根私钥只在内存，用后 `zeroize`；服务端数据目录仍没有根私钥。
- [ ] 吊销时进程内即时通知（`RevocationNotifier`）：订阅者在事务提交后 100 ms 内收到；回滚不通知；接收方 `Lagged` 时退回全量检查。

**手机（M4）**

- [ ] `platform = ios` 配对成功并如实记录；不带 `approval_pubkey` 也成功（`approval_pubkey_fpr = null`）；作为第一台设备配对时角色仍为 `operator`；`set_role(approver)` 与 `recover --promote` 对它返回 `mobile_cannot_approve`（P-76）；配对串长度能放进二维码（版本 ≤ 10，纠错级别 M）；`api` 对该设备给出 `mobile` 档案（联调）。

**模糊测试（fuzzing）/ 属性测试（property-based testing）（建议）**

- [ ] 对 CSR 解析、令牌解析、配对请求体做 `cargo fuzz`：无 panic、无超时、无内存增长。
- [ ] 属性：任意并发的 `redeem` / `revoke` / `set_role` 序列下，恒有"设备 `revoked` 则其所有证书 `revoked`"、"至多一台设备在创建时被赋予 approver（当时库为空）"。

## 13. 开放问题

| # | 问题 | 本文的临时处理（不构成决定） | 影响 |
| --- | --- | --- | --- |
| OQ-K13 | ~~`revocations.journal` 在"事务回滚但日志行已写"时的语义~~；主机迁移时日志文件如何随数据目录搬迁 | 语义已决：见 decisions.md P-80（只追加、同步复制到备份目标、回滚不删条目、按时间戳重放）；搬迁步骤归 `deploy` 运行手册 | `deploy` |
| OQ-K1 | 安装时引导工具的命令形态与由谁调用（`deploy` 的安装脚本调用 `pki::bootstrap::run` 的包装命令）；根私钥导出介质与口令方案；是否支持 TPM / 硬件密钥存放中间 CA（design.md 提过"可选 TPM"，architecture 未采纳） | 库函数 `bootstrap::run`；加密 PKCS#8 导出到指定路径；不支持 TPM | `deploy` |
| OQ-K2 | （①**已决：见裁决 C18 / P-64**——固定离线根 CA 指纹，链 `[叶子, 中间 CA, 根]`）仍未决：② `pubkey_fpr` / `approval_pubkey_fpr` 对 SPKI DER 还是裸公钥点；③ `cert_serial` 的位数与编码（api.md OQ-10） | `pubkey_fpr` = SPKI DER 的 SHA-256；序列号 128 位随机、小写十六进制 | api.md、`desktop-shell`、`mobile` |
| OQ-K3 | 配对串的编码、配对监听器的端口与是否按需开启、窗口时长（api.md OQ-12） | §7.4.2 的 URL 形式；仅在会话窗口内开启；10 分钟 | api.md §1.3、`deploy`、`desktop-shell`、`mobile` |
| OQ-K4 | `confirmation_code` 核对失败的处置：设备在核对前是否已 `active`、有没有 abort / confirm 路由；码是随机还是由 TLS exporter + CSR 公钥派生（SAS） | 设备在 `201` 时即 `active`；无 abort 路由；码为 CSPRNG 随机 6 位；不一致就吊销 | api.md、安全强度 |
| OQ-K5 | 吊销（含自吊销）最后一台 approver 是否需要额外保护；`device.revoked` 是否带 `remaining_approvers` | 不加限制，事件带计数供 UI 提示；恢复靠 `recover` | `desktop-ui`、data-model 事件 Schema |
| OQ-K6 | **已决：见裁决 C18 / P-64**（`POST /v1/devices/self/renew`，M1 交付；客户端在剩余 1/3 有效期时自动续期）。原问题：设备证书短期有效但没有续期路由 | 已落实到 §6.2、§7.7、§12.2 | — |
| OQ-K7 | 主机命令与守护进程的通信（deploy.md OPS-07 把它交给本规格）：`store` 持有数据目录排他锁，守护进程运行时 CLI 不能直连数据库；`recover` 写入的事件是复用 `device.revoked` / `device.updated`（actor = system）还是新增 `device.recovered`；`gantry pair` / `recover` 的完整参数形态 | 在线走 `/run/gantry/control.sock`（`PkiControl`），离线（守护进程未运行）`recover` 直接 `Store::open`；复用现有事件，`data.via = "host_recover"` | data-model §5.2、`deploy`（OPS-07）、`gantryd` |
| OQ-K8 | **已决：见裁决 C1 / P-47**（双密钥：配对同时提交 CSR 与 `approval_pubkey`，审批一律用审批签名密钥，服务端用登记的 `approval_pubkey` 验签）。原问题：审批签名密钥是否与 mTLS 密钥分开 | 已落实到 §6.2、§6.3、§7.4.3 | — |
| OQ-K9 | 同一公钥重复配对的处置（本文：拒绝）；已吊销设备的公钥是否允许再次配对 | 仅拒绝与 `active` 设备重复 | — |
| OQ-K10 | 服务端叶子私钥由 `secrets` 托管（本文：是，使用方 `Consumer::Pki`，secrets.md §6）；`secrets` 的 `stage` / `commit` 是否也用于叶子私钥续签的原子切换（先 `stage` 新私钥，证书入库后 `commit`） | 续签时 `stage` → 写 `pki_server_cert` → `commit` | `secrets` 规格 |
| OQ-K11 | "第一台设备为 approver"的口径（deploy.md OPS-16）：本文按"设备表里从未有过任何设备"判定（含已吊销者）。后果：用 `gantry device recover --revoke` 吊销唯一的 approver 后，重新配对的设备是 `operator`，必须再 `--promote`。是否应改为"当前没有 `active` 的 approver"则新设备成为 approver（更省事，但抬高了抢先配对的风险） | 按"从未有过设备"；恢复靠 `--promote` | `deploy` 运行手册、P-23 的解读 |
| OQ-K12 | 设备续期换证 / 换钥时 `device.updated` 要带哪些字段才能让 `store` 的 `devices` 投影更新 `cert_serial` / `pubkey_fpr`；`pubkey_spki` 是否也应进事件（目前只在 `pki_device_state`，备份恢复后可由证书重建）；`approval_pubkey` 进事件（Device 派生列） | 事件带 `changed: {cert_serial, pubkey_fpr}`；`pubkey_spki` 不进事件 | `schemas/events/device.*`、`store` |
