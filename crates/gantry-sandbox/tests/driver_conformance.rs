//! sandbox.md §12.A 第 1、2、3 条：驱动一致性套件。
//!
//! 同一份用例跑在 `FakeDriver` 与 `PodmanDriver` 上。Podman 用例需要 Linux + rootless
//! Podman + 测试镜像里的 `fake-worker`，本机（macOS）与 A 级环境都没有，所以标
//! `#[ignore]` 并写明原因；它们归 M1.4 的 B 级环境（docs/m1-acceptance.md），不冒充通过。

mod common;

use std::time::Duration;

use common::{collect_logs, spec_for, TempRoot};
use gantry_sandbox::driver::{ContainerStatus, LabelSelector, LogCursor, LogStream, SandboxDriver};
use gantry_sandbox::fake::{DriverCall, FakeBehavior, FakeDriver, FakeScript, SIGKILL, SIGTERM};
use gantry_sandbox::podman_driver::{OutReadMode, PodmanConfig, PodmanDriver};
use gantry_sandbox::spec::{ContainerSpec, ImageDigest, Runtime, LABEL_MANAGED, LABEL_SANDBOX_ID};

/// 套件对"怎样让容器表现出某种行为"的抽象：FakeDriver 登记脚本，Podman 改写容器命令。
trait Scenario {
    fn arrange(&self, spec: &mut ContainerSpec, sandbox: u128, kind: Kind);
}

#[derive(Clone, Copy)]
enum Kind {
    /// 运行到被停止；SIGTERM 时退出。
    RunHonorTerm,
    /// 运行到被停止；忽略 SIGTERM。
    RunIgnoreTerm,
    /// 输出三行后退出 0。
    ThreeLines,
}

impl Scenario for FakeDriver {
    fn arrange(&self, _spec: &mut ContainerSpec, sandbox: u128, kind: Kind) {
        let script = match kind {
            Kind::RunHonorTerm => FakeScript {
                behavior: FakeBehavior::Forever { honor_term: true },
                ..FakeScript::default()
            },
            Kind::RunIgnoreTerm => FakeScript {
                behavior: FakeBehavior::Forever { honor_term: false },
                ..FakeScript::default()
            },
            Kind::ThreeLines => FakeScript {
                behavior: FakeBehavior::ExitAfter {
                    after: Duration::from_secs(5),
                    code: 0,
                    honor_term: true,
                },
                log: vec![
                    (Duration::from_secs(1), LogStream::Stdout, b"one".to_vec()),
                    (Duration::from_secs(2), LogStream::Stderr, b"two".to_vec()),
                    (Duration::from_secs(3), LogStream::Stdout, b"three".to_vec()),
                ],
                out: Vec::new(),
            },
        };
        self.script(common::sandbox_id(sandbox), script);
    }
}

async fn create_does_not_start_and_start_runs<D: SandboxDriver>(
    driver: &D,
    scenario: &dyn Scenario,
    root: &TempRoot,
) {
    let (mut spec, layout) = spec_for(root, 1);
    scenario.arrange(&mut spec, 1, Kind::RunHonorTerm);
    driver.prepare_dirs(&layout).await.unwrap();
    let id = driver.create(&spec).await.unwrap();
    assert_eq!(
        driver.inspect(&id).await.unwrap().status,
        ContainerStatus::Created
    );
    driver.start(&id).await.unwrap();
    assert_eq!(
        driver.inspect(&id).await.unwrap().status,
        ContainerStatus::Running
    );
    driver.stop(&id, Duration::from_secs(10)).await.unwrap();
    assert_eq!(
        driver.inspect(&id).await.unwrap().status,
        ContainerStatus::Exited
    );
    driver.remove(&id).await.unwrap();
}

