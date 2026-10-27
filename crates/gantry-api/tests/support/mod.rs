//! 管线测试的共用替身：计数的设备目录、内存 store、可脚本化的处理程序。
#![allow(dead_code)]

pub mod doubles;
pub mod pki_env;
pub mod settings_ctx;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use gantry_api::idempotency::MemoryIdempotency;
use gantry_api::server::{HandlerRequest, Reply, RouteHandler};
use gantry_api::{ApiConfig, ApiCore, ApiResponse, PeerCert, Problem, RequestHead, Services};
use gantry_core::entities::Actor;
use gantry_core::entities::{Device, DevicePlatform, DeviceRole, DeviceStatus, Seq};
use gantry_core::ids::{DeviceId, IdGen};
use gantry_core::testing::builders;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_core::time::Timestamp;
use gantry_pki::admin::{
    DeviceAdmin, DeviceError, RenewError, RenewRequest, RenewResponse, ReplayReport, RevokeOutcome,
};
use gantry_pki::directory::DeviceDirectory;
use gantry_pki::{CertSerial, PkiError};
use gantry_store::WriteTx;
use gantry_store::{Store, StoreDeps};
use http::{Method, StatusCode};
use serde_json::{json, Value};

pub const OPENAPI: &str = include_str!("../../../../schemas/openapi.yaml");
pub const T0: i64 = 1_793_000_000_000;

/// 计数的设备目录：断言"每个请求恰好读一次 `by_cert_serial`"（角色 / 状态不缓存）。
#[derive(Default)]
pub struct Devices {
    by_serial: Mutex<HashMap<String, Device>>,
    pub lookups: AtomicUsize,
    pub fail: Mutex<bool>,
}

impl Devices {
    pub fn insert(&self, device: Device) {
        self.by_serial
            .lock()
            .unwrap()
            .insert(device.cert_serial.clone(), device);
    }

    pub fn update(&self, serial: &str, f: impl FnOnce(&mut Device)) {
        f(self.by_serial.lock().unwrap().get_mut(serial).unwrap());
    }

    pub fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

/// 管线测试只需要目录的读取；其余方法在这些测试里不会被调用。
#[async_trait]
impl DeviceDirectory for Devices {
    async fn by_cert_serial(&self, serial: &CertSerial) -> Result<Option<Device>, PkiError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if *self.fail.lock().unwrap() {
            return Err(PkiError::Store("unavailable".into()));
        }
        Ok(self.by_serial.lock().unwrap().get(serial.as_str()).cloned())
    }

    async fn get(&self, id: &DeviceId) -> Result<Option<Device>, PkiError> {
        Ok(self
            .by_serial
            .lock()
            .unwrap()
            .values()
            .find(|d| &d.id == id)
            .cloned())
    }

    async fn list(
        &self,
        _page: &gantry_store::Page,
    ) -> Result<gantry_store::Paged<Device>, PkiError> {
        unimplemented!("pipeline tests do not list devices")
    }

    async fn verifying_key(
        &self,
        _id: &DeviceId,
    ) -> Result<Option<p256::ecdsa::VerifyingKey>, PkiError> {
        Ok(None)
    }

    async fn bootstrap_period(&self) -> Result<bool, PkiError> {
        Ok(false)
    }

    async fn touch_last_seen(&self, _id: &DeviceId) {}
}

/// 管线测试不经设备管理；调用即说明测试走错了路径。
pub struct NoAdmin;

