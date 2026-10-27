//! 每个 M0 替身的最小使用示例；HTTP 用例走真实 loopback 连接。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use gantry_core::canon::{sha256, GitOid, RepoSlug};
use gantry_core::egress::SandboxTier;
use gantry_core::entities::{
    Actor, ActorKind, DeviceRole, SandboxLimits, SandboxTrust, TokenWeights,
};
use gantry_core::ids::{AttemptId, DeliveryId, ProjectId, SandboxId, Ulid};
use gantry_core::level::{Effort, Level};
use gantry_core::ports::context::{ActorCtx, ClientProfile};
use gantry_core::ports::conversation::{ConversationService, NewUserMessage};
use gantry_core::ports::egress::{EgressControl, IssueJobToken, UsageCursor};
use gantry_core::ports::engine::{
    CiClassifyRequest, ClassifyPort, DeliveryControl, FindingClassifyRequest, FindingTriage,
    HealPort, HealRequest, HealTask, ProblemSpec, RoundId,
};
use gantry_core::ports::models::{
    EgressRoute, LedgerId, ModelProvider, ProviderError, ProviderRequest,
};
use gantry_core::ports::sandbox::{
    EgressRequest, ExecSpec, ProgressSample, SandboxService, SandboxSpec, Timers, WorkspaceMode,
    WorkspaceRequest,
};
use gantry_core::ports::worker::WorkerAdapter;
use gantry_core::settings::resource::{ProviderKind, SandboxDriver};
use gantry_core::text::UntrustedText;
use gantry_core::time::Timestamp;
use gantry_core::worker::{
    CiCategory, CiClassification, Confidence, FindingSeverity, JobSpec, ProgressStage,
};
use gantry_fakes::conversation::FakeConversation;
use gantry_fakes::egress::{default_config, EgressStub};
use gantry_fakes::engine::FakeEngine;
use gantry_fakes::github::FakeGitHub;
use gantry_fakes::models::{FakeModelProvider, FakeModelRule};
use gantry_fakes::sandbox::FakeSandboxService;
use gantry_fakes::test_ca::TestCA;
use gantry_fakes::worker::StubWorkerAdapter;
use serde_json::json;

fn ts() -> Timestamp {
    Timestamp::from_millis(1_700_000_000_000)
}

fn id<K: gantry_core::ids::IdKind>(seed: u128) -> gantry_core::ids::Id<K> {
    gantry_core::ids::Id::from_parts(1_700_000_000_000, seed).expect("valid id")
}

fn ulid(seed: u128) -> Ulid {
    Ulid::from_parts(1_700_000_000_000, seed).expect("valid ulid")
}

fn ctx() -> ActorCtx {
    ActorCtx {
        actor: Actor::new(ActorKind::User, "usr_test"),
        device: Some(id::<gantry_core::ids::Dev>(1)),
        role: Some(DeviceRole::Operator),
        client_profile: Some(ClientProfile::Desktop),
        request_id: ulid(2),
        idempotency_key: None,
        dedup_key: None,
        causation_id: None,
    }
}

fn job_spec(job_id: AttemptId, project_id: ProjectId, delivery_id: DeliveryId) -> JobSpec {
    let digest = sha256(b"job");
    let base = "9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345";
    let value = json!({
        "protocol": "gantry-worker/1",
        "job_id": job_id,
        "created_at": ts(),
        "project_id": project_id,
        "delivery_id": delivery_id,
        "workflow": {"id": id::<gantry_core::ids::Wfl>(11), "graph_rev": 1},
        "node": {"id": id::<gantry_core::ids::Nod>(12), "role": "build", "exec": "sandbox", "required": true},
        "attempt": {"n": 1},
        "context": {"level_effective": "Low", "revision_round": 0},
        "execution": {
            "agent": "claude-code",
            "tier": "worker",
            "model": "claude-test",
            "reasoning_effort": "high",
            "agent_options": {}
        },
        "task": {"title": "Test job", "instructions": "Do the work"},
        "workspace": {
            "path": "/gantry/work",
            "repo": "acme/widgets",
            "base_sha": base,
            "pr_base_sha": null
        },
        "isolation": {
            "nonce": "0123456789abcdef0123456789abcdef",
            "begin": "<<<BEGIN>>>",
            "end": "<<<END>>>"
        },
        "constraints": {
            "tools": {"edit": true, "shell": true},
            "paths": {"allow": [], "deny": []},
            "limits": {"wall_time_s": 60, "idle_timeout_s": 60, "max_turns": 5, "tokens_total": 1000},
            "patch": {"max_bytes": 1000, "max_files": 1}
        },
        "output": {
            "structured_schema": {
                "id": "gantry.worker.output.build/1",
                "path": "output.schema.json",
                "sha256": digest
            },
            "patch": "optional",
            "report": "optional",
            "proposals": "forbidden"
        }
    });
    serde_json::from_value(value).expect("valid JobSpec fixture")
}

