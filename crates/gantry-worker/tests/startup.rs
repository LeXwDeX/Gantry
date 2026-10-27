//! worker.md §12.A：启动校验（A1，M1）。`base_sha` 与工作区 HEAD、`READY` 等待在执行者 PR 中覆盖。

mod common;

use std::fs;

use common::{adapter, TempDir, JOB_SPEC};
use gantry_core::canon::sha256;
use gantry_worker::startup::{load_job, read_job, verify_manifest, BadJob, Paths};
use serde_json::{json, Value};

const SCHEMA_BYTES: &[u8] = include_bytes!("../../../schemas/worker/output.build.schema.json");
const MATERIAL: &[u8] =
    b"<<<GANTRY-UNTRUSTED-BEGIN ...>>>\nissue body\n<<<GANTRY-UNTRUSTED-END ...>>>\n";

/// 夹具 JobSpec，`structured_schema.sha256` 换成真实 schema 文件的哈希。
fn job_value() -> Value {
    let mut job: Value = serde_json::from_str(JOB_SPEC).unwrap();
    job["output"]["structured_schema"]["sha256"] =
        serde_json::to_value(sha256(SCHEMA_BYTES)).unwrap();
    job
}

struct Staged {
    _dir: TempDir,
    paths: Paths,
}

/// 按 sandbox 的 staging 约定摆好 `job.json`、`inputs/` 与 MANIFEST。
fn stage(job_bytes: &[u8]) -> Staged {
    let dir = TempDir::new("startup");
    let paths = Paths {
        job: dir.path().join("job.json"),
        inputs: dir.path().join("inputs"),
        work: dir.path().join("work"),
        out: dir.path().join("out"),
    };
    fs::create_dir_all(paths.inputs.join("materials")).unwrap();
    fs::write(&paths.job, job_bytes).unwrap();
    fs::write(paths.inputs.join("output.schema.json"), SCHEMA_BYTES).unwrap();
    fs::write(paths.inputs.join("materials/mat-001.txt"), MATERIAL).unwrap();
    let manifest = json!({
        "job_sha256": sha256(job_bytes),
        "files": [
            {"path": "/gantry/inputs/materials/mat-001.txt", "sha256": sha256(MATERIAL), "bytes": MATERIAL.len()},
            {"path": "/gantry/inputs/output.schema.json", "sha256": sha256(SCHEMA_BYTES), "bytes": SCHEMA_BYTES.len()},
        ],
    });
    write_manifest(&paths, &manifest);
    Staged { _dir: dir, paths }
}

