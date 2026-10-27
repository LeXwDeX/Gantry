//! `gantry settings bootstrap-set <resource> <键>=<值>…`（deploy.md §7.9；P-66）。
//!
//! 只做参数校验与转发：经控制通道调 `settings.bootstrap_set {resource, values}`，值一律按
//! 字符串原样传，由服务端按 `core::bootstrap_writable` 白名单与设置校验处理；第一台设备
//! 配对之后服务端拒绝（错误码 `not_bootstrap_period`）。

use serde_json::{json, Map, Value};

use crate::control::{ControlClient, ControlError};

pub const USAGE: &str = "usage: gantry settings bootstrap-set <resource> <键>=<值> [<键>=<值>…]";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapSetArgs {
    pub resource: String,
    pub values: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BootstrapSetError {
    #[error("{0}\n{USAGE}")]
    Usage(String),
    #[error(transparent)]
    Control(#[from] ControlError),
}

fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
}

impl BootstrapSetArgs {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, BootstrapSetError> {
        let usage = |m: String| BootstrapSetError::Usage(m);
        let mut args = args.into_iter();
        let resource = args
            .next()
            .filter(|r| is_ident(r))
            .ok_or_else(|| usage("缺少资源名或资源名不合法".into()))?;
        let mut values: Vec<(String, String)> = Vec::new();
        for pair in args {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| usage(format!("{pair} 不是 <键>=<值>")))?;
            if !is_ident(key) {
                return Err(usage(format!("键名不合法：{key}")));
            }
            if values.iter().any(|(k, _)| k == key) {
                return Err(usage(format!("键 {key} 重复")));
            }
            values.push((key.to_owned(), value.to_owned()));
        }
        if values.is_empty() {
            return Err(usage("至少给一个 <键>=<值>".into()));
        }
        Ok(Self { resource, values })
    }
}

pub fn run(
    args: &BootstrapSetArgs,
    control: &dyn ControlClient,
) -> Result<Value, BootstrapSetError> {
    let values: Map<String, Value> = args
        .values
        .iter()
        .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
        .collect();
    Ok(control.call(
        "settings.bootstrap_set",
        json!({ "resource": args.resource, "values": values }),
    )?)
}
