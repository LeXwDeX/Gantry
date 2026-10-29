//! deploy.md §7.14 的报告约定与 M1 子集（A 级）。

use gantry_deploy::doctor::{self, DoctorContext, Status};
use gantry_deploy::manifest::Entry;
use gantry_deploy::probe::{HttpProbe, Method};

/// 一个把 API 路径也转发出去的入口：/v1/me 返回 401（到达了 gantryd 的鉴权）。
struct LeakyEntry;

impl HttpProbe for LeakyEntry {
    fn status(&self, method: Method, url: &str) -> Result<u16, String> {
        Ok(match (method, url.rsplit_once(".net").unwrap().1) {
            (Method::Post, "/webhook") => 401,
            (Method::Get, "/v1/me") => 401,
            _ => 404,
        })
    }
}

struct GoodEntry;

impl HttpProbe for GoodEntry {
    fn status(&self, method: Method, url: &str) -> Result<u16, String> {
        Ok(match (method, url.rsplit_once(".net").unwrap().1) {
            (Method::Post, "/webhook") => 401,
            _ => 404,
        })
    }
}

fn entry() -> Option<Entry> {
    Some(Entry::parse("https://hooks.example.net/webhook").unwrap())
}

#[test]
fn exit_codes_follow_worst_status() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: Some(false),
        probe: &GoodEntry,
    };
    let webhook = doctor::run(Some("webhook"), &ctx).unwrap();
    assert_eq!(webhook.worst(), Status::Ok, "{}", webhook.render_text());
    assert_eq!(webhook.exit_code(), 0);

    // 未实现的区域报 unchecked，不冒充通过。
    let pki = doctor::run(Some("pki"), &ctx).unwrap();
    assert_eq!(pki.exit_code(), 3);

    let leaky = DoctorContext {
        probe: &LeakyEntry,
        ..ctx
    };
    let report = doctor::run(Some("webhook"), &leaky).unwrap();
    assert_eq!(report.exit_code(), 2);
    let failed: Vec<&str> = report
        .checks
        .iter()
        .filter(|c| c.status == Status::Fail)
        .map(|c| c.name)
        .collect();
    assert_eq!(failed, ["api_not_exposed"]);
}

#[test]
fn unchecked_is_distinct_from_warn() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: Some(false),
        probe: &GoodEntry,
    };
    let pki = doctor::run(Some("pki"), &ctx).unwrap();
    assert_eq!(pki.checks[0].status, Status::Unchecked);
    assert!(pki.checks[0].detail.contains("#106"));
    let json = pki.to_json();
    assert_eq!(json["status"], "unchecked");
    assert_eq!(json["checks"][0]["status"], "unchecked");
    assert!(pki.render_text().starts_with("unchecked pki.pending"));
}

#[test]
fn fail_outranks_unchecked_in_worst_status() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: Some(false),
        probe: &GoodEntry,
    };
    let all = doctor::run(None, &ctx).unwrap();
    assert_eq!(all.worst(), Status::Unchecked);
    assert_eq!(all.exit_code(), 3);

    let failing = DoctorContext {
        test_mode: Some(true),
        ..ctx
    };
    let all = doctor::run(None, &failing).unwrap();
    assert_eq!(all.worst(), Status::Fail);
    assert_eq!(all.exit_code(), 2);
}

#[test]
fn test_mode_install_is_red_in_host_area() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: Some(true),
        probe: &GoodEntry,
    };
    let report = doctor::run(Some("host"), &ctx).unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.checks[0].name, "test_mode");
}

#[test]
fn unreadable_install_state_is_red_in_host_area() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: None,
        probe: &GoodEntry,
    };
    let report = doctor::run(Some("host"), &ctx).unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.checks[0].status, Status::Fail);
    assert_eq!(report.checks[0].name, "test_mode");
    assert!(report.checks[0].detail.contains("install-state.json"));
}

#[test]
fn missing_webhook_url_fails_and_unknown_area_is_rejected() {
    let ctx = DoctorContext {
        entry: None,
        test_mode: Some(false),
        probe: &GoodEntry,
    };
    assert_eq!(doctor::run(Some("webhook"), &ctx).unwrap().exit_code(), 2);
    assert!(doctor::run(Some("nope"), &ctx).is_err());
    // 全量：每个区域至少一条。
    let all = doctor::run(None, &ctx).unwrap();
    for area in doctor::AREAS {
        assert!(all.checks.iter().any(|c| c.area == *area), "{area}");
    }
}

#[test]
fn json_shape() {
    let ctx = DoctorContext {
        entry: entry(),
        test_mode: Some(false),
        probe: &GoodEntry,
    };
    let json = doctor::run(Some("webhook"), &ctx).unwrap().to_json();
    assert_eq!(json["status"], "ok");
    let first = &json["checks"][0];
    assert_eq!(first["area"], "webhook");
    assert_eq!(first["name"], "webhook_reaches_forge");
    assert_eq!(first["status"], "ok");
}

#[test]
fn argument_parsing() {
    let parse = |v: &[&str]| doctor::DoctorArgs::parse(v.iter().map(|s| s.to_string()));
    let args = parse(&["webhook", "--json"]).unwrap();
    assert_eq!(args.area.as_deref(), Some("webhook"));
    assert!(args.json);
    assert_eq!(parse(&[]).unwrap(), doctor::DoctorArgs::default());
    assert!(parse(&["--quick"]).is_err());
    assert!(parse(&["host", "pki"]).is_err());
}
