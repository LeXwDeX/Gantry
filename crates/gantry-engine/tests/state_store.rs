//! 引擎状态与 store 投影的一致性（M1 收尾评审 A1）、按路由表加载（A4）与确定性 ID。

mod common;

use std::sync::Arc;

use common::*;
use gantry_core::entities::{ActorKind, Seq};
use gantry_core::ids::{DeliveryId, IdGen, Nod, NodeId};
use gantry_core::level::Level;
use gantry_engine::decide::{DecideSeed, IdMint, InputIdentity, TickReason};
use gantry_engine::engine::Engine;
use gantry_engine::DeliveryState;
use gantry_store::testing::{FakeClock, SeqEntropy};
use gantry_store::{Store, StoreDeps};
use serde_json::json;

async fn engine() -> Engine {
    let clock = Arc::new(FakeClock::new(T0));
    let ids = Arc::new(IdGen::new(clock.clone(), Arc::new(SeqEntropy::new(7))));
    let store = Store::open_in_memory(StoreDeps { clock, ids }, &[])
        .await
        .unwrap();
    let inbox = Arc::new(gantry_fakes::inbox::FakeInbox::new_in(store.clone()));
    Engine::new(store, inbox)
}

async fn append(engine: &Engine, spec: &Spec) {
    let event = spec.to_new();
    event.validate().unwrap();
    let mut tx = engine.store().begin_write().await.unwrap();
    tx.append(event).await.unwrap();
    tx.commit().await.unwrap();
}

async fn all_events(engine: &Engine) -> Vec<gantry_core::events::Event<serde_json::Value>> {
    let filter = gantry_store::EventFilter::default();
    engine
        .store()
        .events()
        .scan(Seq::ZERO, &filter, 200)
        .await
        .unwrap()
        .events
}

/// 每写一条 `delivery.*`，store 的行、引擎从全量事件折叠的行、引擎按路由加载的行三者相同。
/// 覆盖 #48：压低上限时 effective 收紧到上限（只写 `kind = cap`）。
#[tokio::test]
async fn store_row_and_engine_state_agree_after_every_delivery_event() {
    let engine = engine().await;
    let id: DeliveryId = DELIVERY.parse().unwrap();

    async fn check(engine: &Engine, id: &DeliveryId) {
        let store_row = engine
            .store()
            .snapshot()
            .await
            .unwrap()
            .delivery(id)
            .unwrap();
        let folded = DeliveryState::fold(&all_events(engine).await)
            .unwrap()
            .unwrap()
            .delivery;
        let loaded = engine.load_state(id).await.unwrap().unwrap().delivery;
        assert_eq!(store_row, folded);
        assert_eq!(store_row, loaded);
    }

    append(&engine, &created(DELIVERY)).await;
    check(&engine, &id).await;
    // created 时 cap = Medium、effective = Low；先把 effective 升到 Medium。
    append(
        &engine,
        &spec(
            "delivery.level_changed",
            DELIVERY,
            json!({"from": "Low", "to": "Medium", "kind": "effective", "reason": "escalation"}),
        ),
    )
    .await;
    check(&engine, &id).await;
    // 压低上限：effective 派生为 min(Medium, Low) = Low。
    append(
        &engine,
        &spec(
            "delivery.level_changed",
            DELIVERY,
            json!({"from": "Medium", "to": "Low", "kind": "cap", "reason": "user"}),
        )
        .actor(ActorKind::Device),
    )
    .await;
    let row = engine.load_delivery(&id).await.unwrap().unwrap();
    assert_eq!(row.level_effective, Level::Low);
    check(&engine, &id).await;
    append(
        &engine,
        &stage("spec", "implement", Some("admission_skipped")),
    )
    .await;
    check(&engine, &id).await;
    for spec_ in [
        spec(
            "delivery.paused",
            DELIVERY,
            json!({"by": "dev", "status_before": "running"}),
        ),
        spec(
            "delivery.resumed",
            DELIVERY,
            json!({"by": "dev", "status_after": "running"}),
        ),
        spec(
            "delivery.abandoned",
            DELIVERY,
            json!({"reason": "issue_not_planned", "by": "dev"}),
        ),
    ] {
        append(&engine, &spec_.actor(ActorKind::Device)).await;
        check(&engine, &id).await;
    }
}