fn sandbox_spec(job_id: AttemptId, sandbox_id: SandboxId) -> SandboxSpec {
    let project_id = id::<gantry_core::ids::Prj>(21);
    let delivery_id = id::<gantry_core::ids::Dlv>(22);
    let job = job_spec(job_id, project_id, delivery_id);
    SandboxSpec {
        job_id,
        sandbox_id,
        project_id,
        delivery_id: Some(delivery_id),
        conversation_id: None,
        job,
        raw_materials: Vec::new(),
        upstream: Vec::new(),
        exec: ExecSpec {
            image_digest: sha256(b"image"),
            argv: vec!["gantry-worker".into(), "run".into()],
            env: BTreeMap::new(),
            token_env: Default::default(),
        },
        driver: SandboxDriver::Podman,
        trust: SandboxTrust::Untrusted,
        limits: SandboxLimits {
            cpu_cores: 1.0,
            mem_mb: 128,
            disk_mb: 128,
            pids: 16,
            wall_time_s: 60,
        },
        timers: Timers {
            idle_timeout_s: 60,
            kill_grace_s: 5,
        },
        io: gantry_core::settings::resource::SandboxIo {
            progress: gantry_core::settings::resource::ProgressIo {
                max_line_bytes: 1024,
                max_lines: 10,
                max_rate_per_s: 10,
            },
            artifacts: gantry_core::settings::resource::ArtifactIo {
                result_max_bytes: 1024,
                report_max_bytes: 1024,
                proposals_max_bytes: 1024,
                proposals_max_count: 1,
            },
            materials: gantry_core::settings::resource::MaterialIo {
                max_item_bytes: 1024,
                max_total_bytes: 1024,
            },
            logs_tail_bytes: 1024,
        },
        egress: EgressRequest {
            profile: gantry_core::ports::sandbox::ProfileName::new("default"),
            groups: vec![gantry_core::ports::sandbox::GroupName::new("model_api")],
            tokens_total: 1000,
            weights: TokenWeights::default(),
        },
        workspace: WorkspaceRequest {
            repo: RepoSlug::parse("acme/widgets").expect("repo"),
            base_sha: GitOid::parse("9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345").expect("sha"),
            pr_base_sha: None,
            mode: WorkspaceMode::Rw,
        },
    }
}

fn job_token_request() -> IssueJobToken {
    IssueJobToken {
        job_id: id::<gantry_core::ids::Att>(31),
        sandbox_id: id::<gantry_core::ids::Sbx>(32),
        project_id: id::<gantry_core::ids::Prj>(33),
        delivery_id: Some(id::<gantry_core::ids::Dlv>(34)),
        conversation_id: None,
        tier: SandboxTier::Worker,
        provider: gantry_core::ports::egress::ProviderId::new("anthropic"),
        profile: gantry_core::ports::sandbox::ProfileName::new("default"),
        groups: vec![gantry_core::ports::sandbox::GroupName::new("model_api")],
        tokens_total: 10_000,
        weights: TokenWeights::default(),
    }
}

/// 发送真实 loopback HTTP 请求；返回状态、头、正文。
fn http(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, BTreeMap<String, String>, String) {
    let rest = url.strip_prefix("http://").expect("http url");
    let (authority, path) = rest.split_once('/').expect("url path");
    let mut stream = TcpStream::connect(authority).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut request = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(body.as_bytes()).expect("write body");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let (head, body) = response.split_once("\r\n\r\n").expect("http response");
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("status");
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    (status, headers, body.to_owned())
}

#[tokio::test]
async fn stub_worker_adapter_declares_claude_code_without_secrets() {
    let adapter = StubWorkerAdapter::new();
    let spec = job_spec(
        id::<gantry_core::ids::Att>(41),
        id::<gantry_core::ids::Prj>(42),
        id::<gantry_core::ids::Dlv>(43),
    );
    assert_eq!(adapter.agent_id(), "claude-code");
    assert_eq!(
        adapter.image().reference,
        "ghcr.io/gantry/worker-claude-code:0.1.0"
    );
    assert_eq!(adapter.command(), vec!["gantry-worker", "run"]);

    let env = adapter.env(&spec);
    assert!(!env.contains_key("GITHUB_TOKEN"));
    assert!(!env.contains_key("ANTHROPIC_BASE_URL"));
    assert_eq!(
        env.get("ANTHROPIC_API_KEY").map(String::as_str),
        Some("gantry-egress-managed")
    );
    assert_eq!(
        env.get("GANTRY_JOB").map(String::as_str),
        Some(spec.job_id.to_string().as_str())
    );

    let token_env = adapter.token_env();
    assert_eq!(
        token_env
            .model_base_url
            .get("anthropic")
            .map(String::as_str),
        Some("ANTHROPIC_BASE_URL")
    );
    assert!(token_env.proxy.iter().any(|name| name == "HTTPS_PROXY"));
    assert_eq!(
        adapter.capabilities().provider_kinds,
        vec![ProviderKind::Anthropic]
    );
    assert!(!adapter.capabilities().supports_fallback);
    assert!(adapter.validate_agent_options(&json!({})).is_ok());
    let error = adapter
        .validate_agent_options(&json!({"tools": {"shell": true}}))
        .expect_err("unknown options are rejected");
    assert!(matches!(
        error,
        gantry_core::ports::worker::AdapterError::InvalidAgentOptions { .. }
    ));
}

