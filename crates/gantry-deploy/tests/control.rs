//! 控制通道客户端与 gantryd 行协议的往返（#101 的最小方案）。
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

use gantry_deploy::control::{parse_response, ControlClient, ControlError, UnixControlClient};
use serde_json::{json, Value};

#[test]
fn one_request_line_one_response_line() {
    let dir = std::env::temp_dir().join(format!("gantry-deploy-ctl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
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

    let client = UnixControlClient::new(&path);
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
    let dir = std::env::temp_dir().join(format!(
        "gantry-deploy-ctl-forbidden-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
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
    let err = UnixControlClient::new(&path)
        .call("status", Value::Null)
        .unwrap_err();
    server.join().unwrap();
    assert!(
        matches!(err, ControlError::Rejected { ref code, .. } if code == "forbidden"),
        "{err:?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unavailable_and_malformed() {
    let client = UnixControlClient::new("/nonexistent/gantry/control.sock");
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