/// 不带 `correlation_id` 的节点 / 尝试事件仍按路由表归属；同项目的其他交付单元与
/// 其他项目的事件都不进入本交付单元的状态。
#[tokio::test]
async fn load_state_routes_by_table_not_by_correlation() {
    let engine = engine().await;
    let path = low_path();
    // 交错写入：本交付单元（不带 correlation）、同项目另一交付单元、另一项目的交付单元。
    append(&engine, &path[0]).await;
    append(&engine, &created(OTHER_DELIVERY)).await;
    append(
        &engine,
        &spec(
            "delivery.created",
            "dlv_01JB7M4P8R2X6V9N3Q5T7W1YZD",
            created(DELIVERY).data,
        )
        .project(Some(OTHER_PROJECT)),
    )
    .await;
    for spec in &path[1..12] {
        append(&engine, spec).await;
    }
    append(
        &engine,
        &workflow(OTHER_WORKFLOW, OTHER_DELIVERY, OTHER_NODE),
    )
    .await;
    append(&engine, &node("node.queued", OTHER_NODE, "queued")).await;

    let id: DeliveryId = DELIVERY.parse().unwrap();
    let mut loaded = engine.load_state(&id).await.unwrap().unwrap();
    let mut expected = DeliveryState::fold(&events(&path[..12])).unwrap().unwrap();
    // seq 与时间戳随交错写入不同；比较其余全部字段。
    assert_eq!(loaded.workflows.len(), 1);
    let attempt = &loaded.node(&ATTEMPT_NODE()).unwrap().attempts[0];
    assert_eq!(attempt.id.to_string(), ATTEMPT);
    normalize(&mut loaded);
    normalize(&mut expected);
    assert_eq!(loaded, expected);

    // 另一交付单元也能单独加载，且只看到自己的节点。
    let other = engine
        .load_state(&OTHER_DELIVERY.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let other_node: NodeId = OTHER_NODE.parse().unwrap();
    assert!(other.node(&other_node).is_some());
    assert!(other.node(&ATTEMPT_NODE()).is_none());

    // 不存在的交付单元返回 None，不报错。
    assert!(engine
        .load_state(&"dlv_01JB7M4P8R2X6V9N3Q5T7W1YZE".parse().unwrap())
        .await
        .unwrap()
        .is_none());
}

#[allow(non_snake_case)]
fn ATTEMPT_NODE() -> NodeId {
    NODE.parse().unwrap()
}

/// 抹掉随写入顺序而变的 seq / 时间戳。
fn normalize(state: &mut DeliveryState) {
    let zero = gantry_core::time::Timestamp::from_millis(0);
    state.last_seq = Seq::ZERO;
    state.delivery.stage_entered.seq = Seq::ZERO;
    state.delivery.stage_entered.ts = zero;
    for workflow in state.workflows.values_mut() {
        workflow.created_seq = Seq::ZERO;
        for node in workflow.nodes.values_mut() {
            for attempt in &mut node.attempts {
                attempt.started_at = zero;
                attempt.finished_at = attempt.finished_at.map(|_| zero);
            }
        }
    }
    for lock in state.locks.values_mut() {
        lock.fence = Seq::ZERO;
    }
}

/// `IdMint`：同一种子产生同一序列；种子或计数器不同则不同；时间部分取 `now`。
#[test]
fn id_mint_is_deterministic() {
    let delivery: DeliveryId = DELIVERY.parse().unwrap();
    let now = gantry_core::time::Timestamp::from_millis(T0);
    let seed = DecideSeed::derive(
        &delivery,
        &InputIdentity::Tick(TickReason::Settle),
        Seq::new(7).unwrap(),
    );
    let run = |seed| {
        let mut mint = IdMint::new(now, seed);
        (0..4).map(|_| mint.next_id::<Nod>()).collect::<Vec<_>>()
    };
    let first = run(seed);
    assert_eq!(first, run(seed));
    assert_eq!(
        first
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
    assert!(first.iter().all(|id| id.timestamp_ms() == T0 as u64));

    let other_seq = DecideSeed::derive(
        &delivery,
        &InputIdentity::Tick(TickReason::Settle),
        Seq::new(8).unwrap(),
    );
    let other_input = DecideSeed::derive(
        &delivery,
        &InputIdentity::Tick(TickReason::Recover),
        Seq::new(7).unwrap(),
    );
    assert_ne!(seed, other_seq);
    assert_ne!(seed, other_input);
    assert_ne!(first, run(other_seq));
}

/// #125：提议原文表区分"尚未预取"（`None`）与"确定缺失"（`Some(Missing)`）；键取自尝试状态。
#[test]
fn proposal_texts_distinguish_unfetched_from_missing() {
    use gantry_engine::decide::{ProposalMissing, ProposalRef, ProposalText, ProposalTexts};
    let mut path = low_path();
    path.truncate(11);
    let mut finished = attempt_finished();
    finished.data["accepted_open_pr"] = json!("proposal-1");
    path.push(finished);
    let state = DeliveryState::fold(&events(&path)).unwrap().unwrap();
    let attempt = &state.node(&NODE.parse().unwrap()).unwrap().attempts[0];
    let key = ProposalRef::of(attempt).unwrap();
    assert_eq!(key.proposal_id, "proposal-1");

    let mut texts = ProposalTexts::default();
    assert_eq!(texts.get(&key), None);
    texts.insert(
        key.clone(),
        ProposalText::Missing(ProposalMissing::NotInResult),
    );
    assert_eq!(
        texts.get(&key),
        Some(&ProposalText::Missing(ProposalMissing::NotInResult))
    );

    // 没有被接受的提议：无键。
    let plain = DeliveryState::fold(&events(&low_path()[..12]))
        .unwrap()
        .unwrap();
    assert!(ProposalRef::of(&plain.node(&NODE.parse().unwrap()).unwrap().attempts[0]).is_none());
}
