# 证书与配对（`pki`）

> 状态：设计 v0.1 · 里程碑：M1（主体）→ M3（密钥轮换、进程内即时断流）→ M4（iOS 配对）· 泳道：`sec` · crate / 目录：`crates/gantry-pki`

`pki` 把"这台设备是谁、还被不被信任"变成密码学事实：私有 CA（离线根 + 在线中间 CA）签发服务端与设备证书；一次性令牌 + CA 指纹固定完成首次配对；握手时按吊销表拒绝；设备丢了有主机应急命令兜底。它不处理任何业务 HTTP，也不替客户端保管私钥。

## 1. 职责

1. **CA 层次**：离线根 CA（安装时生成、导出到离线介质后从服务端删除）+ 在线中间 CA（私钥只经 `secrets` 由本模块读取）；提供安装时的引导工具（`bootstrap`）供 `deploy` 调用（architecture §7.1 步骤 1）。
2. **服务端证书**：签发服务端叶子证书，到期前自动续签并热加载，不断开现有连接。
3. **配对**：`gantry pair` 的令牌与配对窗口；配对端点 `POST /v1/pair` 的全部业务逻辑（令牌核验、CSR 校验、签发、创建 Device、确认码）；第一台设备为 `approver`、之后默认 `operator`（P-23）。HTTP 监听器由 `api` 持有，业务由本模块实现（[api.md §1.3](../interfaces/api.md)）。
4. **设备注册表**：Device 实体（名称、平台、角色、状态、`last_seen`）、证书表、吊销表；事件 `device.paired` / `device.updated` / `device.revoked`。
5. **握手校验**：提供 rustls 的 `ClientCertVerifier`——证书链、有效期、增强密钥用途、算法、`cert_serial` 是否在册且未吊销（无 CRL / OCSP，握手时直接查库）。
6. **角色与吊销**：`set_role`、`revoke`、重命名；对 `api` 暴露"在调用方事务内执行"的接口，使"消费审批 + 写业务"同事务（api.md §6.6）。
7. **吊销生效**：新握手立即被拒；已建立的事件流在 M1 靠 `api` 心跳内的设备状态检查在 ≤ 60 s 内断开，M3 起提供进程内即时通知（P-30）。
8. **证书轮换与续期**：服务端叶子证书自动续签（M1）；中间 CA 轮换（M3，需要离线根）；设备证书续期的内部接口（路由未定，OQ-K6）。
9. **主机应急命令** `gantry device recover`（P-39）：需主机 root，吊销设备或把一台设备提升为 approver，写审计事件。
10. **向 `policy` 提供验签公钥**：经 `api` 传递设备的 P-256 公钥（审批签名的验证密钥，[api.md §6.3](../interfaces/api.md)）。

### 1.1 各里程碑交付

| 里程碑 | 交付 |
| --- | --- |
| M0 | 不交付实现。`TestCA`（测试用证书与设备）由 `contract` 泳道交付，供 `api`、`fe-desktop` 集成测试（milestones 替身表）；`schemas/events/device.*.json`。本模块在 M0 只落 crate 骨架与 trait |
| M1 | 全部主体：`bootstrap`（离线根 + 在线中间 CA + 服务端证书）、握手校验器、配对（令牌、窗口、CSR 校验、签发、确认码、首台 approver / 之后 operator）、设备注册表与 `device.*` 事件、`set_role` / `revoke` / 重命名、`last_seen` 节流、服务端证书自动续签与热加载、`gantry pair`、`gantry device recover`、启动自检（根私钥不得在服务端） |
| M2 | 无新增功能；回归。若 OQ-K6 决定把设备证书续期路由放进 M2，则交付 `renew` 的路由对接 |
| M3 | 中间 CA 轮换（需要离线根解锁）；`RevocationNotifier` 被 `api` 用于进程内即时断开事件流（M1 的心跳检查保留为兜底） |
| M4 | `platform = ios` 的配对（配对串需适合二维码）；与 `fe-mobile` 联调；手机档案的 `approver` 限制由 `api` / `policy` 保证，本模块只保证 `platform` 如实记录 |

## 2. 不负责

| 事 | 归谁 |
| --- | --- |
| HTTP 监听、路由、限流、`Idempotency-Key`、角色 / 档案的路由级判定 | `api`；配对监听器的端口、开关由 `api` 按本模块给出的配对窗口执行 |
| 审批的创建、challenge、验签、状态机、`subject_digest` 一致性 | `policy`。本模块只提供设备公钥与角色 |
| `device_role_change` / `device_revoke` 审批的签名校验与消费 | `policy`（`consume_internal`）；`api` 在同一事务里调用本模块的 `set_role` / `revoke` |
| 客户端私钥的生成与保管、CSR 生成、系统级确认 | `desktop-shell`、`mobile`（I6：服务端从不生成客户端私钥） |
| 密钥的加密存储与按使用方隔离读取（中间 CA 私钥、服务端叶子私钥） | `secrets`（本模块是其 `pki` 使用方，只能读自己的密钥） |
| 安装脚本、systemd 单元、根 CA 离线备份的操作引导与介质选择 | `deploy`（本模块提供可调用的 `bootstrap` 库函数） |
| Webhook 入口的 TLS | `forge` / `deploy`（独立监听，不属于本 PKI 信任链的客户端侧，api.md §1.1） |
| 设备管理界面、续期提示、"证书将过期"的展示 | `desktop-shell` / `desktop-ui` |
| 事件流的实际断开动作 | `api` |
| 设备的 settings（证书有效期等是否做成运行期设置） | 不做（config.md OQ-18）：全部为引导配置 |

## 3. 依赖

| 依赖 | 用到什么 | 契约 |
| --- | --- | --- |
| `store` | 同一写事务内追加 `device.*` 事件并更新本模块的投影表；一致读；本模块表的迁移 | [data-model §5](../data-model.md)。**`store` 规格书尚不存在**，需求同 [policy.md §3.1](policy.md) 的占位类型（`WriteTx`、`ReadSnapshot`） |
| `secrets` | 以使用方 `pki` 的身份读写：中间 CA 私钥、服务端叶子私钥；加密存储 | architecture §4、§6；decisions P-38。`secrets` 规格书尚不存在 |
| `core` | `Device`、`DeviceId`、`Role`、`Platform`、`DeviceStatus`、事件枚举、`Clock` | [data-model §3 Device](../data-model.md) |
| 外部 crate（建议） | `rcgen`（签发）、`rustls` + `rustls-webpki`（握手校验）、`x509-parser`（解析与断言）、`p256`、`sha2`、`zeroize`、`subtle`、`qrcode`（终端二维码）、`arc-swap` | architecture §10 |
| 非依赖 | **不依赖** `api`、`policy`（反向被调用）；不依赖 `egress`、`forge` | architecture §4 |

