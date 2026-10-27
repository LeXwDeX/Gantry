#![cfg(feature = "testing")]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use gantry_core::entities::{Actor, ActorKind, Seq};
use gantry_core::events::{Event, EventType, NewEvent, Subject};
use gantry_core::ids::{DeviceId, Id, IdGen, ProjectId};
use gantry_core::testing::{FakeClock, SeqEntropy};
use gantry_store::{Appended, EventFilter, Store, StoreDeps, StoreError, TypePattern};
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
const DEVICE: &str = "dev_01JB6Z3K7R9T2X5N8Q1V4W6YZM";

// 固定种子和有限次数使失败可复现，不依赖外部随机数源。
struct Rng(u64);

impl Rng {
    fn pick(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((self.0 >> 32) as usize) % n
    }
}

#[derive(Clone, Copy, Debug)]
enum Pattern {
    Exact(&'static str),
    Prefix(&'static str),
}

impl Pattern {
    fn accepts(self, ty: &str) -> bool {
        match self {
            Self::Exact(name) => name == ty,
            Self::Prefix(prefix) => ty.starts_with(prefix),
        }
    }

    fn as_filter(self) -> TypePattern {
        match self {
            Self::Exact(name) => TypePattern::Exact(name.parse().unwrap()),
            Self::Prefix(prefix) => TypePattern::Prefix(prefix.to_owned()),
        }
    }
}

#[derive(Debug)]
struct Query {
    projects: Option<BTreeSet<ProjectId>>,
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
}

impl Query {
    fn filter(&self) -> EventFilter {
        EventFilter {
            project_ids: self.projects.clone(),
            include: self.include.iter().map(|p| p.as_filter()).collect(),
            exclude: self.exclude.iter().map(|p| p.as_filter()).collect(),
        }
    }

    fn accepts(&self, event: &Event) -> bool {
        let project_ok = match (&self.projects, event.project_id) {
            (_, None) | (None, _) => true,
            (Some(projects), Some(project)) => projects.contains(&project),
        };
        let name = event.ty.as_str();
        project_ok
            && (self.include.is_empty() || self.include.iter().any(|p| p.accepts(name)))
            && !self.exclude.iter().any(|p| p.accepts(name))
    }
}

fn new_event(ty: EventType, project: Option<ProjectId>, data: Value) -> NewEvent {
    NewEvent {
        ty,
        subject: project
            .map(|p| Subject::parse(&p.to_string()).unwrap())
            .unwrap_or_else(Subject::settings),
        project_id: project,
        actor: Actor::new(ActorKind::System, "api"),
        causation_id: None,
        correlation_id: None,
        data,
        dedup_key: None,
    }
}

fn project_event(project: ProjectId, index: usize, added: bool) -> NewEvent {
    new_event(
        if added {
            EventType::ProjectAdded
        } else {
            EventType::ProjectUpdated
        },
        Some(project),
        json!({
            "id": project, "repo": format!("acme/project-{index}"),
            "installation_id": 1, "bot_user_id": 2, "default_branch": "main",
            "allowed_merge_methods": ["squash"], "merge_queue_enabled": !added,
            "created_at": "2023-11-14T22:13:20.000Z",
            "updated_at": "2023-11-14T22:13:20.000Z"
        }),
    )
}

fn settings_event(project: Option<ProjectId>, base_rev: u64) -> NewEvent {
    let mut event = new_event(
        EventType::SettingsUpdated,
        project,
        json!({
            "scope": if project.is_some() { "project" } else { "global" },
            "resource": "levels", "schema_version": 1, "base_rev": base_rev,
            "value": {"high": {"budget": {"nodes": if project.is_some() { 20 } else { 40 }}}},
            "relaxes": [], "secret_changes": []
        }),
    );
    event.actor = Actor::new(ActorKind::Device, DEVICE);
    event
}

fn assert_settings_are_tightening(events: &[Event], projects: [ProjectId; 2]) {
    assert_eq!(DEVICE.parse::<DeviceId>().unwrap().to_string(), DEVICE);
    let defaults: Value = serde_json::from_str(include_str!(
        "../../gantry-core/src/settings/defaults/levels.json"
    ))
    .unwrap();
    let mut global = defaults["high"]["budget"]["nodes"].as_u64().unwrap();
    assert_eq!(global, 40);
    let mut overrides = [None; 2];
    let mut revisions = [0_u64; 3];
    for event in events.iter().filter(|e| e.ty == EventType::SettingsUpdated) {
        let scope = match event.project_id {
            None => 0,
            Some(p) if p == projects[0] => 1,
            Some(p) if p == projects[1] => 2,
            other => panic!("unexpected project: {other:?}"),
        };
        let nodes = event.data["value"]["high"]["budget"]["nodes"]
            .as_u64()
            .unwrap();
        assert_eq!(event.actor, Actor::new(ActorKind::Device, DEVICE));
        assert_eq!(event.data["base_rev"], json!(revisions[scope]));
        assert_eq!(event.data["relaxes"], json!([]));
        assert_eq!(
            event.data["value"],
            json!({"high": {"budget": {"nodes": if scope == 0 { 40 } else { 20 }}}})
        );
        if scope == 0 {
            assert!(nodes <= global);
            assert!(overrides.iter().flatten().all(|value| *value <= nodes));
            global = nodes;
        } else {
            assert!(nodes <= global);
            assert!(nodes <= overrides[scope - 1].unwrap_or(global));
            overrides[scope - 1] = Some(nodes);
        }
        revisions[scope] = event.seq.as_u64();
    }
    assert!(revisions.iter().all(|rev| *rev > 0));
}

async fn append(tx: &mut gantry_store::WriteTx, event: NewEvent, events: &mut Vec<Event>) {
    let expected = event.clone();
    let Appended::New(actual) = tx.append(event).await.unwrap() else {
        panic!("unexpected duplicate");
    };
    assert_eq!(actual.seq.as_u64(), events.len() as u64 + 1);
    assert_eq!(actual.ty, expected.ty);
    assert_eq!(actual.project_id, expected.project_id);
    assert_eq!(actual.subject, expected.subject);
    assert_eq!(actual.data, expected.data);
    events.push(actual);
}

async fn assert_pages(store: &Store, events: &[Event], query: &Query, after: Seq, limit: u32) {
    let head = events.last().map_or(Seq::ZERO, |event| event.seq);
    let expected: Vec<_> = events
        .iter()
        .filter(|event| event.seq > after && query.accepts(event))
        .cloned()
        .collect();
    let snapshot = store.snapshot().await.unwrap();
    let filter = query.filter();
    let mut cursor = after;
    let mut received = Vec::new();
    loop {
        // 先从完整参考结果切片，而不是调用产品的过滤或扫描函数。
        let start = received.len();
        let end = (start + limit as usize).min(expected.len());
        let wanted = &expected[start..end];
        let scanned_to = if wanted.len() == limit as usize {
            wanted.last().unwrap().seq
        } else {
            head
        };
        let page = store.events().scan(cursor, &filter, limit).await.unwrap();
        let frozen = snapshot.scan(cursor, &filter, limit).unwrap();
        assert_eq!(
            page.events, wanted,
            "query={query:?}, after={after:?}, limit={limit}"
        );
        assert_eq!((page.scanned_to, page.head), (scanned_to, head));
        assert_eq!(
            (frozen.events, frozen.scanned_to, frozen.head),
            (page.events.clone(), scanned_to, head)
        );
        received.extend(page.events);
        if scanned_to == head {
            break;
        }
        assert!(scanned_to > cursor);
        cursor = scanned_to;
    }
    assert_eq!(received, expected);
    assert!(received.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

async fn exercise(sqlite: bool, seed: u64) {
    let clock = Arc::new(FakeClock::new(NOW));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(11))));
    let deps = StoreDeps { clock, ids };
    let store = if sqlite {
        Store::open_temp(deps, &[]).await.unwrap()
    } else {
        Store::open_in_memory(deps, &[]).await.unwrap()
    };
    let projects = [
        Id::from_parts(NOW as u64, 1).unwrap(),
        Id::from_parts(NOW as u64, 2).unwrap(),
    ];
    let all = Query {
        projects: None,
        include: vec![],
        exclude: vec![],
    };
    assert_pages(&store, &[], &all, Seq::ZERO, 1).await;

    let mut rng = Rng(seed);
    let mut events = Vec::new();
    let mut revisions = [0_u64; 3];
    let mut tx = store.begin_write().await.unwrap();
    for (i, &project) in projects.iter().enumerate() {
        append(&mut tx, project_event(project, i, true), &mut events).await;
    }
    // 保证三个作用域都有设置事件，再随机交错更新。
    for step in 0..42 {
        let scope = if step < 3 { step } else { rng.pick(3) };
        if step >= 3 && rng.pick(3) == 0 {
            let index = rng.pick(2);
            append(
                &mut tx,
                project_event(projects[index], index, false),
                &mut events,
            )
            .await;
        } else {
            let project = if scope == 0 {
                None
            } else {
                Some(projects[scope - 1])
            };
            append(
                &mut tx,
                settings_event(project, revisions[scope]),
                &mut events,
            )
            .await;
            revisions[scope] = events.last().unwrap().seq.as_u64();
        }
    }
    assert_settings_are_tightening(&events, projects);
    tx.commit().await.unwrap();
    let head = events.last().unwrap().seq;
    let patterns = [
        Pattern::Exact("project.added"),
        Pattern::Exact("project.updated"),
        Pattern::Exact("settings.updated"),
        Pattern::Prefix("project."),
        Pattern::Prefix("settings."),
        Pattern::Prefix("projects."),
        Pattern::Prefix("setting."),
    ];

    // 固定边界用例避免随机选择遗漏满页、空页或全局事件。
    for projects_filter in [
        None,
        Some(BTreeSet::new()),
        Some(BTreeSet::from([projects[0]])),
        Some(BTreeSet::from([projects[1]])),
    ] {
        let query = Query {
            projects: projects_filter,
            include: vec![],
            exclude: vec![],
        };
        for limit in [1, 2, 200] {
            assert_pages(&store, &events, &query, Seq::ZERO, limit).await;
        }
    }
    for prefix in ["projects.", "setting.", "message."] {
        let query = Query {
            projects: None,
            include: vec![Pattern::Prefix(prefix)],
            exclude: vec![],
        };
        assert_pages(&store, &events, &query, Seq::ZERO, 2).await;
    }
    for query in [
        Query {
            projects: None,
            include: vec![Pattern::Prefix("project.")],
            exclude: vec![Pattern::Exact("project.added")],
        },
        Query {
            projects: None,
            include: vec![
                Pattern::Exact("project.added"),
                Pattern::Exact("settings.updated"),
            ],
            exclude: vec![Pattern::Prefix("settings.")],
        },
    ] {
        assert_pages(&store, &events, &query, Seq::ZERO, 1).await;
    }
    assert!(!TypePattern::Prefix("messages.".into()).matches(EventType::MessagePosted));
    assert!(TypePattern::Prefix("message.".into()).matches(EventType::MessagePosted));
    assert_pages(&store, &events, &all, head, 1).await;
    assert!(store.events().check_cursor(head).await.is_ok());
    for limit in [0, 201] {
        assert!(matches!(
            store.events().scan(Seq::ZERO, &all.filter(), limit).await,
            Err(StoreError::PreconditionFailed(_))
        ));
        assert!(matches!(
            store
                .snapshot()
                .await
                .unwrap()
                .scan(Seq::ZERO, &all.filter(), limit),
            Err(StoreError::PreconditionFailed(_))
        ));
    }
    let ahead = Seq::new(head.as_u64() + 1).unwrap();
    assert!(
        matches!(store.events().check_cursor(ahead).await, Err(StoreError::CursorAhead { head: actual }) if actual == head)
    );
    assert!(
        matches!(store.events().scan(ahead, &all.filter(), 1).await, Err(StoreError::CursorAhead { head: actual }) if actual == head)
    );
    assert!(
        matches!(store.snapshot().await.unwrap().scan(ahead, &all.filter(), 1), Err(StoreError::CursorAhead { head: actual }) if actual == head)
    );

    for _ in 0..120 {
        let projects_filter = match rng.pick(5) {
            0 => None,
            1 => Some(BTreeSet::new()),
            2 => Some(BTreeSet::from([projects[0]])),
            3 => Some(BTreeSet::from([projects[1]])),
            _ => Some(BTreeSet::from(projects)),
        };
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for _ in 0..rng.pick(3) {
            include.push(patterns[rng.pick(patterns.len())]);
        }
        for _ in 0..rng.pick(3) {
            exclude.push(patterns[rng.pick(patterns.len())]);
        }
        let query = Query {
            projects: projects_filter,
            include,
            exclude,
        };
        let after = Seq::from_wire(rng.pick(events.len() + 1) as u64).unwrap();
        let limit = [1, 2, 3, 7, 200][rng.pick(5)];
        assert_pages(&store, &events, &query, after, limit).await;
    }
}

#[tokio::test]
async fn sqlite_event_scan_properties() {
    for seed in [1, 0x12345678, 0xdeadbeef] {
        exercise(true, seed).await;
    }
}

#[tokio::test]
async fn memory_event_scan_properties() {
    for seed in [1, 0x12345678, 0xdeadbeef] {
        exercise(false, seed).await;
    }
}
