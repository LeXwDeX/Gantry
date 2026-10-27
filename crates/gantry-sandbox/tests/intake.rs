//! sandbox.md §12.F：intake 的白名单、判型、大小与文本规则（方式 A 与方式 B）。

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gantry_core::ports::sandbox::ArtifactName;
use gantry_core::worker::PatchMode;
use gantry_sandbox::intake::host::read_out_dir;
use gantry_sandbox::intake::tar::read_out_tar;
use gantry_sandbox::intake::{IntakePlan, IntakeResult, MAX_UNEXPECTED};

const RESULT: &[u8] = br#"{"job_id":"att_x","status":"succeeded"}"#;

fn plan() -> IntakePlan {
    IntakePlan {
        result_max_bytes: 1024,
        report_max_bytes: 512,
        proposals_max_bytes: 512,
        patch_max_bytes: 2048,
        patch: PatchMode::Optional,
        total_max_bytes: 16 * 1024,
    }
}

/// 每个用例一个独立的临时目录（不依赖 tempfile crate）。
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "gantry-sandbox-intake-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("out")).unwrap();
        Self(path)
    }

    fn out(&self) -> PathBuf {
        self.0.join("out")
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        fs::write(self.out().join(name), bytes).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn reasons(result: &IntakeResult) -> Vec<(String, String)> {
    result
        .unexpected
        .iter()
        .map(|item| (item.name.clone(), item.reason.clone()))
        .collect()
}

fn has(result: &IntakeResult, name: &str) -> bool {
    result.files.contains_key(&ArtifactName::new(name))
}

fn mkfifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: c 是有效路径。
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
}

// ---------------------------------------------------------------------------
// 方式 A：主机直读
// ---------------------------------------------------------------------------

#[test]
fn host_accepts_the_four_whitelisted_files() {
    let dir = TempDir::new();
    dir.write("result.json", RESULT);
    dir.write("changes.patch", b"diff --git a/x b/x\n");
    dir.write("report.md", "# 报告\n".as_bytes());
    dir.write("proposals.json", b"[]");
    let result = read_out_dir(&dir.out(), &plan()).unwrap();
    assert!(result.unexpected.is_empty(), "{:?}", reasons(&result));
    assert_eq!(result.files.len(), 4);
    assert_eq!(result.files[&ArtifactName::new("result.json")], RESULT);
}

#[test]
fn host_discards_links_fifos_dirs_and_extra_files_without_blocking() {
    let dir = TempDir::new();
    std::os::unix::fs::symlink("/etc/passwd", dir.out().join("result.json")).unwrap();
    mkfifo(&dir.out().join("report.md"));
    dir.write("proposals.json", b"[]");
    fs::hard_link(dir.out().join("proposals.json"), dir.0.join("outside-link")).unwrap();
    fs::create_dir(dir.out().join("changes.patch")).unwrap();
    fs::create_dir(dir.out().join("sub")).unwrap();
    dir.write("result.json.tmp", RESULT);
    dir.write("extra.txt", b"x");
    // 大小写不符的名字在 tar 用例里测：macOS 默认文件系统不区分大小写，
    // 这里写 Result.json 会顺着上面的 result.json 符号链接写到 /etc/passwd。

    // FIFO 不阻塞：整个读取应在很短时间内完成。
    let started = std::time::Instant::now();
    let result = read_out_dir(&dir.out(), &plan()).unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    assert!(result.files.is_empty(), "{:?}", result.files.keys());
    let mut got = reasons(&result);
    got.sort();
    let mut want: Vec<(String, String)> = [
        ("changes.patch", "directory"),
        ("extra.txt", "not_whitelisted"),
        ("proposals.json", "hardlink"),
        ("report.md", "fifo"),
        ("result.json", "symlink"),
        ("result.json.tmp", "not_whitelisted"),
        ("sub", "directory"),
    ]
    .iter()
    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
    .collect();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn host_out_dir_symlink_is_a_collect_failure() {
    let dir = TempDir::new();
    let real = dir.0.join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("result.json"), RESULT).unwrap();
    let link = dir.0.join("linked-out");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(read_out_dir(&link, &plan()).is_err());
    assert!(read_out_dir(&dir.0.join("missing"), &plan()).is_err());
}

#[test]
fn host_size_boundary_uses_bytes_actually_read() {
    let dir = TempDir::new();
    let mut at_limit = vec![b' '; 1024 - 2];
    at_limit.splice(0..0, *b"{");
    at_limit.push(b'}');
    assert_eq!(at_limit.len(), 1024);
    dir.write("result.json", &at_limit);
    dir.write("report.md", &vec![b'a'; 513]);
    let result = read_out_dir(&dir.out(), &plan()).unwrap();
    assert!(has(&result, "result.json"));
    assert_eq!(
        reasons(&result),
        vec![("report.md".to_owned(), "too_large".to_owned())]
    );
}