#[tokio::test]
async fn fake_model_provider_returns_scripted_errors_and_default_text() {
    let provider = FakeModelProvider::new().with_rule(
        FakeModelRule::error(ProviderError::RateLimited { retry_after: None }).for_model("limited"),
    );
    let route = EgressRoute {
        base_url: "http://127.0.0.1:1/s/token/anthropic".into(),
        tier: gantry_core::entities::Tier::Utility,
        call_id: ulid(51),
        ledger: LedgerId::unattributed(),
    };
    let request = |model: &str| ProviderRequest {
        model: model.into(),
        system: "system".into(),
        input: "input".into(),
        effort: gantry_core::ports::models::ProviderEffort::new("low"),
        output_schema: None,
        max_output_tokens: 16,
        request_timeout: Duration::from_secs(1),
        call_id: ulid(52),
    };
    let error = provider
        .complete(request("limited"), &route)
        .await
        .expect_err("scripted error");
    assert!(matches!(error, ProviderError::RateLimited { .. }));
    let response = provider
        .complete(request("default"), &route)
        .await
        .expect("default response");
    assert_eq!(response.text, "fake model response");
    assert_eq!(provider.calls().len(), 2);
    assert_eq!(
        provider
            .map_effort("default", Effort::High)
            .expect("effort")
            .as_str(),
        "high"
    );
}

#[tokio::test]
async fn fake_conversation_ensure_is_idempotent_and_scripts_messages() {
    let fake = FakeConversation::new();
    let project = id::<gantry_core::ids::Prj>(61);
    let ctx = ctx();
    let first = fake
        .ensure_conversation(&project, &ctx)
        .await
        .expect("ensure");
    let second = fake
        .ensure_conversation(&project, &ctx)
        .await
        .expect("ensure");
    assert_eq!(first, second);

    let message = fake
        .post_user_message(
            ctx.clone(),
            first,
            NewUserMessage {
                text: "hello".into(),
                level_cap: Some(Level::Low),
                refs: Vec::new(),
            },
        )
        .await
        .expect("post");
    assert_eq!(message.text, "hello");
    let mut scripted = message.clone();
    scripted.text = "scripted reply".into();
    fake.push_scripted_message(first, scripted);
    let reply = fake
        .post_user_message(
            ctx,
            first,
            NewUserMessage {
                text: "ignored".into(),
                level_cap: None,
                refs: Vec::new(),
            },
        )
        .await
        .expect("post");
    assert_eq!(reply.text, "scripted reply");

    let page = fake
        .list_messages(
            &first,
            gantry_core::ports::context::PageQuery {
                cursor: None,
                limit: 10,
            },
        )
        .await
        .expect("list");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.last_seq.as_u64(), 2);
    assert_eq!(fake.calls().len(), 5);
}

#[tokio::test]
async fn fake_sandbox_service_replays_and_is_idempotent() {
    let fake = FakeSandboxService::new();
    let job_id = id::<gantry_core::ids::Att>(71);
    let sandbox_id = id::<gantry_core::ids::Sbx>(72);
    let spec = sandbox_spec(job_id, sandbox_id);
    let outcome = FakeSandboxService::success_outcome(job_id, sandbox_id);
    fake.script(spec.clone(), outcome.clone());
    let ctx = ctx();
    let first = fake.submit(&ctx, spec.clone()).await.expect("submit");
    let second = fake.submit(&ctx, spec.clone()).await.expect("submit again");
    assert_eq!(first, second);
    assert_eq!(fake.wait(&job_id).await.expect("wait"), outcome);
    assert_eq!(fake.wait(&job_id).await.expect("wait again"), outcome);
    let mut other = spec.clone();
    other.project_id = id::<gantry_core::ids::Prj>(999);
    assert!(matches!(
        fake.submit(&ctx, other).await,
        Err(gantry_core::ports::sandbox::SandboxError::InvalidSpec(_))
    ));
    let unscripted = sandbox_spec(
        id::<gantry_core::ids::Att>(74),
        id::<gantry_core::ids::Sbx>(75),
    );
    assert!(matches!(
        fake.submit(&ctx, unscripted).await,
        Err(gantry_core::ports::sandbox::SandboxError::InvalidSpec(_))
    ));
    fake.cancel(&ctx, &job_id).await.expect("cancel");
    fake.cancel(&ctx, &job_id).await.expect("cancel again");
    fake.release(&ctx, &job_id).await.expect("release");
    fake.release(&ctx, &job_id).await.expect("release again");

    fake.push_progress(ProgressSample {
        job_id,
        seq: 1,
        stage: ProgressStage::Init,
        detail: UntrustedText::new("started".into(), "fake".into(), false),
        at: ts(),
    });
    let mut progress = fake.progress(&job_id);
    let sample = std::future::poll_fn(|cx| progress.as_mut().poll_next(cx))
        .await
        .expect("progress sample");
    assert_eq!(sample.seq, 1);

    let recovered = fake
        .recover(
            &ctx,
            &[gantry_core::ports::sandbox::UnfinishedAttempt {
                job_id,
                sandbox_id,
                project_id: id::<gantry_core::ids::Prj>(73),
            }],
        )
        .await
        .expect("recover");
    assert_eq!(recovered, vec![first]);
    let report = fake
        .sweep_orphans(&ctx, &std::collections::BTreeSet::new())
        .await
        .expect("sweep");
    assert_eq!(report.destroyed, 0);
    assert_eq!(fake.capacity().await.running, 0);
}

