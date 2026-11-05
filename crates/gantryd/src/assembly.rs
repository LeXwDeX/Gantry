//! 模块装配（architecture §3、§4）。
//!
//! 模块之间只经 `core::ports` 相连（P-51）：`Assembly` 持有每个端口的
//! `Arc<dyn …>`，消费方从这里取依赖。某个模块从替身换成真实实现时，只改
//! 构造 `Assembly` 的那一行，不改模块代码（#101 完成标准）。
//!
//! 服务端二进制只有真实装配 [`assemble_real`]。持久存储（SQLite，store.md §7）
//! 就绪前它拒绝启动，不退回内存存储（失败闭合）。替身装配在
//! `tests/support`，只用 dev-dependency，不进服务端二进制。
//!
//! 各模块的生产构造函数（[`build_policy`]、[`build_notify`]、[`build_secrets`]、
//! [`build_pki`]）也在这里：它们只接收引导配置、settings 与已打开的 store（及其端口
//! 依赖），供 `assemble_real` 与测试装配共用，保证“替换替身”验证的是同一条生产
//! 构造路径。
//!
//! `secrets` 与 `pki` 的生产构造入口已经就绪，但 `assemble_real` **不**打开它们：
//! 主密钥来源未定（secrets.md OQ-2、deploy OPS-08），`pki.ca_dir` 尚无引导配置
//! 字段映射；接线前必须先确定配置与进程加固顺序。

use std::sync::Arc;
use std::time::Duration;

use gantry_conversation::engine_api::EngineApiAdapter;
use gantry_conversation::ports::{EngineApi, ModelsPort};
use gantry_conversation::{Conversation, ConversationDeps, Limits};
use gantry_core::egress::{
    ConfigView, EgressGroup, EgressProfile, InjectionCredentialView, InjectionKind, InjectionView,
    ProviderConfig,
};
use gantry_core::entities::Seq;
use gantry_core::ids::{Clock, Entropy, IdGen};
use gantry_core::ports::conversation::ConversationService;
use gantry_core::ports::egress::{EgressConfigView, EgressControl};
use gantry_core::ports::forge::{ForgeReader, OutboxPort};
use gantry_core::ports::inbox::InboxPort;
use gantry_core::ports::policy::PolicyPort;
use gantry_core::ports::repo::{RepoService, WorktreeLocks};
use gantry_core::ports::sandbox::SandboxService;
use gantry_core::ports::worker::{ImageRef, WorkerAdapter};
use gantry_core::ports::{SecretSink, SecretsError};
use gantry_core::secret::Consumer;
use gantry_core::settings::resource::{LevelEntry, Levels, Models};
use gantry_core::time::Timestamp;
use gantry_egress::client::EgressClient;
use gantry_engine::engine::Engine;
use gantry_engine::runtime::EngineRuntime;
use gantry_forge::{
    Dispatcher, DispatcherDeps, DispatcherOptions, Executor, ExecutorDeps, ExecutorOptions,
    FaultInjector, Forge, ForgeConfig, ReaderDeps, TokenProvider,
};
use gantry_models::{
    AnthropicProvider, EffortTable, ModelRouter, RouterClock, RouterDeps, StoreSettings, ViewCache,
};
use gantry_notify::Notify;
use gantry_pki::{Pki, PkiConfig, PkiError};
use gantry_policy::{PolicyApprovals, PolicyDeps, PolicyEngine};
use gantry_repo::{GitRepo, RepoConfig, RepoDeps};
use gantry_secrets::{MasterKeySource, Secrets, SecretsConfig};
use gantry_store::{ModuleSpec, Store, StoreConfig, StoreDeps};
use serde::Serialize;

/// 装配来源；`status` 原样报告。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Real,
    Fakes,
}

/// 一个模块当前由谁提供；`status` 原样报告，便于看出哪些还是替身。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Real,
    Fake,
    /// 还没有消费方或实现，未装配。
    Absent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModuleInfo {
    pub module: &'static str,
    pub provider: Provider,
    pub note: &'static str,
}

