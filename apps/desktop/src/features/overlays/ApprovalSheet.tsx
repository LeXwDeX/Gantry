import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type {
  Approval,
  ApprovalTarget,
  PullRequestRef,
} from "@/api/types";
import type { ApprovalSheetModel } from "@/api/shell";
import type { DesktopController } from "@/store/controller";
import type { DesktopState } from "@/store/state";
import { Modal } from "@/a11y/Modal";
import { isScenarioAdapter } from "@/ipc";
import { approvalStatus, shortSha, signButtonLabel } from "@/store/sign";
import { UntrustedText } from "@/render";
import { ScenarioMark } from "./ScenarioMark";
import {
  formatDateTime,
  formatRemaining,
  ipcErrorText,
  parseApiFailure,
} from "./settingsData";

export interface ApprovalSheetProps {
  controller: DesktopController;
  state: DesktopState;
  approvalId: string;
  onClose: () => void;
}

export function ApprovalSheet({
  controller,
  state,
  approvalId,
  onClose,
}: ApprovalSheetProps): ReactNode {
  const [sheet, setSheet] = useState<ApprovalSheetModel | null>(null);
  const [remoteApproval, setRemoteApproval] = useState<Approval | null>(null);
  const [approvalReady, setApprovalReady] = useState(false);
  const [approvalError, setApprovalError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const approval = state.approvals[approvalId] ?? remoteApproval;
  const [prepareError, setPrepareError] = useState<string | null>(null);
  const [changed, setChanged] = useState(false);
  const [signing, setSigning] = useState(false);
  const [signError, setSignError] = useState<string | null>(null);
  const [signDone, setSignDone] = useState<string | null>(null);
  const [handleSpent, setHandleSpent] = useState(false);
  const [denyBusy, setDenyBusy] = useState(false);
  const [denyError, setDenyError] = useState<string | null>(null);
  const [linkError, setLinkError] = useState<string | null>(null);
  const [approverName, setApproverName] = useState<string | null>(null);
  const [nowTick, setNowTick] = useState(() => Date.now());
  const preparedRef = useRef<string | null>(null);
  const aliveRef = useRef(true);
  const prepareSeq = useRef(0);
  const startedRef = useRef(false);
  const ignoreChangeRef = useRef(false);
  const approvalKeyRef = useRef("");

  const approvalKey = approval
    ? JSON.stringify([
        approval.id,
        approval.status,
        approval.action,
        approval.target,
        approval.subject_digest,
        approval.device_id,
        approval.delivery_id,
        approval.expires_at,
      ])
    : "";
  const previousKey = useRef(approvalKey);
  approvalKeyRef.current = approvalKey;

  const discardPrepared = useCallback((): void => {
    const preparedId = preparedRef.current;
    preparedRef.current = null;
    if (preparedId) {
      void controller.ipc.approvalDiscard({ preparedId }).catch(() => {
        // 丢弃句柄是尽力而为；错误不回灌到界面。
      });
    }
  }, [controller]);

  const prepare = useCallback(async (): Promise<void> => {
    setLoading(true);
    setPrepareError(null);
    setChanged(false);
    setSignError(null);
    setSignDone(null);
    setHandleSpent(false);
    discardPrepared();
    setSheet(null);
    const requestedKey = approvalKeyRef.current;
    const seq = ++prepareSeq.current;
    try {
      const result = await controller.approvalPrepare(approvalId);
      if (
        !aliveRef.current ||
        seq !== prepareSeq.current ||
        approvalKeyRef.current !== requestedKey
      ) {
        void controller.ipc
          .approvalDiscard({ preparedId: result.prepared_id })
          .catch(() => {
            // 组件已卸载、重新载入或审批已变化；丢弃迟到的句柄。
          });
        if (aliveRef.current && seq === prepareSeq.current) setChanged(true);
        return;
      }
      preparedRef.current = result.prepared_id;
      setSheet(result);
    } catch (failure) {
      if (aliveRef.current && seq === prepareSeq.current) {
        setPrepareError(ipcErrorText(failure));
      }
    } finally {
      if (aliveRef.current && seq === prepareSeq.current) setLoading(false);
    }
  }, [approvalId, controller, discardPrepared]);

  useEffect(() => {
    if (state.approvals[approvalId]) {
      setApprovalError(null);
      setApprovalReady(true);
      return;
    }
    let alive = true;
    void (async () => {
      try {
        const result = await controller.ipc.api("approvals.get", { approvalId });
        if (!alive) return;
        if (result.ok && result.body) {
          setRemoteApproval(result.body);
        } else if (!result.ok) {
          setApprovalError(parseApiFailure(result));
        }
      } catch (failure) {
        if (alive) setApprovalError(ipcErrorText(failure));
      } finally {
        if (alive) setApprovalReady(true);
      }
    })();
    return () => {
      alive = false;
    };
  }, [approvalId, controller, state.approvals]);

  useEffect(() => {
    if (!approvalReady) return;
    startedRef.current = true;
    void prepare();
  }, [approvalReady, prepare]);

  useEffect(() => {
    aliveRef.current = true;
    return () => {
      aliveRef.current = false;
      prepareSeq.current += 1;
      discardPrepared();
    };
  }, [discardPrepared]);

  useEffect(() => {
    if (previousKey.current === approvalKey) return;
    const wasEmpty = previousKey.current === "";
    previousKey.current = approvalKey;
    if (wasEmpty || !startedRef.current) return;
    if (ignoreChangeRef.current) {
      ignoreChangeRef.current = false;
      return;
    }
    if (!preparedRef.current && !sheet) {
      setChanged(true);
      return;
    }
    discardPrepared();
    setSheet(null);
    setChanged(true);
    setHandleSpent(false);
    setSignError(null);
    setSignDone(null);
  }, [approvalKey, discardPrepared, sheet]);

  useEffect(() => {
    const timer = window.setInterval(() => setNowTick(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    let alive = true;
    void (async () => {
      try {
        const result = await controller.ipc.api("devices.list", { limit: 100 });
        if (!alive || !result.ok) return;
        const other = result.body?.items.find(
          (device) =>
            device.role === "approver" &&
            device.status === "active" &&
            device.id !== state.me?.device.id,
        );
        if (other) setApproverName(other.name);
      } catch {
        // 只用于提示文案；失败不影响审批单。
      }
    })();
    return () => {
      alive = false;
    };
  }, [controller, state.me?.device.id]);

  const now = nowTick + (sheet?.server_time_offset_ms ?? 0);
  const challengeMs = sheet ? Date.parse(sheet.challenge_expires_at) : NaN;
  const challengeLeft = Number.isFinite(challengeMs) ? challengeMs - now : null;
  const challengeExpired = challengeLeft !== null && challengeLeft <= 0;
  const expiresAt = sheet?.signed.expires_at ?? approval?.expires_at ?? "";
  const expiresMs = expiresAt ? Date.parse(expiresAt) : NaN;
  const expiresLeft = Number.isFinite(expiresMs) ? expiresMs - now : null;

  const availability = useMemo(() => {
    if (!approval) return { ok: false as const, reason: "not_pending" as const };
    return controller.signAvailability(
      approval,
      sheet,
      undefined,
      approverName ?? undefined,
    );
  }, [approval, approverName, controller, nowTick, sheet, state.connection, state.me]);

  const unsignedGates = sheet ? unsignedGateIssues(sheet.unsigned.pr) : [];
  const scenario = isScenarioAdapter(controller.ipc);
  const signBlocked =
    !availability.ok || handleSpent || unsignedGates.length > 0;
  const blockText = handleSpent
    ? signDone
      ? "签名请求已返回，签名句柄已使用；如需再次签名请重新载入。"
      : "签名句柄已丢弃，请重新载入审批单后再核对。"
    : unsignedGates.length > 0
      ? `先在 GitHub 上完成必需的 review 与 checks，才能签名：${unsignedGates.join("；")}。`
      : !availability.ok
        ? scenario &&
          state.connection?.phase === "live" &&
          availability.reason === "offline"
          ? "示例模式：未连接真实外壳，签名始终被拒绝。"
          : controller.signBlockText(availability)
        : "";
  const platform = state.me?.device.platform ?? null;
  const canDeny = canDenyNow(controller, state, approval);
  const denyBlockText = denyBlockReason(controller, state, approval);
  const status = approval ? controller.approvalStatus(approval) : null;

  const handleSign = async (): Promise<void> => {
    if (!sheet || signBlocked || challengeExpired || signing || denyBusy) return;
    setSigning(true);
    setSignError(null);
    setSignDone(null);
    ignoreChangeRef.current = true;
    try {
      const result = await controller.approvalSign(sheet.prepared_id);
      if (!result.ok) ignoreChangeRef.current = false;
      discardPrepared();
      setHandleSpent(true);
      if (result.ok && result.body) {
        setSignDone(
          `签名请求已返回：${approvalStatus(result.body.status).label}。以服务端状态为准。`,
        );
      } else if (result.ok) {
        setSignDone("签名请求已返回；请重新载入审批单查看服务端状态。");
      } else if (
        "status" in result &&
        result.problem.code === "stale_head"
      ) {
        setSignError("代码在你确认期间已更新，审批已失效。");
      } else {
        setSignError(parseApiFailure(result));
      }
    } catch (failure) {
      ignoreChangeRef.current = false;
      discardPrepared();
      setHandleSpent(true);
      setSignError(ipcErrorText(failure));
    } finally {
      setSigning(false);
    }
  };

  const handleDeny = async (): Promise<void> => {
    if (!approval || !canDeny || denyBusy || signing) return;
    setDenyBusy(true);
    setDenyError(null);
    try {
      const result = await controller.denyApproval(approval.id);
      if (!result.ok) setDenyError(parseApiFailure(result));
    } catch (failure) {
      setDenyError(ipcErrorText(failure));
    } finally {
      setDenyBusy(false);
    }
  };

  const openCommit = async (): Promise<void> => {
    if (!sheet) return;
    const repo = repoOfTarget(sheet.signed.target);
    if (!repo) {
      setLinkError("审批载荷没有仓库信息，无法在 GitHub 上打开。");
      return;
    }
    setLinkError(null);
    try {
      await controller.ipc.openGithub({
        kind: "commit",
        repo,
        sha: sheet.signed.subject_digest,
      });
    } catch (failure) {
      setLinkError(ipcErrorText(failure));
    }
  };

  const openPr = async (): Promise<void> => {
    if (!sheet || !sheet.links.pr) return;
    const pr = prOfTarget(sheet.signed.target);
    if (!pr) return;
    setLinkError(null);
    try {
      await controller.ipc.openGithub({ kind: "pr", repo: pr.repo, number: pr.number });
    } catch (failure) {
      setLinkError(ipcErrorText(failure));
    }
  };

  return (
    <Modal
      open
      variant="drawer"
      label="审批单"
      onClose={() => {
        if (!signing) onClose();
      }}
      className="ov-approval-dialog"
    >
      <div className="ov-approval">
        <header className="ov-approval__head">
          <h2 className="ov-approval__title">
            {sheet ? `审批单 · ${sheet.signed.action_label}` : "审批单"}
          </h2>
          <ScenarioMark ipc={controller.ipc} />
          <button
            type="button"
            className="ov-icon-btn"
            aria-label="关闭"
            disabled={signing}
            onClick={() => {
              if (!signing) onClose();
            }}
          >
            ×
          </button>
        </header>

        {loading ? (
          <p role="status" className="ov-settings__muted">
            正在载入审批单并执行展示前自检…
          </p>
        ) : null}
        {approvalError ? (
          <div role="alert" className="ov-alert">
            审批详情不可用：{approvalError}
          </div>
        ) : null}
        {prepareError ? (
          <div role="alert" className="ov-alert">
            {prepareError}
            <button type="button" className="ov-btn ov-btn--sm" onClick={() => void prepare()}>
              重新载入
            </button>
          </div>
        ) : null}
        {changed ? (
          <div role="alert" className="ov-notice ov-notice--warn">
            审批已变化，旧签名句柄已丢弃；不会自动重新准备。请重新载入后再次核对。
            <button type="button" className="ov-btn ov-btn--sm" onClick={() => void prepare()}>
              重新载入
            </button>
          </div>
        ) : null}
        {status ? (
          <div className={`ov-approval__status ov-approval__status--${status.tone}`}>
            <span className={`tone-dot tone-${status.tone}`} aria-hidden="true" />
            <span>{status.label}</span>
          </div>
        ) : null}
        {signError ? (
          <div role="alert" className="ov-alert">
            {signError}
          </div>
        ) : null}
        {signDone ? (
          <div role="status" className="ov-notice ov-notice--ok">
            {signDone}
          </div>
        ) : null}
        {denyError ? (
          <div role="alert" className="ov-alert">
            {denyError}
          </div>
        ) : null}
        {linkError ? (
          <div role="alert" className="ov-alert">
            {linkError}
          </div>
        ) : null}

        {sheet ? (
          <>
            <section className="ov-approval__signed" aria-label="已签名信息">
              <h3 className="ov-settings__h3">已签名信息</h3>
              <dl className="ov-approval__grid">
                <dt>动作</dt>
                <dd>{sheet.signed.action_label}</dd>
                <dt>目标</dt>
                <dd>{sheet.signed.target_label}</dd>
                {targetRows(sheet.signed.target).map((row) => (
                  <OptRow key={row.label} label={row.label} value={row.value} mono={row.mono} />
                ))}
                <dt>head SHA</dt>
                <dd className="ov-mono ov-break ov-selectable">
                  {sheet.signed.subject_digest}
                </dd>
                <dt>审批设备</dt>
                <dd>{sheet.signed.device_name}</dd>
                <dt>审批有效期</dt>
                <dd>
                  {formatDateTime(sheet.signed.expires_at)}
                  {expiresLeft !== null ? (
                    <span className="ov-settings__meta">
                      {" "}
                      · 还剩 {formatRemaining(expiresLeft)}
                    </span>
                  ) : null}
                </dd>
                <dt>本次确认窗口</dt>
                <dd>
                  {formatDateTime(sheet.challenge_expires_at)}
                  {challengeLeft !== null ? (
                    <span
                      className={
                        challengeExpired
                          ? "ov-field-error"
                          : "ov-settings__meta"
                      }
                    >
                      {" "}
                      · {challengeExpired ? "已过期" : formatRemaining(challengeLeft)}
                    </span>
                  ) : null}
                </dd>
              </dl>
              <p className="ov-settings__meta">
                系统验证提示将显示：
                <span className="ov-mono">
                  {sheet.signed.action} {sheet.signed.target_label} @{" "}
                  {sheet.signed.digest_head12}
                </span>
              </p>

              <details className="ov-approval__tech">
                <summary>技术细节（已签名）</summary>
                <dl className="ov-approval__grid">
                  <dt>approval_id</dt>
                  <dd className="ov-mono ov-break">{sheet.signed.tech.approval_id}</dd>
                  <dt>nonce</dt>
                  <dd className="ov-mono ov-break">{sheet.signed.tech.nonce}</dd>
                  <dt>版本 v</dt>
                  <dd className="ov-mono">{sheet.signed.tech.v}</dd>
                  <dt>动作原值</dt>
                  <dd className="ov-mono">{sheet.signed.action}</dd>
                  <dt>目标类型</dt>
                  <dd className="ov-mono">{sheet.signed.target.kind}</dd>
                  <dt>device_id</dt>
                  <dd className="ov-mono ov-break">{sheet.signed.device_id}</dd>
                  <dt>摘要前 12 位</dt>
                  <dd className="ov-mono">{sheet.signed.digest_head12}</dd>
                </dl>
              </details>
            </section>

            <section className="ov-approval__unsigned" aria-label="未签名信息，仅供参考">
              <h3 className="ov-approval__unsigned-title">
                未签名信息，仅供参考
              </h3>
              {sheet.unsigned.delivery_title ? (
                <p className="ov-approval__line">
                  <span className="ov-settings__meta">交付：</span>
                  <UntrustedText value={sheet.unsigned.delivery_title} />
                </p>
              ) : null}
              {sheet.unsigned.pr ? (
                <div className="ov-approval__snapshot">
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">PR 标题：</span>
                    <UntrustedText value={sheet.unsigned.pr.title} />
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">检查：</span>
                    {checksText(sheet.unsigned.pr.checks)}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">GitHub review：</span>
                    {reviewText(sheet.unsigned.pr.review_state)}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">合并状态：</span>
                    {sheet.unsigned.pr.mergeable_state}
                    {sheet.unsigned.pr.in_merge_queue
                      ? " · 已在 merge queue"
                      : " · 不在 merge queue"}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">快照：</span>
                    {formatDateTime(sheet.unsigned.pr.synced_at)}
                  </p>
                </div>
              ) : (
                <p className="ov-settings__muted">没有 PR 快照。</p>
              )}

              {sheet.unsigned.note ? (
                <p className="ov-approval__line">
                  <UntrustedText value={sheet.unsigned.note} />
                </p>
              ) : null}

              {sheet.unsigned.changes ? (
                <p className="ov-settings__muted">
                  服务端返回了改动明细，但本版本不展示其结构；请以 GitHub 上的
                  diff 为准。
                </p>
              ) : (
                <p className="ov-settings__muted">服务端未提供改动明细。</p>
              )}
              <div className="ov-approval__links">
                {sheet.links.commit ? (
                  <button
                    type="button"
                    className="ov-link-btn"
                    onClick={() => void openCommit()}
                  >
                    在 GitHub 上查看此 SHA 的 diff ↗
                  </button>
                ) : null}
                {sheet.links.pr && prOfTarget(sheet.signed.target) ? (
                  <button
                    type="button"
                    className="ov-link-btn"
                    onClick={() => void openPr()}
                  >
                    在 GitHub 上查看 PR ↗
                  </button>
                ) : null}
                {sheet.unsigned.pr?.review_state !== "approved" &&
                prOfTarget(sheet.signed.target) ? (
                  <button
                    type="button"
                    className="ov-link-btn"
                    onClick={() => void openPr()}
                  >
                    去 GitHub 上 approve ↗
                  </button>
                ) : null}
              </div>
            </section>

            <section className="ov-approval__gates" aria-label="合并闸门（未签名快照）">
              <h3 className="ov-settings__h3">合并闸门（未签名快照）</h3>
              {sheet.unsigned.pr ? (
                <ul className="ov-gates">
                  <li>
                    <span
                      className={`tone-dot ${
                        sheet.unsigned.pr.checks.required_ok ? "tone-ok" : "tone-warn"
                      }`}
                      aria-hidden="true"
                    />
                    必需 checks @{shortSha(sheet.signed.subject_digest)}：
                    {checksText(sheet.unsigned.pr.checks)}
                  </li>
                  <li>
                    <span
                      className={`tone-dot ${
                        sheet.unsigned.pr.review_state === "approved"
                          ? "tone-ok"
                          : "tone-warn"
                      }`}
                      aria-hidden="true"
                    />
                    GitHub review：{reviewText(sheet.unsigned.pr.review_state)}
                  </li>
                  <li>
                    <span
                      className={`tone-dot ${
                        sheet.unsigned.pr.in_merge_queue ? "tone-run" : "tone-idle"
                      }`}
                      aria-hidden="true"
                    />
                    merge queue：{sheet.unsigned.pr.in_merge_queue ? "已排队" : "未排队"}
                  </li>
                  <li>
                    <span
                      className={`tone-dot ${
                        approval?.status === "pending" ? "tone-need" : "tone-idle"
                      }`}
                      aria-hidden="true"
                    />
                    Gantry 签名：{status?.label ?? "未知"}
                  </li>
                </ul>
              ) : (
                <p className="ov-settings__muted">
                  此审批不是 PR 合并动作，没有 GitHub 合并闸门。
                </p>
              )}
            </section>

            <section className="ov-approval__footer">
              <p className="ov-settings__meta">
                有效期与确认窗口分别计时；刷新或重新载入会重新执行外壳自检。
              </p>
              {signBlocked ? (
                <div role="status" className="ov-approval__block">
                  {blockText}
                </div>
              ) : challengeExpired ? (
                <div role="status" className="ov-approval__block">
                  已过期 · 重新载入
                </div>
              ) : null}
              {!canDeny && denyBlockText ? (
                <div role="status" className="ov-approval__block">
                  {denyBlockText}
                </div>
              ) : null}

              <div className="ov-approval__actions">
                <button
                  type="button"
                  className="ov-btn ov-btn--primary"
                  disabled={signBlocked || challengeExpired || signing || denyBusy}
                  title={signBlocked ? blockText : undefined}
                  onClick={() => void handleSign()}
                >
                  {signing ? "等待系统确认…" : signButtonLabel(platform)}
                </button>
                <button
                  type="button"
                  className="ov-btn"
                  disabled={!canDeny || denyBusy || signing}
                  title={!canDeny ? denyBlockText : undefined}
                  onClick={() => void handleDeny()}
                >
                  {denyBusy ? "正在拒绝…" : "拒绝"}
                </button>
                {challengeExpired || !sheet || handleSpent ? (
                  <button
                    type="button"
                    className="ov-btn"
                    onClick={() => void prepare()}
                  >
                    重新载入
                  </button>
                ) : null}
              </div>
              <p className="ov-settings__meta">
                签名只提交外壳持有的 prepared_id；界面不会伪造签名、配对或保存成功。
              </p>
            </section>
          </>
        ) : null}
      </div>
    </Modal>
  );
}

interface TargetRow {
  label: string;
  value: string;
  mono?: boolean;
}

function targetRows(target: ApprovalTarget): TargetRow[] {
  switch (target.kind) {
    case "pr":
      return [
        { label: "仓库", value: target.repo, mono: true },
        { label: "PR", value: `#${target.number}`, mono: true },
      ];
    case "branch":
      return [
        { label: "仓库", value: target.repo, mono: true },
        { label: "分支", value: target.branch, mono: true },
        { label: "远端 OID", value: target.expected_remote_oid, mono: true },
      ];
    case "review_thread":
      return [
        { label: "仓库", value: target.repo, mono: true },
        { label: "PR", value: `#${target.number}`, mono: true },
        { label: "线程", value: target.thread_node_id, mono: true },
      ];
    case "patch":
      return [
        { label: "仓库", value: target.repo, mono: true },
        { label: "Attempt", value: target.attempt_id, mono: true },
        { label: "路径", value: target.paths.join("、") || "未提供" },
      ];
    case "settings":
      return [
        { label: "资源", value: target.resource, mono: true },
        { label: "作用域", value: target.scope, mono: true },
        { label: "基准 rev", value: String(target.base_rev), mono: true },
      ];
    case "device":
      return [{ label: "设备 ID", value: target.id, mono: true }];
  }
}

function OptRow({
  label,
  value,
  mono,
}: TargetRow): ReactNode {
  return (
    <>
      <dt>{label}</dt>
      <dd className={mono ? "ov-mono ov-break" : undefined}>{value}</dd>
    </>
  );
}

function repoOfTarget(target: ApprovalTarget): string | null {
  switch (target.kind) {
    case "pr":
    case "branch":
    case "review_thread":
    case "patch":
      return target.repo;
    default:
      return null;
  }
}

function prOfTarget(
  target: ApprovalTarget,
): { repo: string; number: number } | null {
  if (target.kind === "pr" || target.kind === "review_thread") {
    return { repo: target.repo, number: target.number };
  }
  return null;
}


function unsignedGateIssues(
  pr: ApprovalSheetModel["unsigned"]["pr"],
): string[] {
  if (!pr) return [];
  const issues: string[] = [];
  if (!pr.checks.required_ok) issues.push("必需 checks 未通过");
  if (
    ["dirty", "blocked", "behind", "draft", "unknown"].includes(
      pr.mergeable_state,
    )
  ) {
    issues.push(`GitHub 合并状态为 ${pr.mergeable_state}`);
  }
  if (pr.review_state === "changes_requested") issues.push("GitHub review 要求修改");
  return issues;
}

function checksText(checks: PullRequestRef["checks"]): string {
  const parts = [`${checks.passed}/${checks.passed + checks.failed + checks.pending} 通过`];
  if (checks.failed > 0) parts.push(`${checks.failed} 失败`);
  if (checks.pending > 0) parts.push(`${checks.pending} 等待中`);
  parts.push(checks.required_ok ? "必需 checks 已满足" : "必需 checks 未满足");
  return parts.join("，");
}

function reviewText(state: PullRequestRef["review_state"]): string {
  switch (state) {
    case "approved":
      return "已 approve";
    case "changes_requested":
      return "要求修改";
    case "none":
    default:
      return "未 approve";
  }
}

function canDenyNow(
  controller: DesktopController,
  state: DesktopState,
  approval: Approval | null,
): boolean {
  return Boolean(
    approval &&
      approval.status === "pending" &&
      state.me?.profile === "desktop" &&
      state.connection?.phase === "live" &&
      controller.can("deny"),
  );
}

function denyBlockReason(
  controller: DesktopController,
  state: DesktopState,
  approval: Approval | null,
): string {
  if (!approval) return "审批不可用，暂不能拒绝。";
  if (approval.status !== "pending") return "此审批已不在待签状态。";
  if (state.connection?.phase !== "live") return "连接中断，暂不能拒绝。";
  if (state.me?.profile !== "desktop") return "只有桌面设备可以拒绝审批。";
  if (!controller.can("deny")) return "只有审批设备可以拒绝。";
  return "";
}
