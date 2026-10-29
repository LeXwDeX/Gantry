//! 控制通道客户端与 gantryd 行协议的往返（#101 的最小方案；#368 的对端 uid 校验）。
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;

use gantry_deploy::control::{parse_response, ControlClient, ControlError, UnixControlClient};
use serde_json::{json, Value};

fn current_uid() -> u32 {
    let out = std::process::Command::new("id").arg("-u").output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn client(path: &Path) -> UnixControlClient {
    let mut client = UnixControlClient::new(path);
    client.expected_uid = Some(current_uid());
    client
}

fn sock_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gantry-deploy-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn one_request_line_one_response_line() {
    let dir = sock_dir("ctl");
    let path = dir.join("control.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for reply in [
            json!({ "ok": true, "result": { "state": "s" } }),
            json!({ "ok": false, "error": { "code": "not_bootstrap_period", "message": "m" } }),
        ] {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            seen.push(serde_json::from_str::<Value>(&line).unwrap());
            let mut out = reply.to_string();
            out.push('\n');
            (&stream).write_all(out.as_bytes()).unwrap();
        }
        seen
    });

    let client = client(&path);
    let ok = client
        .call("github.setup.begin", json!({ "ttl_secs": 900 }))
        .unwrap();
    assert_eq!(ok, json!({ "state": "s" }));
    let err = client
        .call("github.setup.status", json!({ "state": "s" }))
        .unwrap_err();
    assert_eq!(
        err,
        ControlError::Rejected {
            code: "not_bootstrap_period".into(),
            message: "m".into()
        }
    );
    let seen = server.join().unwrap();
    assert_eq!(
        seen[0],
        json!({ "cmd": "github.setup.begin", "args": { "ttl_secs": 900 } })
    );
    assert_eq!(seen[1]["cmd"], "github.setup.status");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn forbidden_line_sent_on_connect_is_reported() {
    let dir = sock_dir("ctl-forbidden");
    let path = dir.join("control.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        (&stream)
            .write_all(
                b"{\"ok\":false,\"error\":{\"code\":\"forbidden\",\"message\":\"root only\"}}\n",
            )
            .unwrap();
        drop(stream);
    });
    let err = client(&path).call("status", Value::Null).unwrap_err();
    server.join().unwrap();
    assert!(
        matches!(err, ControlError::Rejected { ref code, .. } if code == "forbidden"),
        "{err:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rejects_peer_with_other_uid_before_sending() {
    let dir = sock_dir("ctl-peer");
    let path = dir.join("control.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        // 客户端应在发请求前断开：读到 EOF，而不是一行 JSON。
        BufReader::new(&stream).read_line(&mut line).unwrap();
        line
    });

    let mut bad = UnixControlClient::new(&path);
    bad.expected_uid = Some(current_uid() + 1);
    let err = bad.call("status", Value::Null).unwrap_err();
    assert!(
        matches!(err, ControlError::PeerUid { actual, .. } if actual == current_uid()),
        "{err:?}"
    );
    assert_eq!(server.join().unwrap(), "");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn rejects_unresolvable_user() {
    let dir = sock_dir("ctl-unknown");
    let mut client = UnixControlClient::new(dir.join("control.sock"));
    client.user = "gantry-test-user-that-does-not-exist".into();
    assert_eq!(
        client.call("status", Value::Null).unwrap_err(),
        ControlError::UnknownUser {
            user: "gantry-test-user-that-does-not-exist".into()
        }
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn resolves_known_user_and_accepts_matching_uid() {
    let dir = sock_dir("ctl-resolve");
    let path = dir.join("control.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        (&stream)
            .write_all(b"{\"ok\":true,\"result\":null}\n")
            .unwrap();
    });

    let mut client = UnixControlClient::new(&path);
    // 本机当前用户的名称一定能被 getpwnam 解析，且对端 uid 相同。
    let name = std::process::Command::new("id")
        .arg("-un")
        .output()
        .unwrap();
    client.user = String::from_utf8_lossy(&name.stdout).trim().to_owned();
    assert_eq!(client.call("status", Value::Null).unwrap(), Value::Null);
    server.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unavailable_and_malformed() {
    let client = client(Path::new("/nonexistent/gantry/control.sock"));
    assert!(matches!(
        client.call("status", Value::Null),
        Err(ControlError::Unavailable { .. })
    ));
    assert!(matches!(
        parse_response("not json"),
        Err(ControlError::Protocol(_))
    ));
    assert!(matches!(
        parse_response("{}"),
        Err(ControlError::Protocol(_))
    ));
}
