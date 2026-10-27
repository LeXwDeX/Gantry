//! repo.md §12.A（锁一致性）与 §12.B（工作树、写权锁）。
#![cfg(unix)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{refs, Fixture, T0};
use gantry_core::canon::GitOid;
use gantry_core::entities::{ActorKind, LockHolder, Worktree};
use gantry_core::events::{EventType, WorktreeUnlockReason, WorktreeUnlockedData};
use gantry_core::ids::DeliveryId;
use gantry_core::ports::{
    BranchName, CreateWorktree, LockError, RepoError, RepoService, WorktreeLocks,
};
use gantry_fakes::repo::{lock_conformance, FakeRepo};

const LEASE: Duration = Duration::from_secs(900);

async fn worktree(fx: &Fixture, base: GitOid) -> (Worktree, DeliveryId) {
    let delivery = fx.delivery();
    let wt = fx
        .repo
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: delivery,
                branch: BranchName::new(format!("gantry/{delivery}")),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .unwrap();
    (wt, delivery)
}

#[tokio::test]
async fn create_worktree_is_idempotent_per_delivery() {
    let fx = Fixture::new("wt-idem").await;
    let base = fx.seed().await;
    let delivery = fx.delivery();
    let req = CreateWorktree {
        project_id: fx.project,
        delivery_id: delivery,
        branch: BranchName::new("gantry/feature-1"),
        base_ref: "main".into(),
        base_sha: base,
    };
    let first = fx
        .repo
        .create_worktree(&fx.ctx(), req.clone())
        .await
        .unwrap();
    let second = fx.repo.create_worktree(&fx.ctx(), req).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(fx.events(EventType::WorktreeCreated).await.len(), 1);
    assert_eq!(fx.repo.branch_head(&first.id).await.unwrap(), base);
    assert_eq!(
        fx.repo.get_worktree(&first.id).await.unwrap(),
        Some(first.clone())
    );
    assert!(refs(&fx.mirror_dir()).contains(&format!("refs/gantry/wt/{} {base}", first.id)));
}

#[tokio::test]
async fn create_worktree_validates_branch_and_base() {
    let fx = Fixture::new("wt-validate").await;
    let base = fx.seed().await;
    for branch in [
        "feature/x",
        "gantry/../x",
        "gantry/a..b",
        "gantry/x.lock",
        "gantry/ x",
    ] {
        let result = fx
            .repo
            .create_worktree(
                &fx.ctx(),
                CreateWorktree {
                    project_id: fx.project,
                    delivery_id: fx.delivery(),
                    branch: BranchName::new(branch),
                    base_ref: "main".into(),
                    base_sha: base,
                },
            )
            .await;
        assert!(
            matches!(
                result,
                Err(RepoError::Git {
                    op: "check-ref-format",
                    ..
                })
            ),
            "{branch} must be rejected"
        );
    }
    let missing = GitOid::parse(&"2".repeat(40)).unwrap();
    let result = fx
        .repo
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: fx.delivery(),
                branch: BranchName::new("gantry/ok"),
                base_ref: "main".into(),
                base_sha: missing,
            },
        )
        .await;
    assert!(matches!(result, Err(RepoError::BaseNotInMirror { oid }) if oid == missing));
    assert!(fx.events(EventType::WorktreeCreated).await.is_empty());
}