#[async_trait]
impl DeviceAdmin for NoAdmin {
    async fn rename(&self, _: &DeviceId, _: &str, _: &Actor) -> Result<Device, DeviceError> {
        unimplemented!("NoAdmin")
    }
    async fn set_role(
        &self,
        _: &mut WriteTx,
        _: &DeviceId,
        _: DeviceRole,
        _: &Actor,
    ) -> Result<Device, DeviceError> {
        unimplemented!("NoAdmin")
    }
    async fn revoke(
        &self,
        _: &mut WriteTx,
        _: &DeviceId,
        _: &Actor,
    ) -> Result<RevokeOutcome, DeviceError> {
        unimplemented!("NoAdmin")
    }
    async fn renew(
        &self,
        _: &DeviceId,
        _: &CertSerial,
        _: RenewRequest,
    ) -> Result<RenewResponse, RenewError> {
        unimplemented!("NoAdmin")
    }
    async fn replay_revocations(&self, _: Timestamp) -> Result<ReplayReport, PkiError> {
        unimplemented!("NoAdmin")
    }
}

/// 记录收到的请求并回 `200 {}` 的处理程序，用于单独检验管线步骤。
#[derive(Default)]
pub struct Echo {
    pub calls: Mutex<Vec<Call>>,
}

/// `(operationId, 请求体, Idempotency-Key)`。
pub type Call = (String, Option<Value>, Option<String>);

#[async_trait]
impl RouteHandler for Echo {
    async fn handle(&self, _core: &ApiCore, req: HandlerRequest<'_>) -> Result<Reply, Problem> {
        self.calls.lock().unwrap().push((
            req.route.operation_id.to_owned(),
            req.body.clone(),
            req.idempotency_key.map(str::to_owned),
        ));
        Ok(Reply::json(StatusCode::OK, json!({}), Seq::ZERO))
    }
}

impl Echo {
    pub fn ops(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }
}

pub struct Harness {
    pub core: ApiCore,
    pub clock: Arc<FakeClock>,
    pub devices: Arc<Devices>,
    pub store: Store,
    pub engine: Arc<gantry_fakes::engine::FakeEngine>,
    pub conversation: Arc<doubles::ConvFaults>,
    pub idempotency: Arc<MemoryIdempotency>,
}

pub fn device(n: u128, platform: DevicePlatform, role: DeviceRole) -> Device {
    let mut d = builders::device();
    d.id = DeviceId::from_parts(T0 as u64, n).unwrap();
    d.cert_serial = format!("5f3a{n:04x}");
    d.platform = platform;
    d.role = role;
    d.status = DeviceStatus::Active;
    d
}

pub fn harness(handler: Option<Arc<dyn RouteHandler>>) -> Harness {
    let clock = Arc::new(FakeClock::new(T0));
    let entropy = Arc::new(SeqEntropy::new(7));
    let ids = Arc::new(IdGen::new(clock.clone(), entropy));
    let ids_for_policy = ids.clone();
    let store = Store::open_in_memory_sync(StoreDeps {
        clock: clock.clone(),
        ids: ids.clone(),
    })
    .unwrap();
    let devices = Arc::new(Devices::default());
    let engine = Arc::new(gantry_fakes::engine::FakeEngine::new());
    let conversation = Arc::new(doubles::ConvFaults::new());
    let idempotency = Arc::new(MemoryIdempotency::new(clock.clone()));
    let svc = Services {
        clock: clock.clone(),
        ids,
        store: store.clone(),
        devices: devices.clone(),
        admin: Arc::new(NoAdmin),
        approvals: {
            let forge = Arc::new(gantry_fakes::forge::FakeForge::new_in(store.clone()));
            policy_approvals(&store, clock.clone(), ids_for_policy.clone(), forge).1
        },
        forge: Arc::new(gantry_fakes::forge::FakeForge::new_in(store.clone())),
        settings_ctx: Arc::new(settings_ctx::FakeSettingsContext::default()),
        egress: Arc::new(gantry_fakes::egress::EgressStub::new()),
        engine: engine.clone(),
        conversation: conversation.clone(),
        idempotency: idempotency.clone(),
    };
    let core = match handler {
        Some(h) => ApiCore::with_handler(ApiConfig::default(), svc, h),
        None => ApiCore::new(ApiConfig::default(), svc),
    };
    Harness {
        core,
        clock,
        devices,
        store,
        engine,
        conversation,
        idempotency,
    }
}