#[test]
fn host_rejects_bad_text_and_json() {
    for (name, bytes, reason) in [
        ("result.json", &b"{\"a\":1,\"a\":2}"[..], "invalid_json"),
        ("result.json", &b"\xff\xfe{}"[..], "invalid_utf8"),
        ("proposals.json", &b"[1,"[..], "invalid_json"),
        ("report.md", &b"\xc3\x28"[..], "invalid_utf8"),
    ] {
        let dir = TempDir::new();
        dir.write(name, bytes);
        let result = read_out_dir(&dir.out(), &plan()).unwrap();
        assert!(result.files.is_empty(), "{name}");
        assert_eq!(reasons(&result), vec![(name.to_owned(), reason.to_owned())]);
    }
    // 深度 32 通过、33 被拒。
    let dir = TempDir::new();
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    dir.write("result.json", deep(32).as_bytes());
    dir.write("proposals.json", deep(33).as_bytes());
    let result = read_out_dir(&dir.out(), &plan()).unwrap();
    assert!(has(&result, "result.json"));
    assert_eq!(
        reasons(&result),
        vec![("proposals.json".to_owned(), "invalid_json".to_owned())]
    );
}

#[test]
fn forbidden_patch_is_discarded_but_others_kept() {
    let dir = TempDir::new();
    dir.write("result.json", RESULT);
    dir.write("changes.patch", b"diff");
    let plan = IntakePlan {
        patch: PatchMode::Forbidden,
        ..plan()
    };
    let result = read_out_dir(&dir.out(), &plan).unwrap();
    assert!(has(&result, "result.json"));
    assert_eq!(
        reasons(&result),
        vec![("changes.patch".to_owned(), "patch_forbidden".to_owned())]
    );
}

#[test]
fn unexpected_names_are_sanitized_and_bounded() {
    let dir = TempDir::new();
    dir.write("evil\u{1b}[31m\u{202e}name", b"x");
    for i in 0..(MAX_UNEXPECTED + 10) {
        dir.write(&format!("junk-{i:03}"), b"x");
    }
    let result = read_out_dir(&dir.out(), &plan()).unwrap();
    assert_eq!(result.unexpected.len(), MAX_UNEXPECTED + 1);
    assert!(result
        .unexpected
        .iter()
        .all(|item| !item.name.chars().any(|c| c.is_control() || c == '\u{202e}')));
    assert_eq!(result.unexpected.last().unwrap().reason, "too_many_entries");
}