#[tokio::test]
async fn shared_lock_conformance_passes_for_real_and_fake() {
    let fx = Fixture::new("wt-conformance").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    lock_conformance(&fx.repo, &fx.ctx(), &wt.id, &delivery, LEASE)
        .await
        .unwrap();

    // 同一份用例对 FakeRepo 也成立（repo.md §12.A）。
    let fake = FakeRepo::new(gantry_core::time::Timestamp::from_millis(T0));
    let fake_wt = fake
        .create_worktree(
            &fx.ctx(),
            CreateWorktree {
                project_id: fx.project,
                delivery_id: delivery,
                branch: BranchName::new("gantry/x"),
                base_ref: "main".into(),
                base_sha: base,
            },
        )
        .await
        .unwrap();
    lock_conformance(&fake, &fx.ctx(), &fake_wt.id, &delivery, LEASE)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_acquire_has_exactly_one_winner() {
    let fx = Fixture::new("wt-race").await;
    let base = fx.seed().await;
    let (wt, _) = worktree(&fx, base).await;
    let repo = Arc::new(fx.repo.clone());
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let repo = repo.clone();
        let ctx = fx.ctx();
        let delivery = fx.delivery();
        let id = wt.id;
        tasks.push(tokio::spawn(async move {
            repo.acquire_lock(&ctx, &id, LockHolder::Engine, &delivery, LEASE)
                .await
        }));
    }
    let mut winners = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => winners += 1,
            Err(RepoError::WorktreeLocked { .. }) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(fx.events(EventType::WorktreeLocked).await.len(), 1);
}

#[tokio::test]
async fn human_cannot_preempt_engine_and_engine_cannot_write_under_human() {
    let fx = Fixture::new("wt-human").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    let engine = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    assert!(matches!(
        fx.repo.acquire_human(&fx.ctx(), &wt.id, LEASE).await,
        Err(LockError::Held { lock }) if lock.holder == LockHolder::Engine
    ));
    fx.repo.release_lock(&fx.ctx(), &engine).await.unwrap();

    assert!(matches!(
        fx.repo
            .acquire_human(&fx.ctx(), &wt.id, Duration::from_secs(14_401))
            .await,
        Err(LockError::LeaseTooLong { .. })
    ));
    let held = fx
        .repo
        .acquire_human(&fx.ctx(), &wt.id, Duration::from_secs(1_800))
        .await
        .unwrap();
    assert_eq!(held.lock.as_ref().unwrap().holder, LockHolder::Human);
    // #122：人工锁不绑定交付单元。
    assert_eq!(held.lock.as_ref().unwrap().delivery_id, None);
    // 人工持锁时 engine 用旧令牌写入（这里是移除工作树）得到 WorktreeLocked。
    assert!(matches!(
        fx.repo.remove_worktree(&fx.ctx(), &wt.id, &engine).await,
        Err(RepoError::WorktreeLocked { lock }) if lock.holder == LockHolder::Human
    ));
    assert!(matches!(
        fx.repo
            .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
            .await,
        Err(RepoError::WorktreeLocked { .. })
    ));
    let released = fx.repo.release_human(&fx.ctx(), &wt.id).await.unwrap();
    assert_eq!(released.lock, None);
}

#[tokio::test]
async fn expired_lease_is_released_with_expired_reason() {
    let fx = Fixture::new("wt-expire").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    fx.repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    fx.clock.advance_secs(899);
    assert_eq!(fx.repo.expire_locks().await.unwrap(), 0);
    fx.clock.advance_secs(1);
    assert_eq!(fx.repo.expire_locks().await.unwrap(), 1);
    let unlocked = fx.events(EventType::WorktreeUnlocked).await;
    assert_eq!(unlocked.len(), 1);
    assert_eq!(unlocked[0].actor.kind, ActorKind::System);
    let data: WorktreeUnlockedData = serde_json::from_value(unlocked[0].data.clone()).unwrap();
    assert_eq!(data.reason, WorktreeUnlockReason::Expired);
    assert_eq!(
        fx.repo.get_worktree(&wt.id).await.unwrap().unwrap().lock,
        None
    );
}

#[tokio::test]
async fn stale_token_after_takeover_cannot_write() {
    let fx = Fixture::new("wt-fence").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    let old = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    fx.clock.advance_secs(901);
    let new = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Guardian, &delivery, LEASE)
        .await
        .unwrap();
    assert!(new.fence > old.fence, "fence is monotonic");
    let before = refs(&fx.mirror_dir());
    let events_before = fx.store.events().head().await.unwrap();
    assert!(matches!(
        fx.repo.remove_worktree(&fx.ctx(), &wt.id, &old).await,
        Err(RepoError::StaleLock)
    ));
    assert!(matches!(
        fx.repo.renew_lock(&fx.ctx(), &old, LEASE).await,
        Err(RepoError::StaleLock)
    ));
    // 旧令牌的 release 是无操作。
    fx.repo.release_lock(&fx.ctx(), &old).await.unwrap();
    assert_eq!(refs(&fx.mirror_dir()), before);
    assert_eq!(fx.store.events().head().await.unwrap(), events_before);
    assert_eq!(
        fx.repo
            .get_worktree(&wt.id)
            .await
            .unwrap()
            .unwrap()
            .lock
            .unwrap()
            .holder,
        LockHolder::Guardian
    );
}