#[tokio::test]
async fn fake_engine_records_context_and_scripts_classify_and_heal() {
    let fake = FakeEngine::new();
    let ctx = ctx();
    let classification = CiClassification {
        category: CiCategory::Test,
        in_pr_scope: true,
        confidence: Confidence::High,
    };
    fake.script_ci(classification.clone());
    let request = CiClassifyRequest {
        project_id: id::<gantry_core::ids::Prj>(81),
        delivery_id: id::<gantry_core::ids::Dlv>(82),
        attempt_id: id::<gantry_core::ids::Att>(83),
        check_run_id: Some(7),
        materials: Vec::new(),
    };
    let actual = fake.classify_ci(request, &ctx).await.expect("classify");
    assert_eq!(actual, classification);
    let calls = fake.calls();
    assert_eq!(calls[0].op(), "classify_ci");

    let triage = FindingTriage {
        security: false,
        design_or_api: true,
        actionable: true,
        confidence: Confidence::Medium,
        severity: Some(FindingSeverity::Minor),
    };
    fake.script_finding(triage);
    let actual = fake
        .classify_finding(
            FindingClassifyRequest {
                project_id: id::<gantry_core::ids::Prj>(81),
                delivery_id: id::<gantry_core::ids::Dlv>(82),
                attempt_id: id::<gantry_core::ids::Att>(83),
                thread_node_id: None,
                materials: Vec::new(),
            },
            &ctx,
        )
        .await
        .expect("finding");
    assert_eq!(actual, triage);

    let request = HealRequest {
        round_id: RoundId::new("round-1"),
        delivery_id: id::<gantry_core::ids::Dlv>(82),
        pr: gantry_core::ports::engine::PrRef {
            repo: RepoSlug::parse("acme/widgets").expect("repo"),
            number: 1,
        },
        head_sha: GitOid::parse("9f2c4e7a1b3d5f60718293a4b5c6d7e8f9012345").expect("sha"),
        task: HealTask::CiFailure,
        problems: vec![ProblemSpec {
            fingerprint: gantry_core::ports::engine::Fingerprint::new("fp"),
            kind: "ci".into(),
            material_refs: Vec::new(),
        }],
        scope_allow: Vec::new(),
        targets: Vec::new(),
        materials: Vec::new(),
        config_rev: 1,
    };
    let ticket = fake.request_heal(request, &ctx).await.expect("heal");
    assert_eq!(ticket.round_id.as_str(), "round-1");
    fake.script_heal_outcome(None);
    assert_eq!(fake.outcome(&ticket).await.expect("outcome"), None);
    fake.cancel(
        &ticket,
        gantry_core::ports::engine::CancelWhy::Shutdown,
        &ctx,
    )
    .await
    .expect("cancel");

    let missing = fake
        .set_level_cap(ctx.clone(), id::<gantry_core::ids::Dlv>(82), Level::Low)
        .await
        .expect_err("unscripted delivery call fails");
    assert!(matches!(
        missing,
        gantry_core::ports::context::ModuleError::NotFound
    ));
    assert!(matches!(fake.calls().last(), Some(call) if call.op() == "set_level_cap"));
}