fn write_manifest(paths: &Paths, manifest: &Value) {
    fs::write(
        paths.inputs.join("MANIFEST.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
}

fn read_manifest(paths: &Paths) -> Value {
    serde_json::from_slice(&fs::read(paths.inputs.join("MANIFEST.json")).unwrap()).unwrap()
}

fn start(paths: &Paths) -> Result<(), BadJob> {
    let loaded = read_job(paths, &adapter())?;
    verify_manifest(paths, &loaded)
}

fn bytes(job: &Value) -> Vec<u8> {
    serde_json::to_vec(job).unwrap()
}

#[test]
fn staged_fixture_passes_and_hash_is_of_raw_bytes() {
    let job_bytes = bytes(&job_value());
    let staged = stage(&job_bytes);
    let loaded = read_job(&staged.paths, &adapter()).unwrap();
    assert_eq!(loaded.job_sha256, sha256(&job_bytes));
    verify_manifest(&staged.paths, &loaded).unwrap();
}

#[test]
fn top_level_unknown_fields_are_ignored() {
    let mut job = job_value();
    job["x_future_field"] = json!({"anything": [1, 2, 3]});
    job["task"]["x_hint"] = json!("ignored");
    let staged = stage(&bytes(&job));
    start(&staged.paths).unwrap();
}

type Check = fn(&BadJob) -> bool;
type Mutation = Box<dyn Fn(&mut Value)>;

/// 每一项只改一处，都必须是 BAD_JOB，且分到预期的原因。
#[test]
fn bad_job_table() {
    let job_mutations: Vec<(&str, Mutation, Check)> = vec![
        (
            "protocol major 2",
            Box::new(|j| j["protocol"] = json!("gantry-worker/2")),
            |e| matches!(e, BadJob::UnsupportedProtocol),
        ),
        (
            "protocol missing",
            Box::new(|j| {
                j.as_object_mut().unwrap().remove("protocol");
            }),
            |e| matches!(e, BadJob::UnsupportedProtocol),
        ),
        (
            "agent mismatch",
            Box::new(|j| j["execution"]["agent"] = json!("codex")),
            |e| matches!(e, BadJob::AgentMismatch { .. }),
        ),
        (
            "unknown constraints.tools field",
            Box::new(|j| j["constraints"]["tools"]["network"] = json!(true)),
            |e| matches!(e, BadJob::InvalidSpec(_)),
        ),
        (
            "unknown constraints field",
            Box::new(|j| j["constraints"]["gpu"] = json!(1)),
            |e| matches!(e, BadJob::InvalidSpec(_)),
        ),
        (
            "unknown output field",
            Box::new(|j| j["output"]["artifacts_dir"] = json!("/tmp")),
            |e| matches!(e, BadJob::InvalidSpec(_)),
        ),
        (
            "unknown agent_options key",
            Box::new(|j| j["execution"]["agent_options"] = json!({"permission_mode": "bypass"})),
            |e| matches!(e, BadJob::AgentOptions(_)),
        ),
        (
            "sandbox_ro with patch",
            Box::new(|j| {
                j["node"]["role"] = json!("review:security");
                j["node"]["exec"] = json!("sandbox_ro");
                j["constraints"]["tools"]["edit"] = json!(false);
                j["output"]["patch"] = json!("optional");
            }),
            |e| matches!(e, BadJob::InvalidSpec(_)),
        ),
        (
            "output.schema.json hash differs from job",
            Box::new(|j| {
                j["output"]["structured_schema"]["sha256"] =
                    serde_json::to_value(sha256(b"another schema")).unwrap()
            }),
            |e| matches!(e, BadJob::Manifest(_)),
        ),
    ];
    for (name, mutate, check) in job_mutations {
        let mut job = job_value();
        mutate(&mut job);
        let staged = stage(&bytes(&job));
        let err = start(&staged.paths).expect_err(name);
        assert!(check(&err), "{name}: {err:?}");
    }
}

#[test]
fn bad_job_raw_bytes() {
    let cases: [(&str, &[u8], Check); 4] = [
        ("not json", b"protocol: gantry-worker/1", |e| {
            matches!(e, BadJob::NotJson(_))
        }),
        ("not utf-8", b"{\"protocol\": \"\xff\"}", |e| {
            matches!(e, BadJob::NotUtf8)
        }),
        (
            "duplicate keys",
            b"{\"protocol\":\"gantry-worker/1\",\"protocol\":\"x\"}",
            |e| matches!(e, BadJob::NotJson(_)),
        ),
        ("utf-8 bom", b"\xEF\xBB\xBF{}", |e| {
            matches!(e, BadJob::NotJson(_))
        }),
    ];
    for (name, raw, check) in cases {
        let err = load_job(raw, &adapter()).expect_err(name);
        assert!(check(&err), "{name}: {err:?}");
    }
}

#[test]
fn missing_job_json() {
    let staged = stage(&bytes(&job_value()));
    fs::remove_file(&staged.paths.job).unwrap();
    assert_eq!(start(&staged.paths), Err(BadJob::Missing));
}

#[test]
fn manifest_job_sha256_mismatch() {
    let job_bytes = bytes(&job_value());
    let staged = stage(&job_bytes);
    // 语义相同但字节不同：哈希针对原始字节。
    let mut other = job_bytes.clone();
    other.push(b'\n');
    fs::write(&staged.paths.job, other).unwrap();
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
}

#[test]
fn manifest_tampered_file() {
    let staged = stage(&bytes(&job_value()));
    fs::write(
        staged.paths.inputs.join("materials/mat-001.txt"),
        b"tampered",
    )
    .unwrap();
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
}

#[test]
fn manifest_listed_file_missing() {
    let staged = stage(&bytes(&job_value()));
    fs::remove_file(staged.paths.inputs.join("materials/mat-001.txt")).unwrap();
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
}

#[test]
fn manifest_rejects_paths_outside_inputs() {
    for path in [
        "/gantry/job.json",
        "/gantry/work/src/lib.rs",
        "/gantry/inputs/../job.json",
        "/gantry/inputs/./output.schema.json",
        "/gantry/inputs",
        "/gantry/inputsx/a",
    ] {
        let staged = stage(&bytes(&job_value()));
        let mut manifest = read_manifest(&staged.paths);
        manifest["files"]
            .as_array_mut()
            .unwrap()
            .push(json!({"path": path, "sha256": sha256(b""), "bytes": 0}));
        write_manifest(&staged.paths, &manifest);
        assert!(
            matches!(start(&staged.paths), Err(BadJob::Manifest(_))),
            "{path}"
        );
    }
}

#[test]
fn manifest_without_output_schema_entry() {
    let staged = stage(&bytes(&job_value()));
    let mut manifest = read_manifest(&staged.paths);
    manifest["files"].as_array_mut().unwrap().pop();
    write_manifest(&staged.paths, &manifest);
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
}

#[test]
fn manifest_missing_or_malformed() {
    let staged = stage(&bytes(&job_value()));
    fs::write(
        staged.paths.inputs.join("MANIFEST.json"),
        b"{\"job_sha256\": 1}",
    )
    .unwrap();
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
    fs::remove_file(staged.paths.inputs.join("MANIFEST.json")).unwrap();
    assert!(matches!(start(&staged.paths), Err(BadJob::Manifest(_))));
}

#[test]
fn bad_job_messages_do_not_echo_file_content() {
    let canary = "CANARY-7f3e";
    let mut job = job_value();
    job["execution"]["agent_options"] = json!({ "x": canary });
    let staged = stage(&bytes(&job));
    let err = start(&staged.paths).unwrap_err();
    assert!(!err.to_string().contains(canary), "{err}");
    let err = load_job(
        format!("{{\"protocol\": \"{canary}\"").as_bytes(),
        &adapter(),
    )
    .unwrap_err();
    assert!(!err.to_string().contains(canary), "{err}");
}
