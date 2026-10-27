//! 测试共用：临时目录、夹具与 schema 校验。
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::canon::sha256;
use gantry_core::ports::worker::ImageRef;
use gantry_worker::ClaudeCodeAdapter;
use serde_json::Value;

pub const JOB_SPEC: &str =
    include_str!("../../../gantry-core/fixtures/protocol/worker/job_spec.json");
pub const RESULT: &str = include_str!("../../../gantry-core/fixtures/protocol/worker/result.json");
pub const PROPOSALS: &str =
    include_str!("../../../gantry-core/fixtures/protocol/worker/proposals.json");

pub fn adapter() -> ClaudeCodeAdapter {
    ClaudeCodeAdapter::new(ImageRef {
        reference: "registry.example/gantry/gantry-worker".into(),
        digest: sha256(b"worker-image"),
    })
}

/// 进程内唯一、测试结束删除的临时目录。
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("gantry-worker-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn schema(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/worker")
        .join(name);
    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
}

/// 用 `schemas/worker/<name>` 校验，失败时列出全部错误。
pub fn assert_valid(schema_name: &str, instance: &Value) {
    let schema = schema(schema_name);
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{schema_name}: {errors:#?}");
}