#[tokio::test]
async fn egress_stub_issues_reissues_revokes_and_meters_real_http() {
    let mut egress = EgressStub::with_default_config();
    let ctx = ctx();
    let request = job_token_request();
    let grant = egress
        .issue_job_token(request.clone(), &ctx)
        .await
        .expect("issue");
    assert!(!grant.reissued);
    let old_url = grant
        .model_base_urls
        .get("anthropic")
        .expect("model url")
        .to_owned();
    let body = json!({"model":"claude-test","input":"hello"}).to_string();
    let (status, _, response) = http("POST", &old_url, &[], &body);
    assert_eq!(status, 200);
    assert!(response.contains("fake egress response"));
    let usage = egress.job_usage(&request.job_id).await.expect("usage");
    assert_eq!(usage.usage.requests, 1);
    assert!(usage.weighted_tokens > 0);

    let reissued = egress
        .issue_job_token(request.clone(), &ctx)
        .await
        .expect("reissue");
    assert!(reissued.reissued);
    let new_url = reissued
        .model_base_urls
        .get("anthropic")
        .expect("model url")
        .to_owned();
    let (status, _, _) = http("POST", &old_url, &[], &body);
    assert_eq!(status, 403);
    let (status, _, _) = http("POST", &new_url, &[], &body);
    assert_eq!(status, 200);
    let usage = egress.job_usage(&request.job_id).await.expect("usage");
    assert_eq!(usage.usage.requests, 2, "reissue preserves usage");

    let revoke = egress
        .revoke_job_token(&request.job_id, &ctx)
        .await
        .expect("revoke");
    assert_eq!(revoke.state, gantry_core::egress::JobTokenState::Revoked);
    let revoke_again = egress
        .revoke_job_token(&request.job_id, &ctx)
        .await
        .expect("revoke again");
    assert_eq!(revoke, revoke_again);
    let (status, _, body_text) = http("POST", &new_url, &[], &body);
    assert_eq!(status, 403);
    assert!(body_text.contains("token_invalid"));
    let conflict = egress.issue_job_token(request.clone(), &ctx).await;
    assert!(matches!(
        conflict,
        Err(gantry_core::ports::egress::EgressError::Conflict { .. })
    ));

    let svc = egress.service_token(&ctx).await.expect("service token");
    let svc_url = svc
        .model_base_urls
        .get("anthropic")
        .expect("svc url")
        .to_owned();
    let call_id = ulid(91).to_string();
    let headers = [
        ("X-Gantry-Ledger", "unattributed"),
        ("x-gantry-tier", "utility"),
        ("x-gantry-call-id", call_id.as_str()),
    ];
    let (status, _, _) = http("POST", &svc_url, &headers, &body);
    assert_eq!(status, 200);
    let before = egress.usage_records().len();
    let (status, _, _) = http("POST", &svc_url, &[], &body);
    assert_eq!(status, 400);
    assert_eq!(
        egress.usage_records().len(),
        before,
        "rejected request is not metered"
    );
    let page = egress
        .usage_since(UsageCursor::new("0"), 10)
        .await
        .expect("usage page");
    assert!(!page.items.is_empty());
    let page_again = egress
        .usage_since(UsageCursor::new("0"), 10)
        .await
        .expect("usage page again");
    assert_eq!(page, page_again);
    let call = egress
        .call_usage(&ulid(91))
        .await
        .expect("call usage")
        .clone();
    assert_eq!(call.usage.requests, 1);
    assert_eq!(
        call.usage_source,
        gantry_core::ports::models::UsageSource::Metered
    );

    let rotated = egress.rotate_service_token(&ctx).await.expect("rotate");
    assert_eq!(rotated.revoked_previous, vec![svc.svc_token_id.clone()]);
    let (status, _, _) = http("POST", &svc_url, &headers, &body);
    assert_eq!(status, 403);
    let new_svc_url = rotated
        .model_base_urls
        .get("anthropic")
        .expect("new svc url")
        .to_owned();
    let (status, _, _) = http("POST", &new_svc_url, &headers, &body);
    assert_eq!(status, 200);
    let old_usage = egress
        .svc_token_usage(&svc.svc_token_id, None)
        .await
        .expect("svc usage");
    assert_eq!(old_usage.items.len(), 1);

    let staged = egress
        .stage_secret(
            gantry_core::ports::egress::StageSecret {
                secret_ref: gantry_core::secret::SecretName::parse("model.anthropic.main")
                    .expect("ref"),
                value: gantry_core::secret::SecretString::new("test-value".into()),
            },
            &ctx,
        )
        .await
        .expect("stage");
    let again = egress
        .stage_secret(
            gantry_core::ports::egress::StageSecret {
                secret_ref: gantry_core::secret::SecretName::parse("model.anthropic.main")
                    .expect("ref"),
                value: gantry_core::secret::SecretString::new("test-value".into()),
            },
            &ctx,
        )
        .await
        .expect("stage replay");
    assert_eq!(staged, again);
    let conflict = egress
        .stage_secret(
            gantry_core::ports::egress::StageSecret {
                secret_ref: gantry_core::secret::SecretName::parse("model.anthropic.main")
                    .expect("ref"),
                value: gantry_core::secret::SecretString::new("different-value".into()),
            },
            &ctx,
        )
        .await;
    assert!(matches!(
        conflict,
        Err(gantry_core::ports::egress::EgressError::Conflict { .. })
    ));
    egress
        .activate_secret(&staged, &ctx)
        .await
        .expect("activate");
    let present = egress
        .secret_present(&[
            gantry_core::secret::SecretName::parse("model.anthropic.main").expect("ref"),
        ])
        .await
        .expect("present");
    assert_eq!(present.values().next(), Some(&true));
    let stale = egress.apply_config(default_config(0), &ctx).await;
    assert!(matches!(
        stale,
        Err(gantry_core::ports::egress::EgressError::Conflict { .. })
    ));
    let mut invalid = default_config(99);
    invalid
        .providers
        .get_mut("anthropic")
        .expect("provider")
        .base_url = "https://api.github.com".into();
    let invalid = egress.apply_config(invalid, &ctx).await;
    assert!(matches!(
        invalid,
        Err(gantry_core::ports::egress::EgressError::InvalidConfig { .. })
    ));
    egress
        .forget_job(&request.job_id, &ctx)
        .await
        .expect("forget");
    assert!(egress.job_usage(&request.job_id).await.is_err());
    egress.shutdown();
}

