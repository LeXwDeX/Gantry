//! 契约一致性：同一套 HTTP 行为用例（`tools/mock-api/test/shared/behavior.mjs`）对
//! 真实 `gantry-api` 跑一遍；Mock API 一侧由 `npm test --prefix tools/mock-api` 运行
//! （`shared-mock.test.mjs`）。本文 §1.1 M0 行与 §12.1「Mock API 服务」。
//!
//! 真实 api 用真实 pki / policy / store 装配，经测试专用的明文监听器暴露，每个连接
//! 视为出示了一台已配对的桌面 approver 设备的证书；用例本身只用 Node 内置的 fetch。

mod support;

use std::path::PathBuf;
use std::process::Command;

use gantry_core::entities::DevicePlatform;
use support::pki_env::PkiEnv;
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_http_behaviour_passes_against_the_real_api() {
    let env = PkiEnv::new().await;
    let device = env.pair(DevicePlatform::Macos, "conformance").await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(gantry_api::listener::serve_plain_for_tests(
        env.core.clone(),
        listener,
        device.peer.clone(),
    ));

    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/mock-api/test/shared/real.mjs");
    let output = tokio::task::spawn_blocking(move || {
        Command::new("node")
            .args(["--test", "--test-reporter=spec"])
            .arg(script)
            .env("GANTRY_API_URL", url)
            .output()
    })
    .await
    .unwrap()
    .expect("node (>= 22) is required to run the shared conformance suite");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "shared behaviour failed against gantry-api:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("ℹ pass 7"), "{stdout}");
}
