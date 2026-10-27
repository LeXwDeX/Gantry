//! 文件实现专属验收（secrets.md §12.1–§12.3）。

#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use common::{cfg, contains, ctx, fresh, inst_dir, name, sentinel, Key, TempDir};
use gantry_core::ports::secrets::{
    CorruptReason, Expect, SecretSink, SecretSource, SecretValue, SecretVersion, SecretsError,
};
use gantry_core::secret::Consumer;
use gantry_secrets::{
    harden_process, KeyFile, MasterKeySource, SecretAdminOps, Secrets, SecretsConfig,
};

fn val(s: &str) -> SecretValue {
    SecretValue::from_bytes(s.as_bytes().to_vec())
}

fn v(n: u64) -> SecretVersion {
    SecretVersion::new(n).unwrap()
}

fn sec_path(tmp: &TempDir, who: Consumer, file: &str) -> std::path::PathBuf {
    inst_dir(tmp).join(who.as_str()).join(file)
}

fn set_mode(path: &std::path::Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode_of(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

// ---- §12.2 篡改检测 -----------------------------------------------------------

#[test]
fn tampering_any_byte_is_detected() {
    let (tmp, _key, secrets) = fresh(&[Consumer::Forge]);
    let s = secrets.scoped(Consumer::Forge).unwrap();
    s.put(&name("k"), val("value-under-test"), Expect::Any, &ctx())
        .unwrap();
    let path = sec_path(&tmp, Consumer::Forge, "k.v1.sec");
    let original = fs::read(&path).unwrap();
    let len = original.len();
    // 魔数、算法、种类、保留、created_at、nonce、密文、标签。
    for offset in [0, 5, 6, 7, 12, 19, 20, 43, 44, len - 17, len - 1] {
        let mut data = original.clone();
        data[offset] ^= 0x01;
        fs::write(&path, &data).unwrap();
        let err = s.get(&name("k")).unwrap_err();
        assert!(
            matches!(err, SecretsError::Corrupt { .. }),
            "offset {offset}: {err:?}"
        );
    }
    // 格式版本字节 → UnsupportedFormat；key_id 字节 → KeyMismatch（§7.2：AEAD 之前先报）。
    let mut data = original.clone();
    data[4] = 9;
    fs::write(&path, &data).unwrap();
    assert!(matches!(
        s.get(&name("k")),
        Err(SecretsError::UnsupportedFormat { found: 9 })
    ));
    let mut data = original.clone();
    data[8] ^= 0xff;
    fs::write(&path, &data).unwrap();
    assert!(matches!(s.get(&name("k")), Err(SecretsError::KeyMismatch)));
    // 截断。
    fs::write(&path, &original[..30]).unwrap();
    assert!(matches!(
        s.get(&name("k")),
        Err(SecretsError::Corrupt { .. })
    ));
    // 复原后可读，失败从不回退到旧值。
    fs::write(&path, &original).unwrap();
    assert_eq!(
        s.get(&name("k")).unwrap().unwrap().value.expose(),
        b"value-under-test"
    );
}

#[test]
fn copying_across_consumers_or_versions_fails_aead() {
    let (tmp, _key, secrets) = fresh(&[Consumer::Forge, Consumer::Pki]);
    let forge = secrets.scoped(Consumer::Forge).unwrap();
    let pki = secrets.scoped(Consumer::Pki).unwrap();
    forge
        .put(&name("k"), val("forge-value"), Expect::Any, &ctx())
        .unwrap();
    fs::copy(
        sec_path(&tmp, Consumer::Forge, "k.v1.sec"),
        sec_path(&tmp, Consumer::Pki, "k.v1.sec"),
    )
    .unwrap();
    assert!(matches!(
        pki.get(&name("k")),
        Err(SecretsError::Corrupt {
            reason: CorruptReason::AeadFailed,
            ..
        })
    ));

    // 把 v1 改名为 v2 冒充新版本。
    fs::rename(
        sec_path(&tmp, Consumer::Forge, "k.v1.sec"),
        sec_path(&tmp, Consumer::Forge, "k.v2.sec"),
    )
    .unwrap();
    assert!(matches!(
        forge.get(&name("k")),
        Err(SecretsError::Corrupt { .. })
    ));

    // 跨名字复制。
    forge
        .put(&name("a"), val("x"), Expect::Any, &ctx())
        .unwrap();
    fs::copy(
        sec_path(&tmp, Consumer::Forge, "a.v1.sec"),
        sec_path(&tmp, Consumer::Forge, "b.v1.sec"),
    )
    .unwrap();
    assert!(matches!(
        forge.get(&name("b")),
        Err(SecretsError::Corrupt { .. })
    ));
}

// ---- §12.2 隔离 ---------------------------------------------------------------

#[test]
fn cross_consumer_handles_are_denied() {
    let (_tmp, _key, secrets) = fresh(&[Consumer::Forge, Consumer::Pki]);
    assert!(matches!(
        secrets.reader(Consumer::Egress),
        Err(SecretsError::Denied {
            who: Consumer::Egress,
            ..
        })
    ));
    assert!(matches!(
        secrets.scoped(Consumer::Egress),
        Err(SecretsError::Denied {
            who: Consumer::Egress,
            ..
        })
    ));
    assert!(matches!(
        secrets.reader(Consumer::Notify),
        Err(SecretsError::Denied { .. })
    ));
    let pki = secrets.scoped(Consumer::Pki).unwrap();
    pki.put(&name("ca.key"), val("x"), Expect::Any, &ctx())
        .unwrap();
    let forge = secrets.reader(Consumer::Forge).unwrap();
    assert!(forge.get(&name("ca.key")).unwrap().is_none());
}

#[test]
fn gantryd_key_cannot_open_egress_instance() {
    let tmp = TempDir::new("two-instances");
    let gantryd_key = Key::random();
    let egress_key = Key::random();
    let gantryd_dir = tmp.path().join("gantryd");
    let egress_dir = tmp.path().join("egress");
    Secrets::init(cfg(
        &gantryd_dir,
        &[Consumer::Forge, Consumer::Pki],
        &gantryd_key,
    ))
    .unwrap();
    Secrets::init(cfg(&egress_dir, &[Consumer::Egress], &egress_key)).unwrap();
    let egress = Secrets::open(cfg(&egress_dir, &[Consumer::Egress], &egress_key)).unwrap();
    let model_key = sentinel();
    egress
        .scoped(Consumer::Egress)
        .unwrap()
        .put(
            &name("models.anthropic"),
            val(&model_key),
            Expect::Any,
            &ctx(),
        )
        .unwrap();
    assert!(matches!(
        Secrets::open(cfg(&egress_dir, &[Consumer::Egress], &gantryd_key)),
        Err(SecretsError::KeyMismatch)
    ));
    // gantryd 实例里没有 egress 使用方，也没有模型 key 的任何痕迹。
    let gantryd = Secrets::open(cfg(
        &gantryd_dir,
        &[Consumer::Forge, Consumer::Pki],
        &gantryd_key,
    ))
    .unwrap();
    assert!(gantryd.reader(Consumer::Egress).is_err());
    assert!(!gantryd_dir.join("egress").exists());
    // 实例只能按 init 时登记的使用方打开。
    assert!(Secrets::open(cfg(&gantryd_dir, &[Consumer::Egress], &gantryd_key)).is_err());
    // 模型 key 只出现在 Egress 实例目录，且是密文。
    for entry in walk(&egress_dir) {
        assert!(!contains(&fs::read(&entry).unwrap(), model_key.as_bytes()));
    }
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

// ---- §12.2 权限 ---------------------------------------------------------------

fn reopen(tmp: &TempDir, key: &Key) -> Result<Secrets, SecretsError> {
    Secrets::open(cfg(&inst_dir(tmp), &[Consumer::Forge], key))
}

#[test]
fn insecure_permissions_are_rejected_and_not_repaired() {
    let (tmp, key, secrets) = fresh(&[Consumer::Forge]);
    secrets
        .scoped(Consumer::Forge)
        .unwrap()
        .put(&name("k"), val("x"), Expect::Any, &ctx())
        .unwrap();
    drop(secrets);
    let root = inst_dir(&tmp);
    let consumer_dir = root.join("forge");
    let file = consumer_dir.join("k.v1.sec");
    assert_eq!(mode_of(&root), 0o700);
    assert_eq!(mode_of(&consumer_dir), 0o700);
    assert_eq!(mode_of(&file), 0o600);

    for (path, bad, good) in [
        (&root, 0o755, 0o700),
        (&consumer_dir, 0o750, 0o700),
        (&file, 0o644, 0o600),
        (&root.join("secrets.meta"), 0o640, 0o600),
    ] {
        set_mode(path, bad);
        assert!(
            matches!(reopen(&tmp, &key), Err(SecretsError::InsecurePermissions { mode, .. }) if mode == bad),
            "{path:?}"
        );
        assert_eq!(mode_of(path), bad, "open 不自动修复");
        set_mode(path, good);
    }
    assert!(reopen(&tmp, &key).is_ok());
}

#[test]
fn symlinks_are_rejected() {
    let (tmp, key, secrets) = fresh(&[Consumer::Forge]);
    drop(secrets);
    let root = inst_dir(&tmp);
    // 符号链接的实例目录。
    let link = tmp.path().join("link");
    std::os::unix::fs::symlink(&root, &link).unwrap();
    assert!(matches!(
        Secrets::open(cfg(&link, &[Consumer::Forge], &key)),
        Err(SecretsError::InsecurePermissions { .. })
    ));
    // 使用方目录里的符号链接文件。
    let outside = tmp.path().join("outside.sec");
    fs::write(&outside, b"x").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("forge").join("k.v1.sec")).unwrap();
    assert!(matches!(
        reopen(&tmp, &key),
        Err(SecretsError::InsecurePermissions { .. })
    ));
}

#[test]
fn init_refuses_existing_directory() {
    let (tmp, key, _secrets) = fresh(&[Consumer::Forge]);
    assert!(matches!(
        Secrets::init(cfg(&inst_dir(&tmp), &[Consumer::Forge], &key)),
        Err(SecretsError::Io(_))
    ));
}

#[test]
fn key_file_permissions_and_length() {
    let tmp = TempDir::new("keyfile");
    let path = tmp.path().join("master.key");
    let mut bytes = [0u8; 32];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(17) ^ (std::process::id() as u8);
    }
    fs::write(&path, bytes).unwrap();
    set_mode(&path, 0o644);
    let source = KeyFile { path: path.clone() };
    assert!(matches!(
        source.load(),
        Err(SecretsError::InsecurePermissions { mode: 0o644, .. })
    ));
    set_mode(&path, 0o400);
    assert!(source.load().is_ok());
    set_mode(&path, 0o600);
    fs::write(&path, &bytes[..31]).unwrap();
    assert!(matches!(
        source.load(),
        Err(SecretsError::KeyUnavailable(_))
    ));
    let missing = KeyFile {
        path: tmp.path().join("missing"),
    };
    assert!(matches!(
        missing.load(),
        Err(SecretsError::KeyUnavailable(_))
    ));
}

#[test]
fn key_file_drives_a_full_instance() {
    let tmp = TempDir::new("keyfile-inst");
    let key_path = tmp.path().join("master.key");
    // 测试现场生成的密钥字节。
    let mut bytes = [0u8; 32];
    let seed = sentinel();
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = seed.as_bytes()[i % seed.len()] ^ (i as u8);
    }
    fs::write(&key_path, bytes).unwrap();
    set_mode(&key_path, 0o400);
    let dir = tmp.path().join("inst");
    let mk = || {
        SecretsConfig::new(
            &dir,
            vec![Consumer::Forge],
            Box::new(KeyFile {
                path: key_path.clone(),
            }),
        )
    };
    Secrets::init(mk()).unwrap();
    let secrets = Secrets::open(mk()).unwrap();
    secrets
        .scoped(Consumer::Forge)
        .unwrap()
        .put(&name("k"), val("x"), Expect::Any, &ctx())
        .unwrap();
    drop(secrets);
    assert_eq!(
        Secrets::open(mk())
            .unwrap()
            .reader(Consumer::Forge)
            .unwrap()
            .get(&name("k"))
            .unwrap()
            .unwrap()
            .value
            .expose(),
        b"x"
    );
}

