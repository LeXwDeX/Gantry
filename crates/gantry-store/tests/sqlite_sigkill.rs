//! 进程崩溃测试；SIGKILL 不模拟断电。
#![cfg(all(unix, feature = "testing"))]
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use gantry_core::entities::{Actor, ActorKind, Seq};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::ids::IdGen;
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{
    Appended, DeliveryRow, EventFilter, LogIdentity, ModuleSpec, Store, StoreConfig, StoreDeps,
    VerifyLevel,
};
use serde_json::{json, Value};

const CASES: &str = include_str!("../../gantry-core/fixtures/events/cases.json");
const PROJECT: &str = "prj_01JB7K2M9Q4X8V3N5R6T1W0YZA";
const BASE: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZB";
const NEXT: &str = "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZC";
const POLICY: ModuleSpec = ModuleSpec::new("policy", &["policy_challenges"]);
const READY: &[u8] = b"gantry-sigkill-ready\n";
const TIMEOUT: Duration = Duration::from_secs(15);

struct Scratch {
    root: PathBuf,
    dir: PathBuf,
}

impl Scratch {
    fn new() -> Arc<Self> {
        let random = rusqlite::Connection::open_in_memory().unwrap();
        for _ in 0..100 {
            let name: String = random
                .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))
                .unwrap();
            let executable = std::env::current_exe().unwrap();
            let dir = executable
                .parent()
                .unwrap()
                .join(format!("gantry-store-sigkill-{name}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    return Arc::new(Self {
                        root: dir.clone(),
                        dir,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create scratch: {error}"),
            }
        }
        panic!("could not create a unique scratch directory");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Process {
    child: Child,
    owner: Option<Arc<Scratch>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    // 不能确认子进程已退出时保留其数据。
                    if let Some(owner) = self.owner.take() {
                        std::mem::forget(owner);
                    }
                    return;
                }
            }
        }
    }
}

fn deps(ms: i64) -> StoreDeps {
    let clock = Arc::new(FakeClock::new(ms));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(5))));
    StoreDeps { clock, ids }
}

async fn open(path: &Path, ms: i64) -> Store {
    Store::open(StoreConfig::new(path), deps(ms), &[POLICY])
        .await
        .unwrap()
}

fn delivery(id: &str) -> NewEvent {
    let cases: Value = serde_json::from_str(CASES).unwrap();
    NewEvent {
        ty: EventType::DeliveryCreated,
        subject: Subject::parse(id).unwrap(),
        project_id: Some(PROJECT.parse().unwrap()),
        actor: Actor::new(ActorKind::Engine, "engine"),
        causation_id: None,
        correlation_id: None,
        data: cases["valid"]["delivery.created"].clone(),
        dedup_key: None,
    }
}

#[derive(Debug, PartialEq)]
struct State {
    identity: LogIdentity,
    head: Seq,
    events: Vec<Event<Value>>,
    base: Option<DeliveryRow>,
    next: Option<DeliveryRow>,
    cursor: Seq,
    module: Option<Value>,
}

async fn state(store: &Store) -> State {
    let head = store.events().head().await.unwrap();
    let events = store
        .events()
        .scan(Seq::ZERO, &EventFilter::default(), 100)
        .await
        .unwrap()
        .events;
    let snapshot = store.snapshot().await.unwrap();
    let report = store.verify(VerifyLevel::Full).await.unwrap();
    assert!(report.is_ok(), "{:?}", report.problems);
    State {
        identity: store.log_identity(),
        head,
        events,
        base: snapshot.delivery(&BASE.parse().unwrap()),
        next: snapshot.delivery(&NEXT.parse().unwrap()),
        cursor: store.cursors().load("sigkill").await.unwrap(),
        module: snapshot
            .module_conn("policy")
            .get("policy_challenges", "key")
            .unwrap(),
    }
}

// 仅由父进程显式选择；输入经管道传入，不依赖环境变量。
#[test]
#[ignore]
fn child() {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut mode = [0];
    input.read_exact(&mut mode).unwrap();
    assert!(matches!(mode[0], b'b' | b'a'), "invalid child mode");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = open(Path::new("."), 1_700_000_100_000).await;
        let mut tx = store.begin_write().await.unwrap();
        let Appended::New(event) = tx.append(delivery(NEXT)).await.unwrap() else {
            panic!("duplicate event")
        };
        assert_eq!(event.seq.as_u64(), 2);
        tx.advance_cursor("sigkill", event.seq).await.unwrap();
        tx.module_conn("policy")
            .put("policy_challenges", "key", &json!({"value": "child"}))
            .unwrap();
        match mode[0] {
            b'b' => {
                // WriteTx 尚在内存中；SQLite 事务只在 commit 中开始。
                assert!(tx.read().delivery(&NEXT.parse().unwrap()).is_some());
                std::io::stdout().write_all(READY).unwrap();
                std::io::stdout().flush().unwrap();
                let mut byte = [0];
                assert_eq!(input.read(&mut byte).unwrap(), 0);
                panic!("parent closed pipe before SIGKILL");
            }
            b'a' => {
                let committed = tx.commit().await.unwrap();
                assert_eq!((committed.first_seq, committed.last_seq), (2, 2));
                std::io::stdout().write_all(READY).unwrap();
                std::io::stdout().flush().unwrap();
                let mut byte = [0];
                assert_eq!(input.read(&mut byte).unwrap(), 0);
                panic!("parent closed pipe before SIGKILL");
            }
            _ => panic!("invalid child mode"),
        }
    });
}

