//! `schemas/worker/` 的夹具往返：worker 读入的 JobSpec 与写出的产物都必须通过 schema，
//! 且解码后与原值相等（worker-protocol §11.1）。

mod common;

use std::fs;

use common::{adapter, assert_valid, TempDir, JOB_SPEC, PROPOSALS, RESULT};
use gantry_core::canon::sha256;
use gantry_core::worker::{Proposals, ResultJson, ResultStatus};
use gantry_worker::artifacts::{write_proposals, write_result, ArtifactError};
use gantry_worker::startup::load_job;
use serde_json::Value;

fn out_dir() -> TempDir {
    TempDir::new("out")
}

fn read_json(path: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn job_spec_fixture_loads_and_reencodes_within_schema() {
    let raw: Value = serde_json::from_str(JOB_SPEC).unwrap();
    assert_valid("job.schema.json", &raw);
    let loaded = load_job(JOB_SPEC.as_bytes(), &adapter()).unwrap();
    assert_eq!(loaded.job_sha256, sha256(JOB_SPEC.as_bytes()));
    let reencoded = serde_json::to_value(&loaded.job).unwrap();
    assert_valid("job.schema.json", &reencoded);
    assert_eq!(
        serde_json::from_value::<gantry_core::worker::JobSpec>(reencoded).unwrap(),
        loaded.job
    );
}

#[test]
fn result_written_by_worker_roundtrips() {
    let result: ResultJson = serde_json::from_str(RESULT).unwrap();
    let out = out_dir();
    let reference = write_result(out.path(), &result).unwrap();
    let path = out.path().join("result.json");
    let bytes = fs::read(&path).unwrap();
    assert_eq!(reference.path, "result.json");
    assert_eq!(reference.sha256, sha256(&bytes));
    assert_eq!(reference.bytes, bytes.len() as u64);
    // 原子写之后不残留 `.tmp`。
    assert_eq!(listing(out.path()), ["result.json"]);
    let value = read_json(&path);
    assert_valid("result.schema.json", &value);
    assert_eq!(serde_json::from_value::<ResultJson>(value).unwrap(), result);
}

#[test]
fn result_must_be_locally_consistent_before_writing() {
    let mut result: ResultJson = serde_json::from_str(RESULT).unwrap();
    result.status = ResultStatus::Failed;
    result.error = None;
    let out = out_dir();
    assert!(matches!(
        write_result(out.path(), &result),
        Err(ArtifactError::Invalid("result.json", _))
    ));
    assert!(listing(out.path()).is_empty());
}

#[test]
fn proposals_written_by_worker_roundtrip() {
    let proposals: Proposals = serde_json::from_str(PROPOSALS).unwrap();
    let out = out_dir();
    let reference = write_proposals(out.path(), &proposals).unwrap();
    let path = out.path().join("proposals.json");
    assert_eq!(reference.sha256, sha256(&fs::read(&path).unwrap()));
    assert_eq!(listing(out.path()), ["proposals.json"]);
    let value = read_json(&path);
    assert_valid("proposals.schema.json", &value);
    assert_eq!(
        serde_json::from_value::<Proposals>(value).unwrap(),
        proposals
    );
}

#[test]
fn result_fixture_artifact_paths_are_out_relative() {
    let result: ResultJson = serde_json::from_str(RESULT).unwrap();
    let artifacts = &result.artifacts;
    for path in [
        artifacts.patch.as_ref().map(|p| p.path.as_str()),
        artifacts.report.as_ref().map(|r| r.path.as_str()),
        artifacts.proposals.as_ref().map(|r| r.path.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        // 与 worker 写出的引用同形：相对 `out/` 的单层文件名。
        assert!(
            ["changes.patch", "report.md", "proposals.json"].contains(&path),
            "{path}"
        );
    }
}