async fn remove_is_idempotent_and_list_selects_by_label<D: SandboxDriver>(
    driver: &D,
    scenario: &dyn Scenario,
    root: &TempRoot,
) {
    let (mut a, la) = spec_for(root, 2);
    let (mut b, lb) = spec_for(root, 3);
    scenario.arrange(&mut a, 2, Kind::RunHonorTerm);
    scenario.arrange(&mut b, 3, Kind::RunHonorTerm);
    driver.prepare_dirs(&la).await.unwrap();
    driver.prepare_dirs(&lb).await.unwrap();
    let ida = driver.create(&a).await.unwrap();
    let idb = driver.create(&b).await.unwrap();

    let mut selector = LabelSelector::default();
    selector.0.insert(LABEL_MANAGED.into(), "1".into());
    let all = driver.list(&selector).await.unwrap();
    assert!(all.iter().any(|c| c.id == ida) && all.iter().any(|c| c.id == idb));

    selector
        .0
        .insert(LABEL_SANDBOX_ID.into(), la.sandbox_id().to_string());
    let only = driver.list(&selector).await.unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].id, ida);

    driver.remove(&ida).await.unwrap();
    driver.remove(&ida).await.unwrap();
    driver.remove(&idb).await.unwrap();
    assert!(driver.list(&selector).await.unwrap().is_empty());
}

async fn logs_are_rereadable_in_stable_order<D: SandboxDriver>(
    driver: &D,
    scenario: &dyn Scenario,
    root: &TempRoot,
) {
    let (mut spec, layout) = spec_for(root, 4);
    scenario.arrange(&mut spec, 4, Kind::ThreeLines);
    driver.prepare_dirs(&layout).await.unwrap();
    let id = driver.create(&spec).await.unwrap();
    driver.start(&id).await.unwrap();
    driver.wait(&id).await.unwrap();
    let first = collect_logs(driver, &id, None).await;
    let lines: Vec<&[u8]> = first.iter().map(|c| c.line.as_slice()).collect();
    assert_eq!(lines, vec![&b"one"[..], b"two", b"three"]);
    assert_eq!(first[1].stream, LogStream::Stderr);
    assert_eq!(collect_logs(driver, &id, None).await, first);
    let tail = collect_logs(driver, &id, Some(first[0].cursor)).await;
    assert_eq!(tail, first[1..].to_vec());
    assert!(collect_logs(driver, &id, Some(LogCursor(3)))
        .await
        .is_empty());
    driver.remove(&id).await.unwrap();
}

async fn invariant_violations_create_nothing<D: SandboxDriver>(
    driver: &D,
    root: &TempRoot,
    count: impl Fn() -> usize,
) {
    let before = count();
    for mutate in common::violations() {
        let (mut spec, _) = spec_for(root, 5);
        mutate(&mut spec);
        let err = driver
            .create(&spec)
            .await
            .expect_err("violation must be rejected");
        assert!(
            matches!(
                err,
                gantry_sandbox::driver::DriverError::InvariantViolated(_)
            ),
            "{err:?}"
        );
    }
    assert_eq!(count(), before);
}

// ---------------------------------------------------------------------------
// FakeDriver（A 级）
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn fake_create_start_stop() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    create_does_not_start_and_start_runs(&driver, &driver, &root).await;
}

#[tokio::test(start_paused = true)]
async fn fake_remove_and_list() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    remove_is_idempotent_and_list_selects_by_label(&driver, &driver, &root).await;
}

#[tokio::test(start_paused = true)]
async fn fake_logs() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    logs_are_rereadable_in_stable_order(&driver, &driver, &root).await;
}

#[tokio::test(start_paused = true)]
async fn fake_invariants() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    let probe = driver.clone();
    invariant_violations_create_nothing(&driver, &root, move || probe.container_count()).await;
}

/// `stop(grace)` 先 SIGTERM，宽限后 SIGKILL（假时钟精确断言）。
#[tokio::test(start_paused = true)]
async fn fake_stop_escalates_after_grace() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    let (mut spec, layout) = spec_for(&root, 6);
    driver.arrange(&mut spec, 6, Kind::RunIgnoreTerm);
    driver.prepare_dirs(&layout).await.unwrap();
    let id = driver.create(&spec).await.unwrap();
    driver.start(&id).await.unwrap();
    tokio::time::sleep(Duration::from_secs(30)).await;
    driver.stop(&id, Duration::from_secs(10)).await.unwrap();
    let signals = driver.signals(&id);
    assert_eq!(signals.len(), 2);
    assert_eq!(
        (signals[0].signal, signals[0].at),
        (SIGTERM, Duration::from_secs(30))
    );
    assert_eq!(
        (signals[1].signal, signals[1].at),
        (SIGKILL, Duration::from_secs(40))
    );
    let exit = driver.wait(&id).await.unwrap();
    assert_eq!(exit.signal, Some(SIGKILL));
    assert_eq!(
        driver.inspect(&id).await.unwrap().status,
        ContainerStatus::Exited
    );
    assert!(driver
        .calls()
        .iter()
        .any(|c| matches!(c, DriverCall::Stop { grace, .. } if *grace == Duration::from_secs(10))));
}

