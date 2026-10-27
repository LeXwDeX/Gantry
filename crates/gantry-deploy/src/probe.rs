//! 对 webhook 入口的外部 HTTP 探测（deploy.md §7.6 验证 ②）。
//!
//! 只关心状态码，不读响应体（I5：外部文本不进日志）。生产实现调用系统的 `curl`：
//! 公网入口要走公共受信 TLS，借用系统证书库，避免在本 crate 里引入 TLS 栈。

use std::process::Command;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

pub trait HttpProbe {
    /// 返回 HTTP 状态码；连不上（DNS、TCP、TLS 失败）返回 `Err`，附简短原因。
    fn status(&self, method: Method, url: &str) -> Result<u16, String>;
}

pub struct CurlProbe {
    pub timeout: Duration,
}

impl Default for CurlProbe {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
        }
    }
}

impl HttpProbe for CurlProbe {
    fn status(&self, method: Method, url: &str) -> Result<u16, String> {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--max-redirs",
                "0",
            ])
            .args(["--output", "/dev/null", "--write-out", "%{http_code}"])
            .args(["--max-time", &self.timeout.as_secs().max(1).to_string()])
            .args(["--request", method.as_str()])
            .arg("--")
            .arg(url)
            .output()
            .map_err(|e| format!("无法执行 curl：{e}"))?;
        let code = String::from_utf8_lossy(&output.stdout);
        match code.trim().parse::<u16>() {
            Ok(0) | Err(_) => Err(format!(
                "请求失败（curl 退出码 {}）",
                output.status.code().unwrap_or(-1)
            )),
            Ok(code) => Ok(code),
        }
    }
}
