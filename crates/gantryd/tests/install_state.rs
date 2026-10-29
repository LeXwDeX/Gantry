//! `install-state.json` 的写端（`deploy/lib/common.sh`）与读端（`gantryd` CLI）对账。
//!
//! 用真实的 `state_mark_step` 产出状态文件，再交给 `install_test_mode` 读取；
//! 不只测 Rust 侧构造的 JSON（#378 的教训）。

use std::path::Path;

#[test]
fn install_script_state_file_drives_cli_reader() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::env::temp_dir().join(format!("gantry-install-state-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    for (mode, want) in [("false", Some(false)), ("true", Some(true))] {
        let state = dir.join(format!("state-{mode}.json"));
        let script = format!(
            "set -Eeuo pipefail; GANTRY_NO_JOURNAL=1; . '{}/deploy/lib/common.sh'; \
             state_mark_step '{}' 0.1.0 verify_release {mode}",
            root.display(),
            state.display()
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("bash");
        assert!(
            output.status.success(),
            "写状态文件失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(gantryd::cli::install_test_mode(&state), want);
    }

    // 旧版本产出的文件没有 test_mode：读端必须失败闭合（doctor 报 fail），不能当 false。
    let legacy = dir.join("legacy.json");
    std::fs::write(
        &legacy,
        r#"{"schema":1,"version":"0.1.0","completed":[],"updated_at":"x"}"#,
    )
    .unwrap();
    assert_eq!(gantryd::cli::install_test_mode(&legacy), None);

    std::fs::remove_dir_all(&dir).unwrap();
}