/// 先停后取：运行中 `read_out` 被拒。
#[tokio::test(start_paused = true)]
async fn fake_read_out_refused_while_running() {
    let root = TempRoot::new();
    let driver = FakeDriver::new(root.path());
    let (mut spec, layout) = spec_for(&root, 7);
    driver.arrange(&mut spec, 7, Kind::RunHonorTerm);
    driver.prepare_dirs(&layout).await.unwrap();
    let id = driver.create(&spec).await.unwrap();
    driver.start(&id).await.unwrap();
    let plan = common::plan();
    assert_eq!(
        driver.read_out(&id, &layout, &plan).await,
        Err(gantry_sandbox::driver::DriverError::StillRunning)
    );
    driver.stop(&id, Duration::from_secs(1)).await.unwrap();
    assert!(driver.read_out(&id, &layout, &plan).await.is_ok());
}

// ---------------------------------------------------------------------------
// PodmanDriver（B 级：需要 Linux）
//
// 运行方式（B 级环境）：
//   GANTRY_PODMAN_SOCKET=$XDG_RUNTIME_DIR/podman/podman.sock \
//   GANTRY_TEST_IMAGE=docker.io/library/busybox@sha256:… \
//   GANTRY_TEST_NETWORK=gantry-sbx-test \
//   cargo test -p gantry-sandbox --all-features --test driver_conformance -- --ignored
// 未设置时直接失败，绝不静默通过。
// ---------------------------------------------------------------------------

const NEEDS_LINUX: &str = "需要 Linux + rootless Podman（B 级，M1.4）";

struct PodmanScenario {
    driver: PodmanDriver,
    image: ImageDigest,
    network: String,
}

impl PodmanScenario {
    fn from_env(root: &TempRoot) -> Self {
        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("{name} 未设置：{NEEDS_LINUX}"))
        };
        let driver = PodmanDriver::new(PodmanConfig {
            podman_bin: "/usr/bin/podman".into(),
            socket: var("GANTRY_PODMAN_SOCKET").into(),
            workspaces_dir: root.path().to_path_buf(),
            runtime: Runtime::Runc,
            out_read: OutReadMode::HostDirect,
            call_timeout: Duration::from_secs(60),
            max_output: 1 << 20,
            caps: FakeDriver::new(root.path()).capabilities(),
        });
        Self {
            driver,
            image: ImageDigest::parse(&var("GANTRY_TEST_IMAGE")).expect("image must be pinned"),
            network: var("GANTRY_TEST_NETWORK"),
        }
    }
}

impl Scenario for PodmanScenario {
    fn arrange(&self, spec: &mut ContainerSpec, _sandbox: u128, kind: Kind) {
        let script = match kind {
            Kind::RunHonorTerm => "trap 'exit 143' TERM; while :; do sleep 1; done",
            Kind::RunIgnoreTerm => "trap '' TERM; while :; do sleep 1; done",
            Kind::ThreeLines => "sleep 1; echo one; sleep 1; echo two >&2; sleep 1; echo three",
        };
        spec.image = self.image.clone();
        spec.network.network = self.network.clone();
        spec.argv = vec!["sh".into(), "-c".into(), script.into()];
    }
}

#[tokio::test]
#[ignore = "需要 Linux + rootless Podman（B 级，M1.4）"]
async fn podman_conformance_suite() {
    let root = TempRoot::new();
    let scenario = PodmanScenario::from_env(&root);
    let driver = &scenario.driver;
    create_does_not_start_and_start_runs(driver, &scenario, &root).await;
    remove_is_idempotent_and_list_selects_by_label(driver, &scenario, &root).await;
    logs_are_rereadable_in_stable_order(driver, &scenario, &root).await;
    invariant_violations_create_nothing(driver, &root, || 0).await;
}