#[tokio::test]
async fn egress_stub_caps_and_rejects_unknown_tokens() {
    let mut egress = EgressStub::with_default_config();
    let ctx = ctx();
    let mut request = job_token_request();
    request.tokens_total = 1;
    let grant = egress
        .issue_job_token(request.clone(), &ctx)
        .await
        .expect("issue");
    let url = grant
        .model_base_urls
        .get("anthropic")
        .expect("url")
        .to_owned();
    let (status, _, _) = http("POST", &url, &[], "{}");
    assert_eq!(status, 200);
    let (status, _, body) = http("POST", &url, &[], "{}");
    assert_eq!(status, 403);
    assert!(body.contains("budget_exhausted"));
    let usage = egress.job_usage(&request.job_id).await.expect("usage");
    assert!(usage.capped);
    let unknown = format!("{}/j/gjt1_unknown/anthropic", egress.data_plane_url());
    let (status, _, body) = http("POST", &unknown, &[], "{}");
    assert_eq!(status, 403);
    assert!(body.contains("token_invalid"));
    let revoke = egress
        .revoke_job_token(&request.job_id, &ctx)
        .await
        .expect("revoke");
    assert!(revoke.capped);
    egress.shutdown();
}

#[tokio::test]
async fn egress_stub_rejects_expired_job_tokens() {
    let mut egress = EgressStub::with_default_config();
    egress.set_job_token_ttl(Duration::from_millis(1));
    let ctx = ctx();
    let grant = egress
        .issue_job_token(job_token_request(), &ctx)
        .await
        .expect("issue");
    let url = grant
        .model_base_urls
        .get("anthropic")
        .expect("url")
        .to_owned();
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (status, _, body) = http("POST", &url, &[], "{}");
    assert_eq!(status, 403);
    assert!(body.contains("token_invalid"));
    egress.shutdown();
}

#[tokio::test]
async fn egress_stub_rejects_expired_service_tokens() {
    let mut egress = EgressStub::with_default_config();
    egress.set_svc_token_ttl(Duration::from_millis(1));
    let ctx = ctx();
    let grant = egress.service_token(&ctx).await.expect("service token");
    let url = grant
        .model_base_urls
        .get("anthropic")
        .expect("url")
        .to_owned();
    let call_id = ulid(92).to_string();
    let headers = [
        ("X-Gantry-Ledger", "unattributed"),
        ("x-gantry-tier", "utility"),
        ("x-gantry-call-id", call_id.as_str()),
    ];
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (status, _, body) = http("POST", &url, &headers, "{}");
    assert_eq!(status, 403);
    assert!(body.contains("token_invalid"));
    egress.shutdown();
}