// ---- §12.2 值大小 -------------------------------------------------------------

#[test]
fn value_size_limit() {
    let tmp = TempDir::new("size");
    let key = Key::random();
    let dir = tmp.path().join("inst");
    Secrets::init(cfg(&dir, &[Consumer::Forge], &key)).unwrap();
    let mut c = cfg(&dir, &[Consumer::Forge], &key);
    c.max_value_bytes = 64;
    let secrets = Secrets::open(c).unwrap();
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let at_limit = SecretValue::from_bytes(vec![b'a'; 64]);
    s.put(&name("k"), at_limit, Expect::Any, &ctx()).unwrap();
    assert_eq!(s.get(&name("k")).unwrap().unwrap().value.expose().len(), 64);
    let over = SecretValue::from_bytes(vec![b'a'; 65]);
    assert!(matches!(
        s.put(&name("k"), over, Expect::Any, &ctx()),
        Err(SecretsError::TooLarge { limit: 64 })
    ));
    let over = SecretValue::from_bytes(vec![b'a'; 65]);
    assert!(matches!(
        s.stage(&name("k"), over, Expect::Any, &ctx()),
        Err(SecretsError::TooLarge { limit: 64 })
    ));
    assert_eq!(s.version(&name("k")).unwrap(), Some(v(1)));
    // 默认上限 65536。
    let (_tmp2, _k2, defaults) = fresh(&[Consumer::Forge]);
    let d = defaults.scoped(Consumer::Forge).unwrap();
    d.put(
        &name("k"),
        SecretValue::from_bytes(vec![1; 65536]),
        Expect::Any,
        &ctx(),
    )
    .unwrap();
    assert!(matches!(
        d.put(
            &name("k"),
            SecretValue::from_bytes(vec![1; 65537]),
            Expect::Any,
            &ctx()
        ),
        Err(SecretsError::TooLarge { limit: 65536 })
    ));
}

