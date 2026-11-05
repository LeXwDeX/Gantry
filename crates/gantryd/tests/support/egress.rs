//! 父测试进程持有的真实 egress：内存登记表、模型代理与控制面，控制套接字放进 scratch，
//! 子进程经 `EgressClient` 连它。

use std::path::{Path, PathBuf};

use gantry_egress::control::ControlServer;
use gantry_egress::testing::{Egress, Options};

/// 保持 runtime 与 egress 任务存活；drop 时随 runtime 一起停止。
pub struct TestEgress {
    runtime: Option<tokio::runtime::Runtime>,
    pub socket: PathBuf,
    _egress: Egress,
}

impl Drop for TestEgress {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // 测试可能是 `#[tokio::test]`；tokio 禁止在异步上下文直接 drop runtime。
            let _ = std::thread::spawn(move || drop(runtime)).join();
        }
    }
}

impl TestEgress {
    pub fn start(dir: &Path, uid: u32) -> Self {
        let socket = dir.join("egress.sock");
        let bind_path = socket.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        // 调用方可能已经在 tokio runtime 内（`#[tokio::test]`），嵌套 `block_on` 会 panic；
        // 在独立线程创建 runtime，再把 runtime 交回测试进程持有。
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("egress runtime");
            let socket_path = bind_path.clone();
            let egress = runtime.block_on(async {
                // rev 0 让装配里的 settings 视图（rev 1）真正替换它。
                let egress = Egress::start(Options {
                    provider_host: "api.anthropic.com",
                    config_rev: 0,
                    ..Options::default()
                })
                .await;
                let listener =
                    tokio::net::UnixListener::bind(&socket_path).expect("bind egress control");
                let registry = egress.registry.clone();
                let audit = egress.audit.clone();
                tokio::spawn(async move {
                    if let Err(error) = ControlServer::new(registry, audit, uid)
                        .serve(listener)
                        .await
                    {
                        eprintln!("test egress control: {error}");
                    }
                });
                egress
            });
            tx.send((runtime, egress)).expect("hand back egress");
        })
        .join()
        .expect("egress thread");
        let (runtime, egress) = rx.recv().expect("egress runtime");
        Self {
            runtime: Some(runtime),
            socket,
            _egress: egress,
        }
    }
}