fn svc_headers(call_id: &str) -> [(&'static str, &str); 3] {
    [
        ("X-Gantry-Ledger", "unattributed"),
        ("x-gantry-tier", "utility"),
        ("x-gantry-call-id", call_id),
    ]
}

#[tokio::test]
async fn egress_stub_closes_calls_with_read_only_terminal_proof() {
    use gantry_core::egress::{records_digest, CallTerminalState};
    use gantry_core::ports::egress::EgressError;

    let mut egress = EgressStub::with_default_config();
    let ctx = ctx();
    let first = egress.service_token(&ctx).await.expect("service token");
    let first_url = first
        .model_base_urls
        .get("anthropic")
        .expect("url")
        .to_owned();
    let call = ulid(93);
    let call_text = call.to_string();
    for _ in 0..2 {
        let (status, _, _) = http("POST", &first_url, &svc_headers(&call_text), "{}");
        assert_eq!(status, 200);
    }

    // 未关闭：terminal 缺省，不得结算。
    let open = egress.call_terminal(&call).await.expect("statement");
    assert_eq!(open.items.len(), 2);
    assert!(open.terminal.is_none());
    assert!(open.settleable_terminal().is_none());
    assert!(open.items.iter().all(|record| record.weight_rev == 1));

    // 轮换后，新 token 与该调用无关联 → 409；旧（已吊销）token 仍可关闭。
    let second = egress.rotate_service_token(&ctx).await.expect("rotate");
    assert!(matches!(
        egress.close_call(&second.svc_token_id, &call, &ctx).await,
        Err(EgressError::Conflict { .. })
    ));
    let closed = egress
        .close_call(&first.svc_token_id, &call, &ctx)
        .await
        .expect("close");
    assert_eq!(closed.call_id, call);
    let terminal = closed.terminal.clone();
    assert_eq!(terminal.state, CallTerminalState::Closed);
    assert_eq!(terminal.record_count, 2);
    assert_eq!(terminal.svc_token_ids, vec![first.svc_token_id.clone()]);
    assert!(!terminal.incomplete);

    // 重复关闭幂等：同一份 terminal。
    let again = egress
        .close_call(&first.svc_token_id, &call, &ctx)
        .await
        .expect("close again");
    assert_eq!(again, closed);

    // 关闭后再计量被拒：403 token_invalid，且无新记录。
    let second_url = second
        .model_base_urls
        .get("anthropic")
        .expect("url")
        .to_owned();
    let before = egress.usage_records().len();
    let (status, _, body) = http("POST", &second_url, &svc_headers(&call_text), "{}");
    assert_eq!(status, 403);
    assert!(body.contains("token_invalid"));
    assert_eq!(egress.usage_records().len(), before);

    // 只读、可重复读取，且证明与明细一致。
    let read_once = egress.call_terminal(&call).await.expect("read");
    let read_twice = egress.call_terminal(&call).await.expect("read again");
    assert_eq!(read_once, read_twice);
    assert_eq!(read_once.terminal.as_ref(), Some(&terminal));
    terminal.verify(call, &read_once.items).expect("proof verifies");
    assert_eq!(read_once.settleable_terminal(), Some(&terminal));
    let mut tampered = read_once.items.clone();
    tampered[0].weight_rev += 1;
    assert!(terminal.verify(call, &tampered).is_err());
    assert!(terminal.verify(call, &read_once.items[..1]).is_err());

    // 零接收：任一签发过的 token 都可关闭，摘要为 `[]` 的摘要。
    let silent = ulid(94);
    let zero = egress
        .close_call(&second.svc_token_id, &silent, &ctx)
        .await
        .expect("close silent call");
    assert_eq!(zero.terminal.record_count, 0);
    assert_eq!(zero.terminal.last_seq, 0);
    assert!(zero.terminal.svc_token_ids.is_empty());
    assert_eq!(
        zero.terminal.records_digest,
        records_digest(&[]).expect("digest")
    );

    // 中断记录 → incomplete；过保留期 → 仍 closed，但 incomplete，明细为空。
    let interrupted = ulid(95);
    let interrupted_text = interrupted.to_string();
    let (status, _, _) = http("POST", &second_url, &svc_headers(&interrupted_text), "{}");
    assert_eq!(status, 200);
    egress.interrupt_call_records(&interrupted);
    let partial = egress
        .close_call(&second.svc_token_id, &interrupted, &ctx)
        .await
        .expect("close interrupted");
    assert!(partial.terminal.incomplete);
    let statement = egress.call_terminal(&interrupted).await.expect("read");
    assert!(statement.settleable_terminal().is_none());

    egress.expire_call_records(&call);
    let expired = egress.call_terminal(&call).await.expect("read expired");
    assert!(expired.items.is_empty());
    let expired_terminal = expired.terminal.as_ref().expect("tombstone kept");
    assert_eq!(expired_terminal.state, CallTerminalState::Closed);
    assert!(expired_terminal.incomplete);
    assert!(expired.settleable_terminal().is_none());

    // 从未签发的路径 token → NotFound。
    let unknown = gantry_core::egress::SvcTokenId::parse("svt_01JBX5A2C4E6G8J1L3N5Q7S9ZZ")
        .expect("id");
    assert!(matches!(
        egress.close_call(&unknown, &ulid(96), &ctx).await,
        Err(EgressError::NotFound)
    ));
    egress.shutdown();
}

fn openssl_supports_explicit_dates() -> bool {
    let Ok(output) = Command::new("openssl").args(["x509", "-help"]).output() else {
        return false;
    };
    // 3.x 的 -help 写 stderr；同时读两路，避免版本差异。
    let help = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    help.contains("-not_before") && help.contains("-not_after")
}

#[test]
fn test_ca_issues_certificates_and_signs_with_a_separate_p256_key() {
    let ca = TestCA::new().expect("openssl must be available for TestCA");
    assert!(ca.root_cert_pem().contains("BEGIN CERTIFICATE"));
    assert!(ca.server_cert_pem().contains("BEGIN CERTIFICATE"));
    assert_eq!(
        ca.device_chain_pem().matches("BEGIN CERTIFICATE").count(),
        3
    );
    assert!(ca.device_chain_pem().contains(ca.root_cert_pem()));
    assert!(ca.device_chain_pem().contains(ca.intermediate_cert_pem()));
    assert_eq!(ca.approval_public_key().len(), 87);
    assert!(ca
        .approval_public_key()
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
    let signature = ca.sign_approval(b"approval message").expect("sign");
    assert_eq!(signature.len(), 64);
    assert!(ca
        .verify_approval(b"approval message", &signature)
        .expect("verify"));
    let mut tampered = signature;
    tampered[0] ^= 1;
    assert!(!ca
        .verify_approval(b"approval message", &tampered)
        .expect("verify tampered"));
    assert!(!ca
        .verify_approval(b"other", &signature)
        .expect("verify wrong message"));
    assert_eq!(ca.root_fingerprint(), ca.ca_fingerprint());
    assert!(ca
        .device_pubkey_fingerprint()
        .expect("spki")
        .to_string()
        .starts_with("sha256:"));

    // shortcut: -not_before/-not_after 需要 OpenSSL 3.4+；Ubuntu 24.04 的 3.0
    // 不支持任意起止日期，该分支在目标 CI 跳过并输出原因，不伪造通过。
    if openssl_supports_explicit_dates() {
        let expired = ca.issue_expired_device().expect("expired");
        let future = ca.issue_not_yet_valid_device().expect("not yet valid");
        assert_ne!(expired.cert_pem, future.cert_pem);
    } else {
        eprintln!(
            "skip explicit-date device certs: openssl lacks -not_before/-not_after (needs 3.4+)"
        );
    }
    let foreign = ca.issue_foreign_ca_device().expect("foreign");
    let server_only = ca.issue_server_auth_only_device().expect("server auth");
    assert_ne!(foreign.cert_pem, server_only.cert_pem);
    assert!(ca.root_dir().starts_with(std::env::temp_dir()));
    let root_dir = ca.root_dir().to_path_buf();
    drop(ca);
    assert!(!root_dir.exists(), "TestCA cleans its temporary directory");
}

#[test]
fn fake_github_serves_rest_graphql_webhooks_and_scripted_429() {
    let mut github = FakeGitHub::start();
    let base = github.base_url();
    let (status, _, body) = http("GET", &format!("{base}/repos/acme/widgets"), &[], "");
    assert_eq!(status, 200);
    assert!(body.contains("acme/widgets"));

    let (status, _, body) = http(
        "POST",
        &format!("{base}/repos/acme/widgets/pulls"),
        &[("Content-Type", "application/json")],
        &json!({"title":"Fix","head":"gantry/fix","base":"main"}).to_string(),
    );
    assert_eq!(status, 201);
    assert!(body.contains("\"number\":1"));
    let (status, _, _) = http(
        "GET",
        &format!("{base}/repos/acme/widgets/pulls/1"),
        &[],
        "",
    );
    assert_eq!(status, 200);

    let (status, _, body) = http(
        "POST",
        &format!("{base}/graphql"),
        &[("Content-Type", "application/json")],
        &json!({"query":"{ viewer { login } }"}).to_string(),
    );
    assert_eq!(status, 200);
    assert!(body.contains("gantry-test-bot"));

    let (listener, received) = start_webhook_target();
    github.register_webhook_with_secret(format!("http://{listener}/hook"), "test-secret");
    let deliveries = github
        .deliver_webhook("pull_request", &json!({"action":"opened"}))
        .expect("deliver webhook");
    assert_eq!(deliveries[0].status, 202);
    let (headers, body) = received
        .recv_timeout(Duration::from_secs(2))
        .expect("webhook received");
    assert_eq!(
        headers.get("x-github-event").map(String::as_str),
        Some("pull_request")
    );
    let signature = headers
        .get("x-hub-signature-256")
        .expect("webhook signature");
    assert_eq!(signature.len(), 71);
    assert!(signature
        .strip_prefix("sha256=")
        .is_some_and(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit())));
    assert!(body.contains(r#""action":"opened""#));

    github.queue_failure(429);
    let (status, headers, _) = http("GET", &format!("{base}/repos/acme/widgets"), &[], "");
    assert_eq!(status, 429);
    assert_eq!(headers.get("retry-after").map(String::as_str), Some("1"));
    github.queue_failure(500);
    let (status, _, _) = http("GET", &format!("{base}/repos/acme/widgets"), &[], "");
    assert_eq!(status, 500);
    assert!(github
        .recorded_calls()
        .iter()
        .any(|call| call.status == 429));
    github.shutdown();
}

type CapturedWebhook = (BTreeMap<String, String>, String);

fn start_webhook_target() -> (String, mpsc::Receiver<CapturedWebhook>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("webhook listener");
    let addr = listener.local_addr().expect("webhook addr");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let head_end = buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap_or(buffer.len());
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let mut headers = BTreeMap::new();
            for line in head.split("\r\n").skip(1) {
                if let Some((name, value)) = line.split_once(':') {
                    headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
                }
            }
            let content_length = headers
                .get("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = head_end + 4;
            while buffer.len() < body_start + content_length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let body = String::from_utf8_lossy(
                buffer
                    .get(body_start..body_start + content_length)
                    .unwrap_or_default(),
            )
            .to_string();
            let _ = sender.send((headers, body));
            let response =
                b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(response);
        }
    });
    (addr.to_string(), receiver)
}