## 4. 输入

| 输入 | 来源 | 格式 / 契约 | 信任级别 |
| --- | --- | --- | --- |
| `PairRequest { token, csr_pem, device{name, platform} }` | `api` 的配对监听器（单向 TLS，未认证） | [api.md §1.3](../interfaces/api.md) | **完全不可信**：令牌、CSR、名称都要严格校验（§7.4） |
| 客户端证书链（握手） | `api` 的 mTLS 监听器 | X.509，TLS 1.3 | 不可信，验证后才映射为 Device |
| `gantry pair` / `gantry device recover` 命令 | 主机管理员（终端） | §7.8；recover 需 root | 主机 root 可信；命令参数仍严格校验 |
| 调用方事务 `tx` + 操作者 `ActorRef` | `api`（`set_role` / `revoke`，审批已在同一 tx 被 `policy` 消费） | §6 | 可信（已通过 `api` 的角色与审批校验）；本模块对"不能改自己"等不变量再做一次防御性检查 |
| 引导配置 | `deploy` 写入的配置文件 | §10 | 可信 |
| 离线根 CA 私钥（仅 M3 轮换、仅一次性） | 管理员从离线介质提供（口令加密） | PKCS#8（加密） | 可信但高敏感：只在内存中使用，用后 `zeroize` |

## 5. 输出

| 输出 | 去向 | 格式 / 契约 |
| --- | --- | --- |
| `PairResponse`（`201`：`device`、`certificate_chain = [叶子, 中间 CA]`、`api_base_url`、`ca_fingerprint`、`confirmation_code`） | `api` → 未配对设备 | api.md §1.3 |
| `Device` 表示（`id`、`name`、`platform`、`cert_serial`、`pubkey_fpr`、`role`、`status`、`last_seen`） | `api`（`/v1/me`、`/v1/devices`） | api.md §4.2、§4.10；data-model Device |
| 事件 `device.paired` / `device.updated` / `device.revoked` | `store` | data-model §5.2；`data` Schema 在 `schemas/events/`（M0）；`device.*` 为全局事件，`project_id = null` |
| rustls `ClientCertVerifier`、`ResolvesServerCert`、根证书 PEM、CA 指纹 | `api`（构造 `ServerConfig`） | §6.4 |
| 设备验签公钥 | `api` → `policy` | `p256::ecdsa::VerifyingKey` |
| `RevocationNotifier`（M3） | `api` | §6.5 |
| 终端输出（配对串、二维码、确认码、设备名与指纹） | 管理员 | §7.4、§7.8 |
| 指标、日志 | 运维 | §11 |

## 6. 对外接口

Rust 签名草案。`Pki` 是对外的总入口，`gantryd` 装配时取各子 trait 分发给 `api` 等调用方。

### 6.1 总入口

```rust
pub struct Pki { /* 内部句柄，见 §7 */ }

impl Pki {
    /// 打开已引导的 PKI：载入根证书（公钥）、当前 / 上一个中间 CA、服务端证书；
    /// 启动自检（§8）：服务端上发现与根证书公钥匹配的私钥 → Err(RootKeyPresent)，拒绝启动。
    pub async fn open(cfg: PkiConfig, store: Arc<dyn Store>, secrets: Arc<dyn SecretStore>,
                      clock: Arc<dyn Clock>, rng: Arc<dyn Rng>) -> Result<Pki, PkiError>;

    pub fn directory(&self) -> Arc<dyn DeviceDirectory>;
    pub fn admin(&self) -> Arc<dyn DeviceAdmin>;
    pub fn pairing(&self) -> Arc<dyn PairingService>;
    pub fn tls(&self) -> TlsMaterial;
    pub fn revocations(&self) -> RevocationNotifier;          // M3 起被 api 使用
    /// 后台任务：服务端证书续签、配对窗口观察（轮询 pairing_sessions）、过期令牌清理
    pub fn spawn_background(&self, shutdown: CancellationToken) -> JoinSet<()>;
}
```

### 6.2 设备目录与管理