// ---- §12.2 并发 ---------------------------------------------------------------

#[test]
fn concurrent_puts_get_unique_versions_and_readers_never_see_corrupt() {
    let (_tmp, _key, secrets) = fresh(&[Consumer::Forge]);
    let secrets = Arc::new(secrets);
    let n = name("hot");
    const WRITERS: usize = 8;
    const PER: usize = 10;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let secrets = Arc::clone(&secrets);
            let stop = Arc::clone(&stop);
            let n = n.clone();
            std::thread::spawn(move || {
                let r = secrets.reader(Consumer::Forge).unwrap();
                let mut reads = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(secret) = r.get(&n).expect("readers never fail") {
                        let text = secret.value.expose_str().unwrap().to_owned();
                        assert!(text.starts_with("w"), "完整的某个版本");
                        reads += 1;
                    }
                }
                reads
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let secrets = Arc::clone(&secrets);
            let n = n.clone();
            std::thread::spawn(move || {
                let s = secrets.scoped(Consumer::Forge).unwrap();
                (0..PER)
                    .map(|i| {
                        s.put(&n, val(&format!("w{w}-{i}")), Expect::Any, &ctx())
                            .unwrap()
                            .meta
                            .version
                            .get()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut versions: Vec<u64> = writers
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    versions.sort_unstable();
    assert_eq!(versions, (1..=(WRITERS * PER) as u64).collect::<Vec<_>>());
}

// ---- §12.1 M1 退出标准中 secrets 负责的部分 ------------------------------------

/// 写入模型凭据（`api` 泳道执行）的 secrets 侧配合：未提交事件 → abort；提交后 → commit。
#[test]
fn staged_write_pairs_with_settings_event() {
    let (_tmp, _key, secrets) = fresh(&[Consumer::Egress]);
    let s = secrets.scoped(Consumer::Egress).unwrap();
    let admin = secrets.admin();
    let n = name("models.anthropic");
    let c = ctx();
    s.put(&n, val("old-key"), Expect::Any, &c).unwrap();

    // settings.updated 未提交 → abort。
    let staged = s.stage(&n, val("new-key"), Expect::Any, &c).unwrap();
    assert_eq!(admin.staged_all().unwrap(), vec![staged.clone()]);
    s.abort(&staged, &c).unwrap();
    assert!(admin.staged_all().unwrap().is_empty());
    let current = s.get(&n).unwrap().unwrap();
    assert_eq!(
        (current.value.expose(), current.version),
        (&b"old-key"[..], v(1))
    );

    // settings.updated 已提交 → commit，版本 +1。
    let staged = s
        .stage(&n, val("new-key"), Expect::Version(v(1)), &c)
        .unwrap();
    s.commit(&staged, &c).unwrap();
    let current = s.get(&n).unwrap().unwrap();
    assert_eq!(
        (current.value.expose(), current.version),
        (&b"new-key"[..], v(2))
    );

    // 无值的首次写入同理：abort 后 get 仍无值。
    let fresh_name = name("models.other");
    let staged = s.stage(&fresh_name, val("k"), Expect::Absent, &c).unwrap();
    s.abort(&staged, &c).unwrap();
    assert!(s.get(&fresh_name).unwrap().is_none());
}

/// 引导期写入（P-66）：第一台设备配对之前，主机命令以 `ScopedSecrets(Forge)` 写入
/// App 私钥与 webhook secret；规则与普通 `put` 相同。
#[test]
fn bootstrap_writes_github_app_credentials() {
    let (tmp, key, secrets) = fresh(&[Consumer::Forge, Consumer::Pki]);
    let forge = secrets.scoped(Consumer::Forge).unwrap();
    let c = ctx();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        sentinel()
    );
    let a = forge
        .put(
            &name("github.app.private_key"),
            val(&pem),
            Expect::Absent,
            &c,
        )
        .unwrap();
    let b = forge
        .put(
            &name("github.webhook_secret"),
            val(&sentinel()),
            Expect::Absent,
            &c,
        )
        .unwrap();
    assert_eq!((a.meta.version, a.previous), (v(1), None));
    assert_eq!((b.meta.version, b.previous), (v(1), None));
    assert!(matches!(
        forge.put(
            &name("github.app.private_key"),
            val("again"),
            Expect::Absent,
            &c
        ),
        Err(SecretsError::Conflict { .. })
    ));
    drop(forge);
    drop(secrets);
    let reopened = Secrets::open(cfg(
        &inst_dir(&tmp),
        &[Consumer::Forge, Consumer::Pki],
        &key,
    ))
    .unwrap();
    let reader = reopened.reader(Consumer::Forge).unwrap();
    assert_eq!(
        reader
            .get(&name("github.app.private_key"))
            .unwrap()
            .unwrap()
            .value
            .expose_str()
            .unwrap(),
        pem
    );
}

/// 沙盒内读不到：以另一个操作系统用户身份读取实例目录失败。需要 root 才能切换用户，
/// 非 root 环境跳过（B 级证据，见 docs/m1-acceptance.md）。
#[test]
fn other_os_user_cannot_read_instance() {
    if rustix::process::geteuid().as_raw() != 0 {
        eprintln!("skip: needs root to run as another user");
        return;
    }
    use std::os::unix::process::CommandExt;
    let (tmp, _key, secrets) = fresh(&[Consumer::Egress]);
    secrets
        .scoped(Consumer::Egress)
        .unwrap()
        .put(&name("models.anthropic"), val("x"), Expect::Any, &ctx())
        .unwrap();
    // 让 nobody 能穿过临时目录本身，只剩实例目录的 0700 挡住它。
    set_mode(tmp.path(), 0o755);
    let target = inst_dir(&tmp)
        .join("egress")
        .join("models.anthropic.v1.sec");
    let status = std::process::Command::new("cat")
        .arg(&target)
        .uid(65534)
        .gid(65534)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
    let status = std::process::Command::new("ls")
        .arg(inst_dir(&tmp))
        .uid(65534)
        .gid(65534)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}

// ---- 磁盘上无明文 / 重开后状态 -------------------------------------------------

#[test]
fn files_on_disk_never_contain_plaintext() {
    let (tmp, _key, secrets) = fresh(&[Consumer::Forge]);
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let secret = sentinel();
    s.put(&name("k"), val(&secret), Expect::Any, &ctx())
        .unwrap();
    s.stage(&name("k"), val(&secret), Expect::Any, &ctx())
        .unwrap();
    for path in walk(&inst_dir(&tmp)) {
        assert!(
            !contains(&fs::read(&path).unwrap(), secret.as_bytes()),
            "{path:?}"
        );
    }
}

#[test]
fn staged_entries_survive_reopen_and_are_not_auto_resolved() {
    let (tmp, key, secrets) = fresh(&[Consumer::Forge]);
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let staged = s
        .stage(&name("k"), val("pending"), Expect::Any, &ctx())
        .unwrap();
    drop(s);
    drop(secrets);
    let reopened = reopen(&tmp, &key).unwrap();
    assert_eq!(reopened.admin().staged_all().unwrap(), vec![staged.clone()]);
    let s = reopened.scoped(Consumer::Forge).unwrap();
    assert!(s.get(&name("k")).unwrap().is_none());
    s.commit(&staged, &ctx()).unwrap();
    assert_eq!(
        s.get(&name("k")).unwrap().unwrap().value.expose(),
        b"pending"
    );
}

#[test]
fn commit_after_competing_put_conflicts() {
    let (_tmp, _key, secrets) = fresh(&[Consumer::Forge]);
    let s = secrets.scoped(Consumer::Forge).unwrap();
    let staged = s
        .stage(&name("k"), val("staged"), Expect::Any, &ctx())
        .unwrap();
    s.put(&name("k"), val("direct"), Expect::Any, &ctx())
        .unwrap();
    assert!(matches!(
        s.commit(&staged, &ctx()),
        Err(SecretsError::Conflict { .. })
    ));
    assert_eq!(
        s.get(&name("k")).unwrap().unwrap().value.expose(),
        b"direct"
    );
    s.abort(&staged, &ctx()).unwrap();
}

// ---- §12.3 进程加固 -----------------------------------------------------------

#[test]
fn harden_process_disables_core_dumps() {
    harden_process().unwrap();
    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    assert_eq!(limit.current, Some(0));
    #[cfg(target_os = "linux")]
    assert_eq!(
        rustix::process::dumpable_behavior().unwrap(),
        rustix::process::DumpableBehavior::NotDumpable
    );
}