#[test]
fn repeated_collect_is_stable() {
    let dir = TempDir::new();
    dir.write("result.json", RESULT);
    dir.write("extra", b"x");
    let first = read_out_dir(&dir.out(), &plan()).unwrap();
    let second = read_out_dir(&dir.out(), &plan()).unwrap();
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// 方式 B：tar 归档导出
// ---------------------------------------------------------------------------

/// 手写 ustar 头，便于构造恶意归档。
fn header(name: &str, typeflag: u8, size: u64, declared: Option<u64>) -> [u8; 512] {
    let mut h = [0u8; 512];
    let (prefix, name) = if name.len() > 100 {
        let split = name.len() - 100;
        (&name[..split.saturating_sub(1)], &name[split..])
    } else {
        ("", name)
    };
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0001750\0");
    h[116..124].copy_from_slice(b"0001750\0");
    let size_text = format!("{:011o}\0", declared.unwrap_or(size));
    h[124..136].copy_from_slice(size_text.as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = typeflag;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    h[148..156].copy_from_slice(b"        ");
    let sum: u64 = h.iter().map(|b| u64::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

#[derive(Default)]
struct Tar(Vec<u8>);

impl Tar {
    fn entry(mut self, name: &str, typeflag: u8, data: &[u8]) -> Self {
        self.0
            .extend_from_slice(&header(name, typeflag, data.len() as u64, None));
        self.data(data)
    }

    fn lying(mut self, name: &str, declared: u64, data: &[u8]) -> Self {
        self.0
            .extend_from_slice(&header(name, b'0', data.len() as u64, Some(declared)));
        self.data(data)
    }

    fn data(mut self, data: &[u8]) -> Self {
        self.0.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.0.extend(std::iter::repeat_n(0u8, pad));
        self
    }

    fn end(mut self) -> Vec<u8> {
        self.0.extend_from_slice(&[0u8; 1024]);
        self.0
    }
}

fn tar_read(bytes: Vec<u8>, plan: &IntakePlan) -> IntakeResult {
    read_out_tar(Cursor::new(bytes), plan, Some("out"))
}

#[test]
fn tar_accepts_regular_files_under_root() {
    let archive = Tar::default()
        .entry("out/", b'5', b"")
        .entry("out/result.json", b'0', RESULT)
        .entry("out/report.md", 0, b"ok")
        .end();
    let result = tar_read(archive, &plan());
    assert!(result.unexpected.is_empty(), "{:?}", reasons(&result));
    assert_eq!(result.files[&ArtifactName::new("result.json")], RESULT);
    assert!(has(&result, "report.md"));
}

#[test]
fn tar_rejects_special_entries_and_bad_names() {
    let archive = Tar::default()
        .entry("out/", b'5', b"")
        .entry("out/result.json", b'2', b"")
        .entry("out/report.md", b'1', b"")
        .entry("out/sub", b'5', b"")
        .entry("out/proposals.json", b'6', b"")
        .entry("out/changes.patch", b'3', b"")
        .entry("out/x", b'x', b"path=../../etc/passwd\n")
        .entry("out/y", b'L', b"long")
        .entry("out/z", b'S', b"")
        .entry("out/../result.json", b'0', RESULT)
        .entry("out/sub/result.json", b'0', RESULT)
        .entry("etc/passwd", b'0', b"root")
        .entry("/out/result.json", b'0', RESULT)
        .entry("out/Result.json", b'0', RESULT)
        .end();
    let result = tar_read(archive, &plan());
    assert!(result.files.is_empty(), "{:?}", result.files.keys());
    let got = reasons(&result);
    let want = [
        ("result.json", "symlink"),
        ("report.md", "hardlink"),
        ("sub", "directory"),
        ("proposals.json", "fifo"),
        ("changes.patch", "device"),
        ("x", "archive_extension_header"),
        ("y", "archive_extension_header"),
        ("z", "archive_sparse"),
        ("../result.json", "invalid_name"),
        ("sub/result.json", "invalid_name"),
        ("etc/passwd", "invalid_name"),
        ("/out/result.json", "invalid_name"),
        ("Result.json", "not_whitelisted"),
    ];
    assert_eq!(
        got,
        want.iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn tar_duplicate_entries_void_the_artifact() {
    let archive = Tar::default()
        .entry("out/result.json", b'0', RESULT)
        .entry("out/result.json", b'0', b"{}")
        .entry("out/report.md", b'0', b"ok")
        .end();
    let result = tar_read(archive, &plan());
    assert!(!has(&result, "result.json"));
    assert!(has(&result, "report.md"));
    assert_eq!(
        reasons(&result),
        vec![("result.json".to_owned(), "duplicate_entry".to_owned())]
    );
}

#[test]
fn tar_size_limits_and_lying_headers() {
    // limit 通过、limit + 1 被拒。
    let archive = Tar::default()
        .entry("out/report.md", b'0', &vec![b'a'; 512])
        .entry("out/proposals.json", b'0', &vec![b' '; 513])
        .end();
    let result = tar_read(archive, &plan());
    assert!(has(&result, "report.md"));
    assert_eq!(
        reasons(&result),
        vec![("proposals.json".to_owned(), "too_large".to_owned())]
    );

    // 头里声明的 size 很小而实际数据很大：多出的数据被当成下一个头，校验失败即中止。
    let archive = Tar::default()
        .lying("out/report.md", 2, &vec![b'a'; 4096])
        .entry("out/result.json", b'0', RESULT)
        .end();
    let result = tar_read(archive, &plan());
    assert!(!has(&result, "result.json"));
    assert_eq!(
        reasons(&result).last().unwrap(),
        &("<archive>".to_owned(), "archive_malformed".to_owned())
    );
}

#[test]
fn tar_total_cap_aborts_reading() {
    // 大量被丢弃的大条目：总字节数超过封顶后中止，不再读后面的条目。
    let plan = IntakePlan {
        total_max_bytes: 4096,
        ..plan()
    };
    let mut tar = Tar::default();
    for i in 0..300 {
        tar = tar.entry(&format!("out/junk{i}"), b'0', &vec![b'x'; 4096]);
    }
    let archive = tar.entry("out/result.json", b'0', RESULT).end();
    let result = tar_read(archive, &plan);
    assert!(!has(&result, "result.json"));
    assert!(reasons(&result)
        .iter()
        .any(|(name, reason)| name == "<archive>" && reason == "archive_total_exceeded"));
}

#[test]
fn tar_truncated_or_corrupt_archives_are_malformed() {
    let mut archive = Tar::default().entry("out/result.json", b'0', RESULT).end();
    archive.truncate(512 + 10);
    let result = tar_read(archive, &plan());
    assert!(result.files.is_empty());
    assert_eq!(
        reasons(&result),
        vec![("<archive>".to_owned(), "archive_malformed".to_owned())]
    );

    let mut archive = Tar::default().entry("out/result.json", b'0', RESULT).end();
    archive[0] ^= 0x01; // 破坏校验和
    let result = tar_read(archive, &plan());
    assert!(result.files.is_empty());
    assert_eq!(
        reasons(&result),
        vec![("<archive>".to_owned(), "archive_malformed".to_owned())]
    );
}

#[test]
fn both_modes_agree_on_the_same_content() {
    let dir = TempDir::new();
    dir.write("result.json", RESULT);
    dir.write("report.md", b"ok");
    dir.write("extra", b"x");
    let host = read_out_dir(&dir.out(), &plan()).unwrap();
    let archive = Tar::default()
        .entry("out/", b'5', b"")
        .entry("out/extra", b'0', b"x")
        .entry("out/report.md", b'0', b"ok")
        .entry("out/result.json", b'0', RESULT)
        .end();
    assert_eq!(host, tar_read(archive, &plan()));
}