```rust
#[async_trait]
pub trait DeviceDirectory: Send + Sync {
    /// 鉴权热路径（api.md §2.1 步骤 2）：按 cert_serial 直接读库，不缓存。
    /// 序列号不在册（哪怕链有效，如从旧备份恢复）→ None。
    async fn by_cert_serial(&self, serial: &CertSerial) -> Result<Option<Device>, PkiError>;
    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, PkiError>;
    async fn list(&self, page: PageReq) -> Result<Page<Device>, PkiError>;       // 含已吊销，keyset 分页
    /// 审批验签用；设备已吊销或密钥不是 P-256 → None
    async fn verifying_key(&self, id: &DeviceId) -> Result<Option<VerifyingKey>, PkiError>;
    /// 节流写入（≤ 每设备每 60 s 一次），不产生事件
    async fn touch_last_seen(&self, id: &DeviceId);
}

#[async_trait]
pub trait DeviceAdmin: Send + Sync {
    /// 1–64 字符，无控制字符；自成事务；事件 device.updated（changed: {name}）。operator 起（api 判）
    async fn rename(&self, id: &DeviceId, name: &str, actor: &ActorRef) -> Result<Device, DeviceError>;
    /// 在调用方事务内执行。actor.id == id → Err(SelfRoleChange)（api 映射 409 conflict / self_role_change）。
    /// 事件 device.updated（changed: {role: {from, to}}）。新角色对该设备的下一个请求立即生效。
    async fn set_role(&self, tx: &mut dyn WriteTx, id: &DeviceId, role: Role, actor: &ActorRef)
        -> Result<Device, DeviceError>;
    /// 在调用方事务内执行；吊销该设备全部证书序列号。已吊销 → Ok(AlreadyRevoked)，不重复发事件。
    /// 允许吊销自己。事件 device.revoked。不可逆：恢复必须重新配对（新 Device 记录）。
    async fn revoke(&self, tx: &mut dyn WriteTx, id: &DeviceId, actor: &ActorRef)
        -> Result<RevokeOutcome, DeviceError>;
}
pub enum RevokeOutcome { Revoked(Device), AlreadyRevoked(Device) }
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
    pub ca_fingerprint: String,                 // "sha256:" + 64 位十六进制（根证书 DER 的 SHA-256，OQ-K2）
    pub expires_at: Timestamp,
}
pub struct PairRequest { pub token: String, pub csr_pem: String, pub device: NewDevice }
pub struct NewDevice { pub name: String, pub platform: Platform }       // windows | macos | ios
pub struct PairResponse {
    pub device: Device,
    pub certificate_chain: [String; 2],         // [叶子 PEM, 中间 CA PEM]
    pub api_base_url: Url, pub ca_fingerprint: String, pub confirmation_code: String,   // 6 位数字
}
pub enum PairError {
    TokenInvalid,                               // 无效 / 已用 / 过期 / 作废：对外统一 401 pair_token_invalid，不给枚举 oracle
    Validation(Vec<FieldError>),                // 422：csr / device.name / device.platform / 未知字段
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

`api` 必须用这两件拼 `ServerConfig`，并满足契约 [interfaces/api.md §1.2](../interfaces/api.md) 与模块规格 [modules/api.md §7.2](api.md) 的加固要求（仅 TLS 1.3、禁会话票据、禁 0-RTT）。

### 6.5 吊销通知（M3）

```rust
pub struct RevocationNotifier(tokio::sync::broadcast::Sender<DeviceId>);
impl RevocationNotifier { pub fn subscribe(&self) -> broadcast::Receiver<DeviceId>; }
// revoke 的事务提交成功之后才发送（回滚不发）；接收方落后（Lagged）时必须退回"全量检查所有流的设备状态"。
```

### 6.6 引导、轮换、应急命令

```rust
pub mod bootstrap {
    /// 安装时由 deploy 调用一次。根私钥只存在于内存，加密导出到 params.root_export_path，
    /// 绝不写入服务端数据目录或 secrets/；返回后内存 zeroize。
    pub fn run(params: BootstrapParams, secrets: &dyn SecretStore, clock: &dyn Clock)
        -> Result<BootstrapReport, PkiError>;
    pub struct BootstrapParams {
        pub server_names: Vec<String>,            // 服务端叶子证书的 SAN（DNS 名 / IP）
        pub root_export_path: PathBuf, pub root_export_passphrase: SecretString,
        pub install_id: String, pub data_dir: PathBuf,
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
    pub async fn run(cmd: RecoverCmd, env: &HostEnv, store: &dyn Store, clock: &dyn Clock)
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
| 公钥算法（强制） | ECDSA P-256（secp256r1），所有 CA 与叶子 | design.md；api.md 的 ES256 审批签名也要求设备密钥是 P-256（§6.3）。CSR 带其他算法 → 拒绝 |
| 签名算法 | `ecdsa-with-SHA256` | — |
| TLS | 仅 TLS 1.3（强制，api.md §1.2）；握手签名方案仅 `ECDSA_NISTP256_SHA256` | 由 `api` 的 `ServerConfig` 固定（modules/api.md §7.2） |
| 根 CA | 自签；有效期 10 年 [建议]；`basicConstraints: CA:TRUE, pathlen:1`（critical）；`keyUsage: keyCertSign, cRLSign`（critical）；`CN = "Gantry Root CA <install_id>"` | 私钥导出到离线介质（口令加密 PKCS#8），服务端删除；**服务端只保留根证书（公钥）** |
| 中间 CA | 根签发；有效期 2 年 [建议]；`CA:TRUE, pathlen:0`（critical）；`keyUsage: keyCertSign, cRLSign`；`CN = "Gantry Intermediate CA <序号>"` | 私钥经 `secrets` 以 `pki` 身份读取，仅签发进程可见；M3 轮换 |
| 服务端叶子 | 中间 CA 签发；有效期 90 天 [建议]，剩余 ≤ 30 天自动续签；`CA:FALSE`；`keyUsage: digitalSignature`；`EKU: serverAuth`；SAN = `PkiConfig.server_names`（DNS / IP，来自 `deploy`），`CN` = 第一个名字 | 私钥由 `secrets` 保管；`notAfter` 不得晚于中间 CA 的 `notAfter` |
| 设备叶子 | 中间 CA 签发；有效期 30 天 [建议，design.md 示例；见 OQ-K6]；`CA:FALSE`；`keyUsage: digitalSignature`；`EKU: clientAuth`；**无 DNS / IP SAN**；`CN = "<device.id>"`（服务端重写，api.md §1.3）；可加 URI SAN `urn:gantry:device:<device.id>` [建议] | **只取 CSR 里的公钥**，CSR 请求的任何扩展、subject、SAN 一律忽略 |
| 生效时间 | `notBefore = 签发时刻 − 5 分钟` [建议] | 两端时钟偏差余量（design.md） |
| 序列号 | 每张证书 128 位 CSPRNG 随机数，最高位清零保证为正，表示为小写十六进制、去前导零 [建议]；全库唯一（冲突则重新生成） | 与 api.md 示例（20 位十六进制）形态兼容；格式由 api.md OQ-10 最终确认（OQ-K2） |
| 公钥指纹 `pubkey_fpr` | `"sha256:"` + 小写十六进制（设备公钥 SubjectPublicKeyInfo DER 的 SHA-256） | api.md 示例形态；SPKI 还是裸点由 OQ-K2 确认 |
| CA 指纹（配对串里的 `ca_fingerprint`） | `"sha256:"` + 小写十六进制（**根**证书 DER 的 SHA-256） | 客户端固定根而非中间 CA，使中间 CA 轮换对客户端无感；api.md `certificate_chain` 不含根，客户端在配对握手里从服务端发送的链取得根并核对指纹——此取舍契约未定（OQ-K2） |
| 随机数 | `OsRng`；令牌、序列号、确认码都不得用可预测来源 | — |

服务端在 TLS 握手中发送的链为 `[服务端叶子, 中间 CA, 根]`（含根，供配对客户端核对 CA 指纹）；设备证书的 `certificate_chain` 响应字段仍按契约为 `[叶子, 中间 CA]`。

### 7.2 持久化

事件是事实源，下列表是投影 / 辅助表（本模块拥有，与事件追加同事务更新）；私钥不进数据库。

| 表 | 列（要点） | 说明 |
| --- | --- | --- |
| `pki_ca` | `kind(root|intermediate)`、`serial`、`fingerprint`、`not_before`、`not_after`、`status(current|previous|retired)`、`cert_pem` | 根只存证书；中间 CA 私钥在 `secrets` |
| `pki_server_cert` | `serial`、`not_before`、`not_after`、`chain_pem`、`status` | 私钥在 `secrets` |
| `devices` | `id`、`name`、`platform`、`role`、`status(active|revoked)`、`cert_serial`（当前）、`pubkey_fpr`、`pubkey_spki`（DER，验签用）、`created_at`、`updated_at`、`last_seen` | Device 投影；`last_seen` 不进事件日志（避免洪泛；备份恢复后可能回退，无业务后果） |
| `device_certs` | `serial`（主键）、`device_id`、`not_before`、`not_after`、`status(active|superseded|revoked)` | 吊销表；握手时按 `serial` 查询 |
| `pairing_sessions` | `id`、`token_sha256`、`created_at`、`expires_at`、`failed_attempts`、`state(open|redeemed|voided|expired)`、`redeemed_device_id?`、`confirmation_code?`、`created_by_uid` | CLI 与守护进程之间的会合点（§7.8）；明文 token 永不落库 |

### 7.3 握手校验器 `ClientCertVerifier`

按顺序，任一步失败即握手失败（对客户端表现为 TLS 告警，**不产生 HTTP 响应**；失败原因只记本地日志）：

1. **链**：用 `rustls-webpki` 以根证书为信任锚，验证 `[叶子, 中间 CA]`，时间用注入的 `Clock`；增强密钥用途须含 `clientAuth`；叶子 `CA:FALSE`；公钥为 P-256，签名算法在白名单内。
2. **中间 CA 钉扎**：出示的中间 CA 的指纹必须属于 `pki_ca` 中 `current` 或 `previous` 的集合（使泄露的中间 CA 可被单独退役，M3）。
3. **身份格式**：叶子 `CN` 形如 `dev_<ULID>`。
4. **吊销 / 在册检查**：用叶子 `serial` 查 `device_certs`（读库，**不加带 TTL 的缓存**）：行必须存在，状态为 `active`，或为 `superseded` 且仍在宽限期内（建议 10 分钟）；所属设备 `status = active`。**序列号不在册 → 拒绝**（失败即关闭：如从旧备份恢复后，链上有效但库里没有的证书不放行）。
5. 通过后不在 verifier 里放任何业务状态；`api` 从 `peer_certificates()` 取叶子序列号，再用 `DeviceDirectory::by_cert_serial` 实时映射到 Device（每个请求一次，api.md §2.1）。

要求：`offer_client_auth = true`、`client_auth_mandatory = true`（无证书直接失败）；`root_hint_subjects` 给出中间 CA 的 subject。

### 7.4 配对

#### 7.4.1 会话与令牌

- `gantry pair`（主机，§7.8）→ `PairingService::open_session`：创建 `pairing_sessions` 行，`state = open`；新会话**作废**此前仍 `open` 的会话（同一时刻至多一个有效令牌）。
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
3. **设备字段**：`name` 1–64 字符、不含控制字符与换行；`platform ∈ {windows, macos, ios}`（`ios` 在 M4 前也接受并如实记录）→ 否则 `Validation`。
4. **CSR**：见 §7.4.4。→ 否则 `Validation`（指针 `/csr`）。令牌在 CSR 校验失败时**不**被消耗（令牌只在签发成功后失效），但失败计入第 2 步之外的限流（`api` 按源 IP，5/min）。
5. **单一写事务**：
   1. 角色：`NOT EXISTS (SELECT 1 FROM devices)` → `approver`，否则 `operator`（P-23）。同一事务内判定并插入，SQLite 写事务串行化，保证"第一台"唯一（并发两次首配对只会有一台 `approver`）。
   2. 生成 `dev_<ULID>`、序列号；用中间 CA 私钥签发设备叶子（§7.1）。
   3. 插入 `devices`（`status = active`）与 `device_certs`；会话置 `redeemed`，写入 `redeemed_device_id` 与 `confirmation_code`。
   4. 追加 `device.paired`：`data = {device_id, name, platform, role, cert_serial, pubkey_fpr}`；`actor = {kind: "system", id: "pki"}`；`project_id = null`。
6. **响应**：`201`，`certificate_chain = [叶子, 中间 CA]`，`ca_fingerprint`，`api_base_url`，`confirmation_code`。令牌消耗后不可重放；响应丢失 → 只能重新 `gantry pair`（本端点不支持 `Idempotency-Key`，api.md §1.3）。
7. **主机侧**：`wait_outcome` 看到 `redeemed` 后，终端打印"设备名（已去除控制字符与 ANSI 转义）· 平台 · `pubkey_fpr` · 确认码"，管理员与设备屏幕上的确认码核对。

#### 7.4.4 CSR 校验（强制）

| 检查 | 规则 |
| --- | --- |
| 编码 | 恰好一个 PEM `CERTIFICATE REQUEST` 块；大小 ≤ 4 KiB；无尾随垃圾 |
| 自签名 | 验证 CSR 签名（持有私钥证明）；失败即拒绝 |
| 公钥 | 必须是 ECDSA P-256（OID `id-ecPublicKey` + `prime256v1`）；RSA / Ed25519 / P-384 一律拒绝；公钥点必须在曲线上（解析时校验），非未压缩点格式拒绝 |
| 签名算法 | `ecdsa-with-SHA256` |
| 请求的扩展 / subject / 属性 | **全部忽略**（尤其 `basicConstraints`、`keyUsage`、`extKeyUsage`、`subjectAltName`、`challengePassword`）；签发的叶子参数完全由服务端按 §7.1 构造。实现上用公钥 + 固定参数新建证书，**不得**从 CSR 复制 `CertificateParams` |
| 重复公钥 | 同一公钥已属于 `active` 设备 → 拒绝（避免一个密钥两个 Device，破坏"一个设备一把验签密钥"） |

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

- `revoke` 事务内：`devices.status = revoked`；该设备全部 `device_certs.status = revoked`；追加 `device.revoked`（`data`：`device_id`、`cert_serials[]`、`remaining_approvers`、可选 `via`）。提交成功后才广播 `RevocationNotifier`。
- **新握手**：下一次握手即被拒（§7.3 第 4 步，直接读库，没有缓存可过期）。**会话恢复与 0-RTT 会绕过握手时的证书检查**，所以 `api` 的 `ServerConfig` 必须关闭会话票据与会话缓存、`max_early_data_size = 0`（见 [modules/api.md §7.2](api.md)）；本模块的验收含一个"吊销后无法恢复会话"的联调测试。
- **已建立的连接**：HTTP 请求在每个请求的步骤 2 复查 `Device.status`；事件流 M1 由 `api` 在每次心跳（15 s）读 `directory.get`，≤ 60 s 内断开（P-30）；M3 起 `api` 订阅 `RevocationNotifier` 即时断开，心跳检查保留为兜底。

### 7.7 证书续期与轮换

| 对象 | 机制 | 里程碑 |
| --- | --- | --- |
| 服务端叶子 | 后台任务每 6 小时检查；剩余 ≤ 30 天或 SAN 配置变化时，用中间 CA 签发新叶子，写 `secrets` 与 `pki_server_cert`，`ArcSwap` 替换 `ResolvesServerCert`；旧连接不受影响。客户端固定根，续签对其无感 | M1（不做则系统在 90 天后不可用） |
| 设备叶子（续期） | 内部接口 `renew(device_id, presented_serial, csr_pem)`：设备须 `active` 且出示的证书有效；可沿用旧密钥或换新密钥（换新则更新 `pubkey_spki` / `pubkey_fpr`、发 `device.updated`，旧密钥签的未用 challenge 自然失效）；旧证书置 `superseded`（宽限 10 分钟后不再通过握手）；吊销作用于该设备全部序列号。**HTTP 路由契约未定**（api.md OQ-12），M1 验收不含续期 | 接口随 M1 落地；路由待 OQ-K6 |
| 中间 CA | 需要离线根：管理员提供加密的根私钥导出文件与口令，`rotate_intermediate` 在内存中解开、签发新中间 CA、立即 `zeroize`；旧中间 CA 状态 `previous`，在其已签发证书的最长剩余寿命后（建议设备证书有效期 + 1 天）转 `retired`；紧急情况（中间 CA 泄露）可立即 `retired`，其签发的全部证书失效，设备需重新配对 | M3 |
| 根 CA | 不轮换（有效期 10 年）；到期前重新引导并要求全部设备重新配对 | — |

### 7.8 主机命令与守护进程的会合

`gantry pair` 与 `gantry device recover` 是**主机上的独立进程**，不开放任何网络或 socket 入口。本规格的会合方式（待 OQ-K7 确认）：命令进程用同一 `store` 库直接打开数据库（`BEGIN IMMEDIATE` 串行化写入），守护进程靠两点感知：

- 配对：守护进程每秒轮询 `pairing_sessions`（`PairingService::window()` 的实现）；CLI 轮询会话行得到结果并打印。
- recover：守护进程不需要被通知——设备状态与角色每次请求都实时读库，事件流的心跳检查在 ≤ 15 s 内发现吊销；`store` 的订阅需能感知其他进程追加的事件（`store` 规格须保证，如轮询 `data_version`）。

`gantry device recover`：

| 项 | 规则 |
| --- | --- |
| 权限 | 要求 `geteuid() == 0`，否则拒绝（退出码非 0）。通过后**降权到服务用户**再打开数据库，避免 root 属主的 WAL / SHM 文件让守护进程无法写入 |
| 子命令（形态建议，骨架只定了命令名） | `list`（列出设备 id / 名称 / 平台 / 角色 / 状态 / `last_seen` / 指纹）；`revoke <device_id>`；`promote <device_id>`（把一台 `active` 设备提升为 `approver`） |
| 确认 | 交互终端上要求输入目标设备 id 才执行；`--yes` 跳过（脚本用） |
| 事件 | `revoke` → `device.revoked`；`promote` → `device.updated`（`changed: {role: {from, to}}`）；`actor = {kind: "system", id: "host_recover"}`，`data` 带 `via: "host_recover"`、`host_uid`、`sudo_user`（来自 `SUDO_USER`，仅供审计参考，不作为授权依据）。事件类型是否需要专门的 `device.recovered`：OQ-K7 |
| 幂等 | 已吊销再吊销 → 成功且不重复发事件；已是 approver 再提升 → 成功且不发事件 |
| 不做 | 不签发证书、不创建设备、不碰根 CA；恢复访问 = `promote` 一台已配对设备，或 `gantry pair` 重新配对（此时库中已有设备记录，新设备默认是 `operator`，再 `promote`） |

### 7.9 并发模型

- 所有写入经 `store` 的 `WriteTx`（SQLite 单写者串行化）；读取走 `ReadSnapshot`。无进程内可变业务状态，除：服务端证书的 `ArcSwap`、`last_seen` 节流表、`RevocationNotifier`。
- 握手校验器在 tokio 阻塞池之外运行：只做 CPU 小计算 + 一次本地 SQLite 点查（微秒级）；用独立的只读连接池，避免被写事务阻塞握手。
- 配对 `redeem` 是低频路径；签发（P-256 签名）在 `spawn_blocking` 中执行。

## 8. 不变式与安全要求

| 不变式 / 要求 | 本模块如何保证 |
| --- | --- |
| **I6 客户端不执行、不持凭据**（核心） | 服务端**从不生成**客户端私钥，只接收 CSR 并只取其中的公钥；设备私钥不可导出、不出设备；配对响应与任何接口都不含 GitHub / 模型凭据或服务端私钥；设备证书不含秘密 |
| **I3（间接）** | 审批签名的验证密钥就是设备证书里的公钥（api.md §6.3）；`verifying_key` 仅对 `active` 设备返回，吊销即失去签名资格；角色实时读库，降级 / 吊销对下一个请求立即生效 |
| 根私钥不常驻 | `bootstrap` 只在内存持有根私钥，加密导出后 `zeroize`；`Pki::open` 启动自检：数据目录与 `secrets/` 中若存在与根证书公钥匹配的私钥 → 拒绝启动并提示"根私钥必须离线保存" |
| 中间 CA 私钥隔离 | 只经 `secrets` 以使用方 `pki` 读取，`api`、`engine`、`sandbox` 等读不到；进程内以 `Zeroizing` 持有，日志与 `Debug` 输出不含密钥 |
| 配对面最小化 | 单向 TLS 面只有 `POST /v1/pair` 一条路由；窗口外端口关闭；令牌高熵、短 TTL、单次、失败即作废、只存哈希、恒定时间比较；`401 pair_token_invalid` 统一，不给枚举 oracle；配对失败是安全信号，单独计数告警（api.md §1.3 第 4 点） |
| 防"抢先配对" | 令牌带外传递且单次；确认码 + 终端显示设备名与指纹让管理员核对；首台设备成为 approver 之后，其余设备默认 `operator`（P-23），抢注的设备也不能审批 |
| 防权限提升 | 签发的叶子参数完全由服务端决定（忽略 CSR 的 CA 位 / EKU / SAN）；`CA:FALSE`、`EKU: clientAuth` 固定；不能自助改角色 / 自吊销以外的操作 |
| 吊销失败即关闭 | 握手时序列号不在册 → 拒绝；吊销无缓存；会话恢复 / 0-RTT 禁用（`api`） |
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
| 数据库从旧备份恢复 | 之后签发的设备证书序列号不在册 → 握手被拒，需重新配对（失败即关闭，宁可误拒）；`device.paired` 事件缺失也无法凭事件重建 → 恢复演练文档（`deploy`）应写明 |
| 时钟偏差 | 签发 `notBefore` 回拨 5 分钟；服务端时钟异常（早于最近一次签发时间）时拒绝签发并告警 |
| `recover` 非 root | 立即拒绝，不触碰数据库，退出码非 0 |
| `recover` 指向不存在 / 已吊销设备 | 不存在 → 报错；已吊销 → 成功且无副作用 |
| 启动发现根私钥在服务端 | 拒绝启动，日志说明处置（导出到离线介质并删除，或重新引导） |
| 根私钥导出文件 / 口令丢失 | 无法轮换中间 CA；到期后全部设备重新配对（deploy 的备份引导须强调） |

## 10. 配置

`pki` 的参数是**引导配置**（`deploy` 写入的配置文件，重启生效），不是 settings（config.md OQ-18 不收录；这也使 `pki` 不受 I7 的"设置页"约束）。下列默认值**全部是建议值**，除标注"强制"外。

| 配置项 | 默认 | 说明 |
| --- | --- | --- |
| `pki.data_dir` | `deploy` 定 | 根证书（公钥）、中间 / 服务端证书 PEM |
| `pki.server_names` | `deploy` 定 | 服务端叶子 SAN；变化触发重新签发 |
| `pki.pair_listen` / `pki.api_listen` / `pki.api_base_url` | `deploy` 定 | 端口由部署决定，api.md 不固定；`api_base_url` 写入配对响应 |
| `pki.root_validity` | 10 年 | 仅 `bootstrap` |
| `pki.intermediate_validity` | 2 年 | `bootstrap` 与 `rotate` |
| `pki.server_cert_validity` / `pki.server_cert_renew_before` | 90 天 / 30 天 | — |
| `pki.client_cert_validity` | 30 天 | 见 OQ-K6：续期路由落地前这个值会迫使每 30 天重新配对 |
| `pki.not_before_skew` | 5 分钟 | — |
| `pki.superseded_grace` | 10 分钟 | 续期后旧证书仍可握手的宽限 |
| `pki.pair_token_ttl` | 10 分钟 | design.md 示例 |
| `pki.pair_max_failures` | 5 | 失败达到次数令牌作废 |
| `pki.max_csr_bytes` | 4096 | — |
| `pki.device_name_max_chars` | 64 | 与重命名一致（api.md §4.10） |
| `pki.last_seen_throttle` | 60 秒 | — |
| `pki.key_algorithm`（强制） | `ecdsa-p256` | 不可配置为其他值 |
| `pki.tls_min_version`（强制） | `1.3` | 不可配置 |

## 11. 可观测性

- **事件**（data-model §5.2）：`device.paired`、`device.updated`（改名、改角色、续期换钥）、`device.revoked`。均为全局事件（`project_id = null`）；`actor`：配对 `{system, pki}`，API 触发为 `{device, dev_…}`，recover 为 `{system, host_recover}`。事件 `data` 不含令牌、私钥、CSR。
- **日志**（结构化）：配对会话创建 / 兑换 / 作废（令牌只记哈希前 8 位）、握手拒绝（原因、序列号，不含证书全文）、证书签发（kind、serial、subject CN、notAfter）、续签结果、`recover` 的执行者 uid 与参数、启动自检结果。
- **指标**：`pki_handshake_total{result,reason}`、`pki_pairing_attempts_total{result}`（`ok` / `token_invalid` / `validation` / `voided`；失败突增 = 安全信号，应告警）、`pki_certs_issued_total{kind}`、`pki_cert_expiry_seconds{kind=root|intermediate|server}` 与设备证书最小剩余（仪表）、`pki_active_devices{role}`、`pki_pairing_window_open`（仪表）、`pki_revocations_total`。
- **告警建议**：服务端叶子剩余 < 14 天；中间 CA 剩余 < 90 天；配对失败 5 分钟内 > N；启动自检失败；`unknown_serial` 握手拒绝出现（可能是备份恢复或伪造）。
- **健康**：并入 `GET /v1/settings/status` 之外的运维健康检查（`ok` / `degraded` / `unhealthy`）。

## 12. 测试与验收

### 12.1 测试替身与夹具

| 替身 / 夹具 | 用途 |
| --- | --- |
| 真实 `rcgen` / `rustls` / `rustls-webpki` | 本模块是被测方，**不用 `TestCA` 代替自己**；用真实 CA 材料跑真实 TLS 握手（`tokio-rustls` 的客户端 / 服务端对） |
| `TestCA`（M0，`contract` 泳道） | 给 `api`、`fe-desktop` 用；本模块测试里用它构造"外部 CA 签发""过期""尚未生效""错误 EKU"等**反例证书**，并校验 `TestCA` 签出的叶子能通过本模块的 verifier（二者参数一致）。若 `TestCA` 缺 `issue_expired` / `issue_not_yet_valid` / `issue_from_foreign_ca` / `issue_server_auth_only` 等变体，提替身 PR |
| `InMemoryStore`、`FakeClock`、`SeededRng` | 事务、时间、随机数可控；崩溃类测试在提交前注入失败 |
| `InMemorySecrets` | 实现 `SecretStore`，可注入"读不到"故障 |
| 固定 CSR 夹具 | P-256、P-384、RSA-2048、Ed25519 各一份；带 `CA:TRUE` / 通配 SAN / `serverAuth` 请求扩展的恶意 CSR；签名被破坏的 CSR；超长 / 多块 PEM |

### 12.2 验收项

**引导与证书参数（M1）**

- [ ] `bootstrap` 产出：根（自签、`CA:TRUE pathlen:1`、`keyCertSign|cRLSign`、10 年）、中间（`pathlen:0`、2 年）、服务端叶子（`serverAuth`、SAN = `server_names`、90 天）；全部 P-256 / `ecdsa-with-SHA256`；用 `x509-parser` 逐项断言。
- [ ] `bootstrap` 之后服务端数据目录与 `secrets/` 中**没有**根私钥（按公钥匹配全盘扫描断言）；导出文件需口令才能解开；`Pki::open` 在目录里放入匹配根公钥的私钥时拒绝启动。
- [ ] 设备叶子：`CA:FALSE`、`EKU = clientAuth` 仅此一项、无 DNS / IP SAN、`CN = dev_…`、`notBefore` 早于签发时刻 5 分钟、`notAfter ≤ 中间 CA notAfter`、序列号为正且全库唯一；链 `[叶子, 中间]` 可验证到根。
- [ ] `ca_fingerprint` 等于根证书 DER 的 SHA-256；`pubkey_fpr` 等于设备 SPKI DER 的 SHA-256；两者格式为 `sha256:` + 64 位小写十六进制。

**配对（M1）**

- [ ] 成功路径：令牌 + 合法 CSR → `201`，含 api.md §1.3 列出的全部字段；`devices`、`device_certs` 各一行；一条 `device.paired`；`confirmation_code` 为 6 位数字且与会话行一致。
- [ ] 第一台设备 `role = approver`，第二台起 `operator`；两个并发的首次配对恰好产生一台 `approver`。
- [ ] 令牌：单次有效（第二次兑换 `401`）；过期（`FakeClock` 推进 10 分钟）→ `401`；作废（5 次错误）→ `401`；无效 / 已用 / 过期 / 作废四种情形的 `401` 响应体与头**逐字节相同**。
- [ ] 数据库与日志中搜不到明文令牌（扫描库文件与捕获的 `tracing` 输出）；库里只有 SHA-256。
- [ ] 新会话作废旧会话：旧令牌立即 `401`。
- [ ] CSR 反例逐个拒绝（`422`、令牌不消耗）：RSA、Ed25519、P-384、CSR 签名被破坏、超长、多块 PEM、公钥已属于 `active` 设备。
- [ ] 恶意 CSR（请求 `CA:TRUE`、`keyCertSign`、通配 SAN、`serverAuth`）能被受理，但签出的证书不含这些扩展，subject 被重写为 `CN = <device.id>`。
- [ ] `name` 含控制字符 / 超 64 字符 / 空 → `422`；`platform` 不在枚举 → `422`；未知字段 → `422 unknown_field`。
- [ ] 终端打印：设备名中的 ANSI 转义序列与换行在输出里被剥离。
- [ ] 配对窗口：存在 `open` 会话时 `window()` 为 `Some`；兑换成功 / 作废 / 过期 / 取消后 1 秒内变为 `None`（与 `api` 联调：窗口外配对端口拒绝连接）。
- [ ] 令牌兑换成功后，用返回的证书对 mTLS 监听器做一次握手 + `GET /v1/me` 成功（与 `api` 联调，M1 退出标准的前置）。
- [ ] 配对串编码往返测试（待 OQ-K3 契约后补测试向量）；客户端用 `fp` 钉扎根指纹，错误指纹的服务端被拒（模拟中间人）。

**握手校验（M1）**

- [ ] 有效设备证书 → 握手成功；`api` 取到的 `cert_serial` 与 Device 一致。
- [ ] 以下一律握手失败且不产生任何 HTTP 响应：无证书、过期、尚未生效、外部 CA 签发、未知中间 CA、仅 `serverAuth` 的证书、非 P-256 密钥、`CA:TRUE` 的叶子、`CN` 不是 `dev_…`。
- [ ] 吊销：`revoke` 提交后**下一次握手**即失败（无重启、无缓存刷新）；该设备所有证书序列号均失败。
- [ ] 序列号链有效但库中不存在 → 握手失败（失败即关闭）。
- [ ] 时钟偏差：客户端时钟快 4 分钟的设备仍能握手；快 6 分钟则 `not_yet_valid`。
- [ ] 吊销后无法恢复旧 TLS 会话、无法使用 0-RTT（用支持会话恢复的 rustls 客户端实测；断言在 `api` 的 `ServerConfig` 上，本模块提供联调用例）。
- [ ] 中间 CA 钉扎：用同一根签发的"未登记中间 CA"所签叶子被拒。

**设备管理与吊销（M1）**

- [ ] `set_role` 后同一设备的下一次 `by_cert_serial` 立刻返回新角色；自改角色 → `SelfRoleChange`；不存在 → `NotFound`；事件 `device.updated` 的 `changed.role = {from, to}`。
- [ ] `revoke`：设备与全部证书置吊销，发 `device.revoked`；重复吊销 `AlreadyRevoked` 且不重复发事件；可吊销自己；吊销后 `verifying_key` 返回 `None`。
- [ ] `set_role` / `revoke` 在调用方事务回滚时不留任何副作用、不广播 `RevocationNotifier`。
- [ ] `rename`：1–64 字符；越界 / 控制字符拒绝；发 `device.updated`。
- [ ] `touch_last_seen` 连续调用 100 次只写库 1 次（60 s 内）；时钟推进 61 s 后再写 1 次；不产生事件。
- [ ] 吊销后已建立的事件流在心跳间隔内断开（上限 60 s，M1 退出标准；与 `api` 联调，用 `FakeClock` 驱动心跳）。

**续期（M1 服务端叶子；设备叶子接口）**

- [ ] 服务端叶子：`FakeClock` 推进到剩余 ≤ 30 天，后台任务签发新叶子并热替换；已建立的连接不断；新握手拿到新叶子；客户端固定的根指纹不变；新叶子 `notAfter ≤ 中间 CA notAfter`。
- [ ] 续签失败（`secrets` 读不到中间 CA 私钥）：继续使用旧叶子，健康 `degraded`，重试；剩余 ≤ 7 天 `unhealthy`。
- [ ] 设备 `renew`（内部接口，路由待 OQ-K6）：沿用旧密钥 → 新序列号、`pubkey_fpr` 不变；换新密钥 → `pubkey_fpr` 变化并发 `device.updated`；旧证书宽限期后握手失败；吊销作用于新旧全部序列号。

**应急命令（M1）**

- [ ] 非 root 运行 `recover` → 拒绝、退出码非 0、数据库零写入。
- [ ] root：`revoke` 吊销目标设备，事件 `actor = {system, host_recover}`、`data.via = "host_recover"`；`promote` 把一台 `operator` 提升为 `approver`；`list` 输出已剥离控制字符。
- [ ] 吊销唯一的 approver 后，新配对的设备是 `operator`（不是 approver），用 `promote` 恢复审批能力；守护进程在 ≤ 15 s 内关闭被吊销设备的事件流，无需重启。
- [ ] `recover` 运行后数据目录的文件属主仍是服务用户（未留下 root 属主文件）；守护进程可继续读写。
- [ ] 幂等：重复吊销 / 重复提升不重复发事件。

**轮换（M3）**

- [ ] `rotate_intermediate` 用加密的根私钥导出 + 口令：新旧中间 CA 并行期内两条链签发的设备证书都能握手；旧中间 CA 退役后其证书被拒；根指纹不变，客户端无需重新配对；口令错误 → 失败且不改变状态。
- [ ] 轮换过程中根私钥只在内存，用后 `zeroize`；服务端数据目录仍没有根私钥。
- [ ] 吊销时进程内即时通知（`RevocationNotifier`）：订阅者在事务提交后 100 ms 内收到；回滚不通知；接收方 `Lagged` 时退回全量检查。

**手机（M4）**

- [ ] `platform = ios` 配对成功并如实记录；配对串长度能放进二维码（版本 ≤ 10，纠错级别 M）；`api` 对该设备给出 `mobile` 档案（联调）。

**模糊 / 属性测试（建议）**

- [ ] 对 CSR 解析、令牌解析、配对请求体做 `cargo fuzz`：无 panic、无超时、无内存增长。
- [ ] 属性：任意并发的 `redeem` / `revoke` / `set_role` 序列下，恒有"设备 `revoked` 则其所有证书 `revoked`"、"至多一台设备在创建时被赋予 approver（当时库为空）"。

## 13. 开放问题

| # | 问题 | 本文的临时处理（不构成决定） | 影响 |
| --- | --- | --- | --- |
| OQ-K1 | 安装时引导工具的命令形态与由谁调用（`deploy` 的安装脚本调用 `pki::bootstrap::run` 的包装命令）；根私钥导出介质与口令方案；是否支持 TPM / 硬件密钥存放中间 CA（design.md 提过"可选 TPM"，architecture 未采纳） | 库函数 `bootstrap::run`；加密 PKCS#8 导出到指定路径；不支持 TPM | `deploy` |
| OQ-K2 | 指纹口径：① 配对串里的 `ca_fingerprint` 指根还是中间 CA（api.md 的 `certificate_chain` 为 `[叶子, 中间 CA]`，不含根，客户端若只钉扎根就无法仅凭响应验证）；② `pubkey_fpr` 对 SPKI DER 还是裸公钥点；③ `cert_serial` 的位数与编码（api.md OQ-10） | 钉扎**根**；服务端握手链含根；`pubkey_fpr` = SPKI DER 的 SHA-256；序列号 128 位随机、小写十六进制 | api.md、`desktop-shell`、`mobile` |
| OQ-K3 | 配对串的编码、配对监听器的端口与是否按需开启、窗口时长（api.md OQ-12） | §7.4.2 的 URL 形式；仅在会话窗口内开启；10 分钟 | api.md §1.3、`deploy`、`desktop-shell`、`mobile` |
| OQ-K4 | `confirmation_code` 核对失败的处置：设备在核对前是否已 `active`、有没有 abort / confirm 路由；码是随机还是由 TLS exporter + CSR 公钥派生（SAS） | 设备在 `201` 时即 `active`；无 abort 路由；码为 CSPRNG 随机 6 位；不一致就吊销 | api.md、安全强度 |
| OQ-K5 | 吊销（含自吊销）最后一台 approver 是否需要额外保护；`device.revoked` 是否带 `remaining_approvers` | 不加限制，事件带计数供 UI 提示；恢复靠 `recover` | `desktop-ui`、data-model 事件 Schema |
| OQ-K6 | **设备证书续期**：证书"短期有效"（api.md §2.1）但没有续期路由（api.md OQ-12），milestones 也没有任何泳道排期。有效期 30 天意味着每月重新配对。可选：① 把续期路由放进 M1；② M1 先用较长有效期（如 365 天）；③ 保持 30 天并接受重配 | 默认 30 天 [建议值]；内部 `renew` 接口随 M1 落地，路由不做 | api.md、milestones、`desktop-shell`（"证书将过期"提示） |
| OQ-K7 | 主机命令与守护进程的通信：直连数据库（本文）还是本机管理 socket；`recover` 写入的事件是复用 `device.revoked` / `device.updated`（actor = system）还是新增 `device.recovered`；`gantry pair` / `recover` 的完整参数形态 | 直连数据库 + 轮询；复用现有事件，`data.via = "host_recover"` | data-model §5.2、`store`（跨进程订阅）、`deploy` |
| OQ-K8 | 审批签名密钥 = TLS 密钥（api.md OQ-6 的现状）还是独立的审批密钥：设计评审建议分开生成（mTLS 密钥要能无提示后台重连，审批密钥要求每次用户在场）。若分开，配对需随 CSR 登记第二把公钥，`Device` 需新增字段，签名校验用审批公钥 | 按 api.md 现状：用设备证书公钥验签；`verifying_key` 即该公钥 | data-model Device、api.md §1.3 与 §6.3、`desktop-shell` |
| OQ-K9 | 同一公钥重复配对的处置（本文：拒绝）；已吊销设备的公钥是否允许再次配对 | 仅拒绝与 `active` 设备重复 | — |
| OQ-K10 | 服务端叶子的私钥是否由 `secrets` 托管（本文：是，使用方 `pki`）；`secrets` 对使用方隔离与轮换的接口 | 假定 `SecretStore` 提供 `get / put / delete` 并按使用方隔离 | `secrets` 规格 |