impl ModuleInfo {
    pub fn new(module: &'static str, provider: Provider, note: &'static str) -> Self {
        Self {
            module,
            provider,
            note,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AssemblyError {
    #[error("module {module} is not ready in the real assembly: {why}")]
    NotReady {
        module: &'static str,
        why: &'static str,
    },
    #[error(transparent)]
    Store(#[from] gantry_store::StoreError),
}

/// 装配好的进程内模块。字段即端口；没有实现的端口不出现。
pub struct Assembly {
    pub profile: Profile,
    pub store: Store,
    pub ids: Arc<IdGen>,
    /// 引擎运行时（engine.md §7.15）：`serve` 负责 `run` 到停机；写入口经 `engine()`。
    pub engine: Arc<EngineRuntime>,
    pub inbox: Arc<dyn InboxPort>,
    pub policy: Arc<dyn PolicyPort>,
    pub outbox: Arc<dyn OutboxPort>,
    pub forge: Arc<dyn ForgeReader>,
    pub repo: Arc<dyn RepoService>,
    pub locks: Arc<dyn WorktreeLocks>,
    pub sandbox: Arc<dyn SandboxService>,
    pub worker: Arc<dyn WorkerAdapter>,
    pub models: Arc<dyn ModelsPort>,
    pub egress: Arc<dyn EgressControl>,
    pub conversation: Arc<dyn ConversationService>,
    pub modules: Vec<ModuleInfo>,
}

/// 已接入生产构造函数的模块私有表（store.md §7.10）。notify 的状态在 store
/// 内置的 `inbox_items` 投影里，不需要私有表。sandbox 的 `BuiltinSchemas` 属于
/// sandbox 片，本片不构造 sandbox，也不注入。
pub fn module_specs() -> Vec<ModuleSpec> {
    vec![
        gantry_policy::POLICY_MODULE,
        gantry_pki::store_schema::module_spec(),
        gantry_forge::module_spec(),
    ]
}

/// `gantryd` 的 M1 实例承载的 `secrets` 使用方：Forge、Pki（secrets.md §10；
/// deploy.md §7.4 在 M4 才加入 Notify）。**不包含 Egress**：Egress 是独立进程、
/// 独立实例与独立主密钥。
pub fn secrets_consumers() -> Vec<Consumer> {
    vec![Consumer::Forge, Consumer::Pki]
}

/// `secrets` 的生产构造：使用引导配置的 `server.secrets_dir`，只 `open` 已有
/// 实例，绝不 `init`（初始化归 deploy）。
pub fn build_secrets(
    config: &crate::config::BootConfig,
    master_key: Box<dyn MasterKeySource>,
) -> Result<Secrets, SecretsError> {
    Secrets::open(SecretsConfig::new(
        &config.server.secrets_dir,
        secrets_consumers(),
        master_key,
    ))
}

/// `pki` 的生产构造。`config`、store、按 `Consumer::Pki` 限定的 secrets 句柄与
/// 时钟/熵均由调用方给出；本函数只调用真实 [`Pki::open`]。
pub async fn build_pki(
    config: PkiConfig,
    store: &Store,
    secrets: Arc<dyn SecretSink>,
    clock: Arc<dyn Clock>,
    entropy: Arc<dyn Entropy>,
) -> Result<Pki, PkiError> {
    Pki::open(config, store.clone(), secrets, clock, entropy).await
}

/// policy 的生产构造：`PolicyEngine`（`PolicyPort`）与 `ApprovalService` 共用同一
/// 实例与 store。审批 nonce 用操作系统熵源。
pub struct PolicyStack {
    pub port: Arc<dyn PolicyPort>,
    pub approvals: PolicyApprovals,
}

pub fn build_policy(
    store: &Store,
    forge: Arc<dyn ForgeReader>,
    clock: Arc<dyn Clock>,
    ids: Arc<IdGen>,
) -> PolicyStack {
    let engine = Arc::new(PolicyEngine::new(PolicyDeps {
        store: store.clone(),
        forge,
        clock,
        ids,
    }));
    let port: Arc<dyn PolicyPort> = engine.clone();
    let approvals = PolicyApprovals::new(engine, Arc::new(OsEntropy));
    PolicyStack { port, approvals }
}

/// notify 的生产构造；状态在 store 的内置投影里，不注册私有表。
pub fn build_notify(store: &Store, ids: Arc<IdGen>) -> Arc<dyn InboxPort> {
    Arc::new(Notify::new(store.clone(), ids))
}

/// models 的生产构造：模型配置与推理深度来自共享 store 的设置投影（`StoreSettings`
/// 按修订缓存视图）；提供方凭据由注入的 Egress 持有，调用只经其回环。本函数接入
/// Egress 端口、ID / 熵 / 时钟和调用方给出的推理深度能力表（`EffortTable`），不
/// 内置模型能力数据（models.md OQ-1 / OQ-8）。
///
/// `assemble_real` 尚未调用本函数；测试装配经本函数构造真实 `ModelRouter`。
/// 生产 `worker` 适配器：只声明镜像、命令、非密环境变量、令牌变量与能力表。
/// **不在本进程运行沙盒内二进制**；实际执行由沙盒中的 worker 完成。
///
/// K12：镜像由调用方按 `sandbox.base_image`（digest 固定）给出；M1 适配器持有固定
/// `ImageRef`，设置刷新归 #318。生产装配缺少该设置时保持 `NotReady`，不猜默认镜像。
pub fn build_worker(image: ImageRef) -> gantry_worker::claude_code::ClaudeCodeAdapter {
    gantry_worker::claude_code::ClaudeCodeAdapter::new(image)
}

pub fn build_models(
    store: &Store,
    egress: Arc<dyn EgressControl>,
    ids: Arc<IdGen>,
    entropy: Arc<dyn Entropy>,
    clock: Arc<dyn RouterClock>,
    efforts: EffortTable,
) -> ModelRouter {
    ModelRouter::new(RouterDeps {
        views: ViewCache::new(Arc::new(StoreSettings::new(store.clone()))),
        providers: vec![Arc::new(AnthropicProvider::new(efforts))],
        egress,
        store: store.clone(),
        entropy,
        ids,
        clock,
    })
}

/// conversation 的生产构造：会话写路径经 [`EngineApiAdapter`] 调用共享的
/// [`Engine`]，模型经 `models` 端口；会话本身不写设置（I7）。
///
/// `low` 与 `config_rev` 来自已应用的 settings 快照；`models` 的生产实例是
/// [`build_models`] 的 `ModelRouter`。
pub fn build_conversation(
    store: &Store,
    engine: Arc<Engine>,
    ids: Arc<IdGen>,
    models: Arc<dyn ModelsPort>,
    low: LevelEntry,
    config_rev: Seq,
) -> Arc<Conversation> {
    let engine: Arc<dyn EngineApi> =
        Arc::new(EngineApiAdapter::new(engine, ids.clone(), low, config_rev));
    Arc::new(Conversation::new(ConversationDeps {
        store: store.clone(),
        ids,
        engine,
        models,
        limits: Limits::default(),
    }))
}

/// `egress` 配置视图（config.md §8、K11）：`egress.*` 与从 `models.providers`
/// 派生的模型 API 注入项、`levels.token_weights`。只含 `secret_ref`，不含秘密值。
pub fn egress_config_view(
    egress: &gantry_core::settings::resource::Egress,
    models: &Models,
    levels: &Levels,
    rev: Seq,
) -> EgressConfigView {
    let groups = egress
        .groups
        .iter()
        .map(|(name, group)| {
            (
                name.clone(),
                EgressGroup {
                    hosts: group.hosts.clone(),
                },
            )
        })
        .collect();
    let profiles = egress
        .profiles
        .iter()
        .map(|(name, profile)| {
            (
                name.clone(),
                EgressProfile {
                    groups: profile.groups.clone(),
                },
            )
        })
        .collect();
    // `injection` 只放私有包仓库；模型 API 的注入由 `providers` 表达，
    // 不在两处重复 `secret_ref`（config.md §8.1）。
    let injection: std::collections::BTreeMap<String, InjectionView> = egress
        .injection
        .iter()
        .map(|(id, item)| {
            let view = InjectionView {
                hosts: item.hosts.clone(),
                kind: match item.kind {
                    gantry_core::settings::resource::EgressInjectionKind::Bearer => {
                        InjectionKind::Bearer
                    }
                    gantry_core::settings::resource::EgressInjectionKind::Header => {
                        InjectionKind::Header
                    }
                },
                header: item.header.clone(),
                credential: InjectionCredentialView {
                    secret_ref: item.credential.secret_ref.clone(),
                },
            };
            (id.clone(), view)
        })
        .collect();
    ConfigView {
        rev: rev.as_u64(),
        providers: models
            .providers
            .iter()
            .map(|(id, provider)| {
                (
                    id.clone(),
                    ProviderConfig {
                        kind: "anthropic".to_owned(),
                        base_url: provider.base_url.clone(),
                        secret_ref: provider.credential.secret_ref.clone(),
                    },
                )
            })
            .collect(),
        groups,
        profiles,
        default_profile: egress.default_profile.clone(),
        injection,
        token_weights: levels.token_weights,
    }
}

/// egress 控制面客户端；`gantryd` 经它下发配置与计量。实例由独立的 egress 进程提供，
/// 本函数不启动该进程。
pub fn build_egress(
    socket: impl Into<std::path::PathBuf>,
    clock: Arc<dyn Clock>,
    issued_to: gantry_core::egress::IssuedTo,
) -> EgressClient {
    EgressClient::new(socket, clock, issued_to)
}

/// repo 的生产构造：`RepoDeps` 的时钟必须与 store 共用，`policy` 与 `cas` 由调用方注入。
pub async fn build_repo(
    config: RepoConfig,
    deps: RepoDeps,
) -> Result<GitRepo, gantry_core::ports::RepoError> {
    GitRepo::open(config, deps).await
}

/// forge 装配输入：引导配置、共享 store 之外的令牌来源与 GitHub/Git 地址。
///
/// `ForgeConfig` 由调用方从项目绑定构造（forge.md §7.12）；本函数不读取设置，
/// 也不创建 App 凭据（生产 App 私钥来源按 #405 / secrets.md OQ-2 待决）。
#[derive(Clone)]
pub struct ForgeDeps {
    pub config: ForgeConfig,
    pub tokens: Arc<dyn TokenProvider>,
    /// GitHub REST 根地址。
    pub api_base: String,
    /// Git 远端根地址，拼成 `{git_base}/{owner}/{repo}.git`。
    pub git_base: String,
    pub http_timeout: Duration,
    pub git_timeout: Duration,
    pub dispatcher_options: DispatcherOptions,
}

/// forge 的第一阶段产物：同一 [`Forge`] 上的 dispatcher 与无 mirror 的 reader。
///
/// `reader` 供 `policy` 使用（它只读 store 快照并可按需对账）；第二阶段再以真实
/// repo 与 policy 构造 executor 和带 mirror 的完整 reader（#105 S3a 决定）。
pub struct ForgeStack {
    pub forge: Arc<Forge>,
    pub dispatcher: Arc<Dispatcher>,
    pub reader: Arc<dyn ForgeReader>,
    clock: Arc<dyn Clock>,
    deps: ForgeDeps,
}

/// 第一阶段：构造 Forge、Dispatcher 与无 mirror 的 Reader。所有实例共享同一
/// store、ForgeConfig 与 dispatcher。
pub fn build_forge(store: &Store, clock: Arc<dyn Clock>, deps: ForgeDeps) -> ForgeStack {
    let forge = Arc::new(Forge::new(store.clone(), deps.config.clone()));
    let dispatcher = Arc::new(forge.dispatcher(DispatcherDeps {
        tokens: deps.tokens.clone(),
        clock: clock.clone(),
        api_base: deps.api_base.clone(),
        options: deps.dispatcher_options.clone(),
    }));
    let reader: Arc<dyn ForgeReader> = Arc::new(forge.reader(ReaderDeps {
        tokens: deps.tokens.clone(),
        api_base: deps.api_base.clone(),
        clock: clock.clone(),
        http_timeout: deps.http_timeout,
        reconciler: Some(dispatcher.clone()),
        mirror: None,
    }));
    ForgeStack {
        forge,
        dispatcher,
        reader,
        clock,
        deps,
    }
}

/// 第二阶段的 executor 输入：审批、真实 repo 与故障注入。
pub struct ForgeExecutorDeps {
    pub policy: Arc<dyn PolicyPort>,
    pub ids: Arc<IdGen>,
    pub options: ExecutorOptions,
    pub faults: Option<Arc<dyn FaultInjector>>,
    pub repo: Arc<dyn RepoService>,
}

/// 第二阶段：注入 repo 与 policy 构造执行器。失败只来自 git 辅助目录的创建。
pub fn build_forge_executor(
    stack: &ForgeStack,
    deps: ForgeExecutorDeps,
) -> std::io::Result<Executor> {
    stack.forge.executor(ExecutorDeps {
        policy: deps.policy,
        tokens: stack.deps.tokens.clone(),
        api_base: stack.deps.api_base.clone(),
        ids: deps.ids,
        options: deps.options,
        faults: deps.faults,
        repo: deps.repo,
        git_base: stack.deps.git_base.clone(),
    })
}

/// 第二阶段：带 mirror 的完整 reader，供 engine / assembly 使用。它与第一阶段
/// 的 reader 共享同一 store、ForgeConfig 与 dispatcher。
pub fn build_forge_reader(stack: &ForgeStack, repo: Arc<dyn RepoService>) -> Arc<dyn ForgeReader> {
    Arc::new(stack.forge.reader(ReaderDeps {
        tokens: stack.deps.tokens.clone(),
        api_base: stack.deps.api_base.clone(),
        clock: stack.clock.clone(),
        http_timeout: stack.deps.http_timeout,
        reconciler: Some(stack.dispatcher.clone()),
        mirror: Some(gantry_forge::MirrorDeps {
            repo,
            git_base: stack.deps.git_base.clone(),
            git_timeout: stack.deps.git_timeout,
        }),
    }))
}

/// 真实装配。各模块按端口接入的顺序见 `tests/support` 的替身装配。
///
/// 当前只打开持久化 store；notify 也只保留构造点。forge / policy / repo / models
/// 的生产构造函数已就绪，但这里尚未按依赖顺序调用；secrets / pki 的构造函数
/// （[`build_secrets`]、[`build_pki`]）同样未接入，生产主密钥来源未定
/// （secrets.md OQ-2、deploy OPS-08），`pki.ca_dir` 也尚无引导配置字段映射。
/// sandbox、worker、egress、conversation 与 api 尚未接线，仍拒绝启动（失败闭合）。
pub async fn assemble_real(config: &crate::config::BootConfig) -> Result<Assembly, AssemblyError> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(OsEntropy)));
    let modules = module_specs();
    let store = Store::open(
        StoreConfig::new(&config.server.data_dir),
        StoreDeps {
            clock,
            ids: ids.clone(),
        },
        &modules,
    )
    .await?;
    // notify 只依赖 store 与 ID 生成，生产路径可以先构造；消费方（engine/api）
    // 尚未接线，因此本片只保留构造点，不把实例放进 `Assembly`。
    let _notify = build_notify(&store, ids);
    // 生产凭据来源和 CA 配置尚未接入，不在这里猜测或初始化。
    Err(AssemblyError::NotReady {
        module: "sandbox",
        why: "real assembly is incomplete (gantryd.md §7.1): persistent store and \
              notify are ready, forge/policy/repo/models constructors exist but are \
              not wired here, and sandbox/worker/egress/conversation/api plus the \
              production secrets/pki configuration are still missing; refusing to run",
    })
}

/// 墙钟。毫秒精度足够 ULID 与事件时间戳；早于纪元按 0 处理。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        Timestamp::from_millis(ms)
    }
}

/// 操作系统熵源。取不到随机数时进程不能安全地生成 ID，直接中止。
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("OS entropy source unavailable");
    }
}
