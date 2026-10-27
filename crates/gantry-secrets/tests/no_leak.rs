//! 哨兵测试（secrets.md §12.3）：值不出现在 `tracing` 输出、错误的 `Debug` / `Display`、
//! 元数据类型的 `Debug` 与 panic 信息里。

#![cfg(unix)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::{ctx, fresh, name, sentinel};
use gantry_core::ports::secrets::{Expect, SecretSink, SecretSource, SecretValue, SecretVersion};
use gantry_core::secret::Consumer;
use gantry_secrets::SecretAdminOps;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn sentinel_never_leaks() {
    let secret = sentinel();
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    let mut seen: Vec<String> = Vec::new();
    let val = || SecretValue::from_bytes(secret.as_bytes().to_vec());

    tracing::subscriber::with_default(subscriber, || {
        let (tmp, _key, secrets) = fresh(&[Consumer::Forge]);
        let s = secrets.scoped(Consumer::Forge).unwrap();
        let c = ctx();
        let n = name("k");
        let out = s.put(&n, val(), Expect::Any, &c).unwrap();
        seen.push(format!("{out:?}"));
        let got = s.get(&n).unwrap().unwrap();
        seen.push(format!("{got:?}"));
        seen.push(format!("{:?}", got.value));
        let staged = s.stage(&n, val(), Expect::Any, &c).unwrap();
        seen.push(format!("{staged:?}"));
        seen.push(format!("{:?}", secrets.admin().staged_all().unwrap()));
        let meta = s.commit(&staged, &c).unwrap();
        seen.push(format!("{meta:?}"));
        seen.push(format!("{:?}", s.list().unwrap()));
        seen.push(format!("{:?}", s.delete(&n, Expect::Any, &c).unwrap()));
        seen.push(format!("{secrets:?} {s:?} {:?}", secrets.admin()));

        // 错误路径：冲突、暂存冲突、过大、未暂存、损坏、拒绝。
        let mut errors = Vec::new();
        s.put(&n, val(), Expect::Any, &c).unwrap();
        errors.push(s.put(&n, val(), Expect::Absent, &c).unwrap_err());
        let staged = s.stage(&n, val(), Expect::Any, &c).unwrap();
        errors.push(s.stage(&n, val(), Expect::Any, &c).unwrap_err());
        s.abort(&staged, &c).unwrap();
        errors.push(s.abort(&staged, &c).unwrap_err());
        let mut huge = secret.as_bytes().to_vec();
        huge.resize(70_000, b'x');
        errors.push(
            s.put(&n, SecretValue::from_bytes(huge), Expect::Any, &c)
                .unwrap_err(),
        );
        errors.push(
            s.put(
                &n,
                val(),
                Expect::Version(SecretVersion::new(99).unwrap()),
                &c,
            )
            .unwrap_err(),
        );
        errors.push(secrets.reader(Consumer::Egress).unwrap_err());
        let path = tmp.path().join("inst/forge");
        let current = std::fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "sec"))
            .max()
            .unwrap();
        let mut data = std::fs::read(&current).unwrap();
        let last = data.len() - 1;
        data[last] ^= 1;
        std::fs::write(&current, data).unwrap();
        errors.push(s.get(&n).unwrap_err());
        for error in &errors {
            seen.push(format!("{error:?}"));
            seen.push(format!("{error}"));
        }

        // panic 信息：unwrap 一个携带错误的 Result。
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.get(&n).unwrap();
        }))
        .unwrap_err();
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default();
        assert!(!message.is_empty());
        seen.push(message);
    });

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("op=\"put\""), "日志应有记录：{logs}");
    assert!(!logs.contains(&secret), "日志泄露了值");
    for text in seen {
        assert!(!text.contains(&secret), "泄露：{text}");
    }
}