#[derive(Clone, Copy)]
enum CrashPoint {
    Before,
    After,
}

fn crash(scratch: &Arc<Scratch>, mode: CrashPoint) {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(&scratch.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut process = Process {
        child,
        owner: Some(scratch.clone()),
    };
    let mut input = process.child.stdin.take().unwrap();
    input
        .write_all(&[match mode {
            CrashPoint::Before => b'b',
            CrashPoint::After => b'a',
        }])
        .unwrap();
    input.flush().unwrap();
    let output = process.child.stdout.take().unwrap();
    let (send, recv) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(output);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => {
                    let _ = send.send(Err("child stdout closed".to_owned()));
                    return;
                }
                Ok(_) if line.ends_with(READY) => {
                    let _ = send.send(Ok(()));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = send.send(Err(error.to_string()));
                    return;
                }
            }
        }
    });
    recv.recv_timeout(TIMEOUT)
        .expect("child readiness timeout")
        .expect("child failed before ready");
    assert!(
        process.child.try_wait().unwrap().is_none(),
        "child exited before SIGKILL"
    );
    process.child.kill().unwrap();
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "child did not exit after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.signal(), Some(9), "expected SIGKILL: {status:?}");
    drop(input);
}

#[test]
fn sigkill_before_and_after_commit() {
    exercise(&Scratch::new());
}

fn exercise(scratch: &Arc<Scratch>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let baseline = runtime.block_on(async {
        let store = open(&scratch.dir, 1_700_000_000_000).await;
        let mut tx = store.begin_write().await.unwrap();
        tx.append(delivery(BASE)).await.unwrap();
        tx.advance_cursor("sigkill", Seq::new(1).unwrap())
            .await
            .unwrap();
        tx.module_conn("policy")
            .put("policy_challenges", "key", &json!({"value": "baseline"}))
            .unwrap();
        tx.commit().await.unwrap();
        state(&store).await
    });
    assert_eq!(baseline.head.as_u64(), 1);
    assert_eq!(baseline.events.len(), 1);
    assert_eq!(baseline.events[0].seq.as_u64(), 1);
    assert_eq!(
        baseline.base,
        Some(DeliveryRow::created(&baseline.events[0]).unwrap())
    );
    assert!(baseline.next.is_none());
    assert_eq!(baseline.cursor.as_u64(), 1);
    assert_eq!(baseline.module, Some(json!({"value": "baseline"})));

    crash(scratch, CrashPoint::Before);
    runtime.block_on(async {
        let store = open(&scratch.dir, 1_700_000_200_000).await;
        assert_eq!(state(&store).await, baseline);
    });

    crash(scratch, CrashPoint::After);
    let committed = runtime.block_on(async {
        let store = open(&scratch.dir, 1_700_000_300_000).await;
        let actual = state(&store).await;
        assert_eq!(actual.identity, baseline.identity);
        assert_eq!(actual.head.as_u64(), 2);
        assert_eq!(actual.events.len(), 2);
        assert_eq!(actual.events[0], baseline.events[0]);
        let event = &actual.events[1];
        let expected = delivery(NEXT);
        assert_eq!(event.seq.as_u64(), 2);
        assert_eq!(event.ty, expected.ty);
        assert_eq!(event.subject, expected.subject);
        assert_eq!(event.project_id, expected.project_id);
        assert_eq!(event.actor, expected.actor);
        assert_eq!(event.data, expected.data);
        assert_eq!(event.causation_id, expected.causation_id);
        assert_eq!(event.correlation_id, expected.correlation_id);
        assert_eq!(event.ts.as_millis(), 1_700_000_100_000);
        assert_eq!(actual.base, baseline.base);
        assert_eq!(actual.next, Some(DeliveryRow::created(event).unwrap()));
        let row = actual.next.as_ref().unwrap();
        assert_eq!(row.title, expected.data["title"].as_str().unwrap());
        assert_eq!(row.user_text, expected.data["user_text"].as_str().unwrap());
        assert_eq!(row.stage_entered.seq, event.seq);
        assert_eq!(actual.cursor.as_u64(), 2);
        assert_eq!(actual.module, Some(json!({"value": "child"})));
        actual
    });
    runtime.block_on(async {
        let store = open(&scratch.dir, 1_700_000_400_000).await;
        assert_eq!(state(&store).await, committed);
    });
}

#[test]
fn newline_in_parent_path_does_not_redirect_child() {
    let mut scratch = Scratch::new();
    let root = scratch.root.clone();
    let neighbor = root.join("line");
    std::fs::create_dir(&neighbor).unwrap();
    std::fs::write(neighbor.join("sentinel"), b"unchanged").unwrap();
    let parent = root.join("line\nbreak");
    std::fs::create_dir(&parent).unwrap();
    let dir = parent.join("scratch");
    std::fs::create_dir(&dir).unwrap();
    Arc::get_mut(&mut scratch).unwrap().dir = dir.clone();

    exercise(&scratch);

    assert!(dir.join("gantry.db").is_file());
    assert_eq!(
        std::fs::read(neighbor.join("sentinel")).unwrap(),
        b"unchanged"
    );
    let entries: Vec<_> = std::fs::read_dir(&neighbor)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("sentinel")]);
}
