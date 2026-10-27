//! deploy.md §12「引导期写入 (M1，P-66)」中 CLI 侧的部分；白名单与引导期判定在服务端。

use std::cell::RefCell;

use gantry_deploy::bootstrap_set::{self, BootstrapSetArgs, BootstrapSetError};
use gantry_deploy::control::{ControlClient, ControlError};
use serde_json::{json, Value};

struct Recorder {
    reply: Result<Value, ControlError>,
    calls: RefCell<Vec<(String, Value)>>,
}

impl ControlClient for Recorder {
    fn call(&self, cmd: &str, args: Value) -> Result<Value, ControlError> {
        self.calls.borrow_mut().push((cmd.to_owned(), args));
        self.reply.clone()
    }
}

fn parse(v: &[&str]) -> Result<BootstrapSetArgs, BootstrapSetError> {
    BootstrapSetArgs::parse(v.iter().map(|s| s.to_string()))
}

#[test]
fn forwards_values_verbatim_as_strings() {
    let image = "ghcr.io/acme/worker@sha256:0123abcd=x";
    let args = parse(&["sandbox", &format!("base_image={image}")]).unwrap();
    let control = Recorder {
        reply: Ok(json!({ "applied": true })),
        calls: RefCell::new(Vec::new()),
    };
    assert_eq!(
        bootstrap_set::run(&args, &control).unwrap(),
        json!({ "applied": true })
    );
    let calls = control.calls.borrow();
    assert_eq!(calls[0].0, "settings.bootstrap_set");
    assert_eq!(
        calls[0].1,
        json!({ "resource": "sandbox", "values": { "base_image": image } })
    );
}

#[test]
fn server_rejection_after_pairing_is_surfaced() {
    let control = Recorder {
        reply: Err(ControlError::Rejected {
            code: "not_bootstrap_period".into(),
            message: "已有设备配对".into(),
        }),
        calls: RefCell::new(Vec::new()),
    };
    let args = parse(&["sandbox", "base_image=x"]).unwrap();
    assert!(matches!(
        bootstrap_set::run(&args, &control),
        Err(BootstrapSetError::Control(ControlError::Rejected { code, .. })) if code == "not_bootstrap_period"
    ));
}

#[test]
fn argument_validation() {
    for bad in [
        &[][..],
        &["sandbox"][..],
        &["Sandbox", "a=b"][..],
        &["sandbox", "novalue"][..],
        &["sandbox", "=b"][..],
        &["sandbox", "a=1", "a=2"][..],
        &["sandbox", "a b=1"][..],
    ] {
        assert!(
            matches!(parse(bad), Err(BootstrapSetError::Usage(_))),
            "{bad:?}"
        );
    }
    assert_eq!(
        parse(&["levels", "low.budget=10"]).unwrap().values,
        [("low.budget".to_owned(), "10".to_owned())]
    );
}
