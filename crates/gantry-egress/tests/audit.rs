//! 审计测试：`docs/modules/egress.md` §12.H（记录形状、不含头值 / 体 / 查询串、写失败可见）
//! 与 §7.8（按日轮转、`seq` 跨重启单调、文件权限）。

use std::sync::Arc;

use gantry_core::entities::{Tier, TokenUsage};
use gantry_core::ids::AttemptId;
use gantry_core::testing::FakeClock;
use gantry_egress::audit::{
    AuditCaller, AuditEvent, AuditKind, AuditSink, Decision, JsonlAudit, MemoryAudit,
};
use serde_json::Value;

const T0: i64 = 1_793_000_000_000; // 2026-10-26T…Z

fn job() -> AuditCaller {
    AuditCaller::Job {
        job_id: AttemptId::from_parts(T0 as u64, 1).unwrap(),
        tier: Tier::Worker,
    }
}

fn model_event() -> AuditEvent {
    let mut event = AuditEvent::new(AuditKind::Model, job(), Decision::Allow);
    event.method = Some("POST".into());
    event.host = Some("api.anthropic.com".into());
    event.port = Some(443);
    event.path = Some("/v1/messages?key=sk-gantry-sentinel-1#frag".into());
    event.status = Some(200);
    event.bytes_in = 18211;
    event.bytes_out = 4102;
    event.duration_ms = 2310;
    event.provider = Some("anthropic".into());
    event.usage = Some(TokenUsage::new(3201, 410, 2800, 0, 1).into());
    event.src = Some("10.90.0.5".into());
    event
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("gantry-egress-audit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn records_have_the_documented_shape_without_query_strings() {
    let audit = MemoryAudit::new();
    audit.record(model_event()).unwrap();
    let mut denied = AuditEvent::new(
        AuditKind::Connect,
        AuditCaller::Unknown {
            src: "10.90.0.7".into(),
        },
        Decision::Deny,
    );
    denied.rule = Some("token.unknown");
    denied.host = Some("github.com\u{7}\r\n".into());
    denied.port = Some(443);
    audit.record(denied).unwrap();

    let events = audit.events();
    let first = &events[0];
    assert_eq!(first["seq"], 1);
    assert_eq!(first["kind"], "model");
    assert_eq!(first["caller"]["type"], "job");
    assert_eq!(first["caller"]["tier"], "worker");
    assert_eq!(first["decision"], "allow");
    assert_eq!(first["rule"], Value::Null);
    assert_eq!(first["path"], "/v1/messages", "no query string or fragment");
    assert_eq!(first["usage"]["cache_read_tokens"], 2800);
    assert_eq!(first["prev_hash"], Value::Null);
    let keys: Vec<&str> = first
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "seq",
            "ts",
            "kind",
            "caller",
            "decision",
            "rule",
            "method",
            "host",
            "port",
            "path",
            "status",
            "bytes_in",
            "bytes_out",
            "duration_ms",
            "provider",
            "usage",
            "src",
            "resolved_ip",
            "prev_hash",
            "error"
        ]
    );

    let second = &events[1];
    assert_eq!(second["seq"], 2);
    assert_eq!(
        second["caller"],
        serde_json::json!({"type": "unknown", "src": "10.90.0.7"})
    );
    assert_eq!(second["rule"], "token.unknown");
    assert_eq!(
        second["host"], "github.com",
        "control characters are stripped"
    );
    assert!(!audit.lines().concat().contains("sk-gantry-sentinel"));
}

#[test]
fn injected_write_failures_surface_as_errors() {
    let audit = MemoryAudit::new();
    audit.fail_writes(true);
    assert!(audit.record(model_event()).is_err());
    audit.fail_writes(false);
    audit.record(model_event()).unwrap();
    assert_eq!(
        audit.events()[0]["seq"],
        1,
        "a failed write consumes no seq"
    );
}

#[test]
fn jsonl_files_rotate_daily_and_seq_survives_reopen() {
    let dir = temp_dir("rotate");
    let clock = Arc::new(FakeClock::new(T0));
    let audit = JsonlAudit::open(&dir, clock.clone()).unwrap();
    audit.record(model_event()).unwrap();
    audit.record(model_event()).unwrap();
    clock.advance_secs(86_400);
    audit.record(model_event()).unwrap();
    drop(audit);

    let day1 = clock.now_timestamp().to_date();
    let day0 = gantry_core::time::Timestamp::from_millis(T0).to_date();
    let read = |day: gantry_core::time::Date| -> Vec<Value> {
        std::fs::read_to_string(dir.join(format!("{day}.jsonl")))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    assert_eq!(read(day0).len(), 2);
    assert_eq!(read(day1)[0]["seq"], 3);

    let reopened = JsonlAudit::open(&dir, clock.clone()).unwrap();
    reopened.record(model_event()).unwrap();
    assert_eq!(read(day1)[1]["seq"], 4, "seq is monotonic across restarts");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = std::fs::metadata(dir.join(format!("{day1}.jsonl")))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unwritable_audit_directories_fail_instead_of_dropping_records() {
    let dir = temp_dir("unwritable");
    let clock = Arc::new(FakeClock::new(T0));
    let audit = JsonlAudit::open(&dir, clock).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();
    assert!(audit.record(model_event()).is_err());
    let _ = std::fs::remove_file(&dir);
}