pub fn peer(device: &Device) -> PeerCert {
    PeerCert {
        serial: device.cert_serial.clone(),
        not_before: Timestamp::from_millis(T0 - 10 * 86_400_000),
        not_after: Timestamp::from_millis(T0 + 20 * 86_400_000),
    }
}

pub fn req(method: Method, uri: &str, device: &Device) -> RequestHead {
    RequestHead::new(method, uri, Some(peer(device)))
}

pub fn json_req(method: Method, uri: &str, device: &Device) -> RequestHead {
    req(method, uri, device).header("content-type", "application/json")
}

pub async fn send(h: &Harness, head: RequestHead, body: &str) -> ApiResponse {
    h.core.handle(head, Bytes::from(body.to_owned())).await
}

pub fn body(resp: &ApiResponse) -> Value {
    serde_json::from_slice(resp.body().as_bytes()).expect("response body is JSON")
}

pub fn header<'a>(resp: &'a ApiResponse, name: &str) -> Option<&'a str> {
    resp.headers().get(name).map(|v| v.to_str().unwrap())
}

/// 用 `openapi.yaml` 的组件 schema 校验一个实例。
pub fn validate(component: &str, instance: &Value) {
    let doc: Value = serde_json::from_str(OPENAPI).unwrap();
    let root = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "allOf": [{ "$ref": format!("#/components/schemas/{component}") }],
        "components": doc["components"],
    });
    let validator = jsonschema::validator_for(&root).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{component} invalid: {errors:?}\n{instance:#}"
    );
}

/// 断言 problem+json 的公共形状（api.md §3.1、本文 §12 契约一致性）。
pub fn assert_problem(resp: &ApiResponse, status: u16, code: &str) -> Value {
    assert_eq!(resp.status().as_u16(), status, "{:?}", body(resp));
    assert!(header(resp, "content-type")
        .unwrap()
        .starts_with("application/problem+json"));
    let b = body(resp);
    assert_eq!(b["code"], code);
    assert_eq!(b["status"], status);
    assert_eq!(b["type"], format!("urn:gantry:problem:{code}"));
    assert_eq!(header(resp, "x-request-id").unwrap(), b["request_id"]);
    assert!(header(resp, "x-log-epoch").is_some());
    assert!(header(resp, "access-control-allow-origin").is_none());
    validate("Problem", &b);
    b
}

/// 换一个幂等存储重建同一套服务（模拟重启后 `api_idempotency` 的完成记录丢失）。
pub fn rebuild_with_idempotency(h: &Harness, idempotency: Arc<MemoryIdempotency>) -> Harness {
    let mut svc = h.core.services().clone();
    svc.idempotency = idempotency.clone();
    Harness {
        core: ApiCore::new(ApiConfig::default(), svc),
        clock: h.clock.clone(),
        devices: h.devices.clone(),
        store: h.store.clone(),
        engine: h.engine.clone(),
        conversation: h.conversation.clone(),
        idempotency,
    }
}

/// 真实 policy 的审批服务（FakeForge 作 ForgeReader）。store 需注册 `POLICY_MODULE`
/// 才能使用 challenge / consume；管线测试不触及这些路径。
pub fn policy_approvals(
    store: &Store,
    clock: Arc<FakeClock>,
    ids: Arc<IdGen>,
    forge: Arc<gantry_fakes::forge::FakeForge>,
) -> (
    Arc<gantry_policy::PolicyEngine>,
    Arc<gantry_policy::PolicyApprovals>,
) {
    let engine = Arc::new(gantry_policy::PolicyEngine::new(
        gantry_policy::PolicyDeps {
            store: store.clone(),
            forge,
            clock,
            ids,
        },
    ));
    let approvals = Arc::new(gantry_policy::PolicyApprovals::new(
        engine.clone(),
        Arc::new(SeqEntropy::new(99)),
    ));
    (engine, approvals)
}
