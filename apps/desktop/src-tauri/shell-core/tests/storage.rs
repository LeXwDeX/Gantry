//! 本机存储（desktop-shell.md §12"本机存储与日志"、"密钥与身份"中 identity.json 的 A 级部分）。

use std::path::PathBuf;

use gantry_desktop_shell::ipc::ShellErrorCode;
use gantry_desktop_shell::keys::KeyLabels;
use gantry_desktop_shell::storage::{
    AppStore, IdentityFile, Prefs, Theme, UI_STATE_MAX_BYTES_PER_PROJECT, UI_STATE_MAX_TOTAL_BYTES,
};
use serde_json::{json, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "gantry-shell-storage-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const PRJ_A: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const PRJ_B: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZB";

#[test]
fn prefs_default_patch_and_restart() {
    let tmp = TempDir::new("prefs");
    let store = AppStore::open(&tmp.0).unwrap();
    assert_eq!(store.prefs(), Prefs::default());
    assert_eq!(store.prefs().theme, Theme::System);

    let next = store
        .set_prefs(&json!({
            "theme": "dark",
            "layout": {"leftWidth": 300, "inspectorWidth": 260, "leftCollapsed": true, "inspectorCollapsed": false}
        }))
        .unwrap();
    assert_eq!(next.theme, Theme::Dark);
    assert_eq!(next.layout.left_width, 300);

    // 重启后恢复。
    let store = AppStore::open(&tmp.0).unwrap();
    assert_eq!(store.prefs(), next);
    // 序列化形状与前端 Prefs 一致（camelCase）。
    let v = serde_json::to_value(store.prefs()).unwrap();
    assert_eq!(v["systemNotifications"], true);
    assert_eq!(v["layout"]["inspectorWidth"], 260);
}

#[test]
fn prefs_rejects_unknown_keys_bad_values_and_credentials() {
    let tmp = TempDir::new("prefs-reject");
    let store = AppStore::open(&tmp.0).unwrap();
    let cases: Vec<(Value, &str)> = vec![
        (json!({"api_key": "x"}), "patch.api_key"),
        (json!({"models": {"worker": {}}}), "patch.models"),
        (json!({"theme": "neon"}), "patch.theme"),
        (
            json!({"systemNotifications": "yes"}),
            "patch.systemNotifications",
        ),
        (
            json!({"language": "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}),
            "patch.language",
        ),
        (
            json!({"language": "ghp_0123456789abcdefABCDEF0123456789abcd"}),
            "patch.language",
        ),
        (
            json!({"layout": {"leftWidth": 10, "inspectorWidth": 300, "leftCollapsed": false, "inspectorCollapsed": false}}),
            "patch.layout.leftWidth",
        ),
        (
            json!({"layout": {"leftWidth": 240, "inspectorWidth": 300, "leftCollapsed": false, "inspectorCollapsed": false, "token": "x"}}),
            "patch.layout",
        ),
        (json!({"layout": {"leftWidth": 240}}), "patch.layout"),
        (json!(["theme"]), "patch"),
    ];
    for (patch, field) in cases {
        let err = store.set_prefs(&patch).unwrap_err();
        assert_eq!(err.code, ShellErrorCode::BadArgument, "{patch}");
        assert_eq!(err.field.as_deref(), Some(field), "{patch}");
    }
    // 被拒的补丁不落盘。
    assert_eq!(store.prefs(), Prefs::default());
}

#[test]
fn corrupt_prefs_fall_back_to_default() {
    let tmp = TempDir::new("prefs-corrupt");
    let store = AppStore::open(&tmp.0).unwrap();
    for text in [
        "{not json",
        r#"{"theme":"dark"}"#,
        r#"{"theme":"dark","density":"compact","language":"zh-CN","systemNotifications":true,"layout":{"leftWidth":9999,"inspectorWidth":300,"leftCollapsed":false,"inspectorCollapsed":false}}"#,
    ] {
        std::fs::write(tmp.0.join("prefs.json"), text).unwrap();
        assert_eq!(store.prefs(), Prefs::default(), "{text}");
    }
}

fn ui_state(draft: &str) -> Value {
    json!({"view": "chat", "sel": null, "draft": draft, "outbox": []})
}

#[test]
fn ui_state_roundtrip_list_restart_and_clear() {
    let tmp = TempDir::new("ui");
    let store = AppStore::open(&tmp.0).unwrap();
    assert_eq!(store.ui_state_get(PRJ_A).unwrap(), None);

    store.ui_state_set(PRJ_A, &ui_state("草稿 A")).unwrap();
    store.ui_state_set(PRJ_B, &ui_state("草稿 B")).unwrap();
    store.ui_state_set(PRJ_A, &ui_state("草稿 A2")).unwrap();

    let store = AppStore::open(&tmp.0).unwrap();
    assert_eq!(
        store.ui_state_get(PRJ_A).unwrap(),
        Some(ui_state("草稿 A2"))
    );
    let all = store.ui_state_list().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[PRJ_B], ui_state("草稿 B"));

    store.ui_state_clear().unwrap();
    assert!(store.ui_state_list().unwrap().is_empty());
}

#[test]
fn ui_state_rejects_bad_ids_shapes_and_oversize() {
    let tmp = TempDir::new("ui-reject");
    let store = AppStore::open(&tmp.0).unwrap();

    for id in ["../identity", "prj_x", "dlv_01JB7K2M9Q4X8V3N5R6T1W0YZA", ""] {
        let err = store.ui_state_set(id, &ui_state("x")).unwrap_err();
        assert_eq!(
            (err.code, err.field.as_deref()),
            (ShellErrorCode::BadArgument, Some("project_id")),
            "{id}"
        );
        assert!(store.ui_state_get(id).is_err());
    }
    for state in [json!("string"), json!([1, 2]), json!(null)] {
        let err = store.ui_state_set(PRJ_A, &state).unwrap_err();
        assert_eq!(err.code, ShellErrorCode::BadArgument, "{state}");
    }

    let big = "x".repeat(UI_STATE_MAX_BYTES_PER_PROJECT);
    let err = store.ui_state_set(PRJ_A, &ui_state(&big)).unwrap_err();
    assert_eq!(err.code, ShellErrorCode::TooLarge);
    assert_eq!(store.ui_state_get(PRJ_A).unwrap(), None);
}

#[test]
fn ui_state_total_limit() {
    let tmp = TempDir::new("ui-total");
    let store = AppStore::open(&tmp.0).unwrap();
    let chunk = "y".repeat(UI_STATE_MAX_BYTES_PER_PROJECT - 200);
    let per = UI_STATE_MAX_BYTES_PER_PROJECT - 100;
    let fits = UI_STATE_MAX_TOTAL_BYTES / per;
    let mut ids = Vec::new();
    for i in 0..fits {
        // 构造合法的不同项目 ID（Crockford base32 的末两位变化）。
        let alphabet = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let id = format!(
            "prj_01JB7K2M9Q4X8V3N5R6T1W0Y{}{}",
            alphabet[i / 32] as char,
            alphabet[i % 32] as char
        );
        store.ui_state_set(&id, &ui_state(&chunk)).unwrap();
        ids.push(id);
    }
    let err = store
        .ui_state_set("prj_01JB7K2M9Q4X8V3N5R6T1W0ZZZ", &ui_state(&chunk))
        .unwrap_err();
    assert_eq!(err.code, ShellErrorCode::TooLarge);
    // 覆盖已有项目不因"总量"误判（自身旧值不计入）。
    store.ui_state_set(&ids[0], &ui_state(&chunk)).unwrap();
}

#[test]
fn corrupt_ui_state_reads_as_none() {
    let tmp = TempDir::new("ui-corrupt");
    let store = AppStore::open(&tmp.0).unwrap();
    std::fs::write(
        tmp.0.join("ui-state").join(format!("{PRJ_A}.json")),
        "{oops",
    )
    .unwrap();
    std::fs::write(tmp.0.join("ui-state").join("not-a-project.json"), "{}").unwrap();
    assert_eq!(store.ui_state_get(PRJ_A).unwrap(), None);
    assert!(store.ui_state_list().unwrap().is_empty());
}

fn identity() -> IdentityFile {
    let labels = KeyLabels::random().unwrap();
    IdentityFile {
        api_base_url: "https://gantry.example.com:8443".into(),
        ca_fingerprint: format!("sha256:{}", "ab".repeat(32)),
        device_id: "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM".into(),
        device_name: "Susan 的 MacBook".into(),
        platform: "macos".into(),
        cert_chain_pem: vec!["leaf".into(), "intermediate".into(), "root".into()],
        tls_key_label: labels.tls,
        approval_key_label: labels.approval,
        paired_at: "2026-10-27T00:00:00.000Z".into(),
        renewed_at: None,
        log_epoch: Some(1),
        revoked: false,
    }
}

#[test]
fn identity_roundtrip_and_delete() {
    let tmp = TempDir::new("identity");
    let store = AppStore::open(&tmp.0).unwrap();
    assert_eq!(store.identity(), None);
    let id = identity();
    store.save_identity(&id).unwrap();
    assert_eq!(AppStore::open(&tmp.0).unwrap().identity(), Some(id.clone()));
    assert_eq!(store.identity().unwrap().key_labels(), id.key_labels());
    store.delete_identity().unwrap();
    store.delete_identity().unwrap();
    assert_eq!(store.identity(), None);
}

#[test]
fn corrupt_or_tampered_identity_is_unpaired() {
    let tmp = TempDir::new("identity-corrupt");
    let store = AppStore::open(&tmp.0).unwrap();
    let path = tmp.0.join("identity.json");

    std::fs::write(&path, "{broken").unwrap();
    assert_eq!(store.identity(), None);

    let tampered: [fn(&mut IdentityFile); 5] = [
        |i| i.api_base_url = "http://gantry.example.com".into(),
        |i| i.ca_fingerprint = "sha256:XYZ".into(),
        |i| i.device_id = "prj_1".into(),
        |i| {
            i.cert_chain_pem.pop();
        },
        |i| i.tls_key_label = "../../keys".into(),
    ];
    for tamper in tampered {
        let mut id = identity();
        tamper(&mut id);
        std::fs::write(&path, serde_json::to_vec(&id).unwrap()).unwrap();
        assert_eq!(store.identity(), None);
        assert!(store.save_identity(&id).is_err());
    }

    // 未知字段（例如有人塞进去的"allow_all"）也不被接受。
    let mut v = serde_json::to_value(identity()).unwrap();
    v["allow_all"] = json!(true);
    std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
    assert_eq!(store.identity(), None);
}

#[cfg(unix)]
#[test]
fn files_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new("perms");
    let store = AppStore::open(&tmp.0).unwrap();
    store.set_prefs(&json!({"theme": "light"})).unwrap();
    store.ui_state_set(PRJ_A, &ui_state("x")).unwrap();
    store.save_identity(&identity()).unwrap();
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(tmp.0.clone()), 0o700);
    assert_eq!(mode(tmp.0.join("ui-state")), 0o700);
    assert_eq!(mode(tmp.0.join("prefs.json")), 0o600);
    assert_eq!(mode(tmp.0.join("identity.json")), 0o600);
    assert_eq!(
        mode(tmp.0.join("ui-state").join(format!("{PRJ_A}.json"))),
        0o600
    );
    // 没有残留的临时文件。
    assert!(!tmp.0.join("prefs.json.tmp").exists());
}