#[tokio::test]
async fn renew_keeps_fence_and_extends_lease() {
    let fx = Fixture::new("wt-renew").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    let token = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    fx.clock.advance_secs(600);
    let renewed = fx.repo.renew_lock(&fx.ctx(), &token, LEASE).await.unwrap();
    assert_eq!(renewed.fence, token.fence);
    assert!(renewed.lease_until > token.lease_until);
    // 同 holder + 同 delivery 再次 acquire 也是续租。
    let again = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    assert_eq!(again.fence, token.fence);
    fx.clock.advance_secs(800);
    assert_eq!(fx.repo.expire_locks().await.unwrap(), 0);
}

#[tokio::test]
async fn restart_rebuilds_locks_and_expires_overdue_ones() {
    let mut fx = Fixture::new("wt-restart").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    let token = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    let renewed = fx.repo.renew_lock(&fx.ctx(), &token, LEASE).await.unwrap();

    fx.reopen().await;
    let rebuilt = fx.repo.get_worktree(&wt.id).await.unwrap().unwrap();
    let lock = rebuilt.lock.expect("lock survives restart");
    assert_eq!(lock.lease_until, renewed.lease_until);
    // 重建后的令牌仍然有效（围栏一致）。
    let again = fx
        .repo
        .renew_lock(&fx.ctx(), &renewed, LEASE)
        .await
        .unwrap();
    assert_eq!(again.fence, token.fence);
    assert_eq!(fx.repo.branch_head(&wt.id).await.unwrap(), base);

    fx.clock.advance_secs(3_600);
    fx.reopen().await;
    assert_eq!(
        fx.repo.get_worktree(&wt.id).await.unwrap().unwrap().lock,
        None
    );
    let unlocked = fx.events(EventType::WorktreeUnlocked).await;
    assert_eq!(unlocked.len(), 1, "startup appends the overdue unlock once");
    fx.reopen().await;
    assert_eq!(fx.events(EventType::WorktreeUnlocked).await.len(), 1);
}

#[tokio::test]
async fn remove_worktree_requires_lock_and_drops_branch_ref() {
    let fx = Fixture::new("wt-remove").await;
    let base = fx.seed().await;
    let (wt, delivery) = worktree(&fx, base).await;
    let fake_token = gantry_core::ports::LockToken {
        worktree_id: wt.id,
        holder: LockHolder::Engine,
        delivery_id: delivery,
        fence: gantry_core::entities::Seq::new(1).unwrap(),
        lease_until: gantry_core::time::Timestamp::from_millis(T0 + 10_000_000),
    };
    assert!(matches!(
        fx.repo
            .remove_worktree(&fx.ctx(), &wt.id, &fake_token)
            .await,
        Err(RepoError::LockRequired)
    ));
    let token = fx
        .repo
        .acquire_lock(&fx.ctx(), &wt.id, LockHolder::Engine, &delivery, LEASE)
        .await
        .unwrap();
    fx.repo
        .remove_worktree(&fx.ctx(), &wt.id, &token)
        .await
        .unwrap();
    assert_eq!(fx.repo.get_worktree(&wt.id).await.unwrap(), None);
    assert_eq!(fx.events(EventType::WorktreeRemoved).await.len(), 1);
    assert!(!refs(&fx.mirror_dir()).contains("refs/gantry/wt/"));
    assert!(matches!(
        fx.repo.branch_head(&wt.id).await,
        Err(RepoError::WorktreeNotFound)
    ));
}
