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
import { shortSha, signButtonLabel } from "@/store/sign";
import { UntrustedText } from "@/render";
import { simulationFor } from "@/features/workspace/SimulationActivity";
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
  // 断线期间准备的审批单在恢复后自动重新准备（见下方 effect）。
  const phaseRef = useRef(state.connection?.phase);
  phaseRef.current = state.connection?.phase;
  const preparedOffline = useRef(false);
  const sawOfflineSheet = useRef(false);
  const signedRef = useRef(false);
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
    preparedOffline.current = phaseRef.current !== "live";
    setLoading(true);
    setPrepareError(null);
    setChanged(false);
    setSignError(null);
    setSignDone(null);
    setHandleSpent(false);
    signedRef.current = false;
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

  // 断线期间准备的审批单不含 challenge（can_sign = not_connected）；连接恢复后自动重新准备一次，
  // 不要求用户手动重新载入（E2E-04）。
  // 只在“准备时确实断线 → 现在恢复”这一次转换时触发，避免对始终返回 not_connected 的替身反复准备。
  const connectionLive = state.connection?.phase === "live";
  useEffect(() => {
    const offlineSheet =
      sheet !== null && !sheet.can_sign.ok && sheet.can_sign.reason === "not_connected";
    if (!offlineSheet) {
      sawOfflineSheet.current = false;
      return;
    }
    if (!connectionLive) {
      sawOfflineSheet.current = true;
      return;
    }
    if ((sawOfflineSheet.current || preparedOffline.current) && !loading) {
      sawOfflineSheet.current = false;
      preparedOffline.current = false;
      void prepare();
    }
  }, [connectionLive, loading, prepare, sheet]);

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
    // 签名返回的终态可能被一次过期的 pending 列表刷新短暂覆盖；
    // 仍保留签名结果，直到用户重新载入。
    if (signedRef.current && approval?.status === "pending") return;
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
  }, [approval, approvalKey, discardPrepared, sheet]);

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

  // Keep the demo-only approval affordance out of the production bundle. The
  // mode constant is folded by Vite before Rollup tree-shaking.
  const scenario =
    import.meta.env.MODE !== "production" && isScenarioAdapter(controller.ipc);
  const simulation = simulationFor(controller);
  const approvalDelivery = findApprovalDelivery(state, approval);
  // 未签名区的 PR 信息与闸门以较新的 GitHub 同步结果为准：forge.synced 刷新交付后，
  // 展示与签名可用性一起更新，不必重新载入审批单（E2E-07）。
  const unsignedPr = sheet ? newerPr(sheet.unsigned.pr, approvalDelivery?.pr ?? null) : null;
  const unsignedGates = unsignedGateIssues(unsignedPr);
  const liveHead =
    !approval ||
    approval.target.kind !== "pr" ||
    approvalDelivery?.pr?.head_sha === approval.subject_digest;
  const simulateBlockText = !scenario
    ? null
    : !simulation
      ? "应用内模拟边界不可用；生产签名仍被拒绝。"
      : !approval || approval.status !== "pending"
        ? "审批已不是待处理状态。"
        : state.connection?.phase !== "live"
          ? "连接无效，不能模拟批准。"
          : state.me?.profile !== "desktop" ||
              state.me.device.role !== "approver"
            ? "只有审批设备可以模拟批准。"
            : !liveHead
              ? "PR head 已变化，审批载荷不再对应当前代码。"
              : !sheet ||
                  sheet.signed.tech.approval_id !== approval.id
                ? "审批单尚未载入或已变化，请重新载入。"
                : challengeExpired
                  ? "确认窗口已过期，请重新载入。"
                  : expiresLeft !== null && expiresLeft <= 0
                    ? "审批已过期。"
                    : "";
  const canSimulate =
    scenario && simulation !== null && simulateBlockText === "";
  const signBlocked =
    !availability.ok || handleSpent || unsignedGates.length > 0;
  // 场景中的非待签审批没有 signature；载荷区只是演示载荷，不能称为已签名。
  const simulated =
    scenario &&
    approval !== null &&
    approval.status !== "pending" &&
    approval.signature === null;
  const blockText = handleSpent
    ? simulated
      ? "本次模拟没有生成签名，也没有可复用的签名句柄；如需重新演示，请重新载入审批单。"
      : signDone
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
    signedRef.current = true;
    ignoreChangeRef.current = true;
    try {
      const result = await controller.approvalSign(sheet.prepared_id);
      if (!result.ok) {
        signedRef.current = false;
        ignoreChangeRef.current = false;
      }
      discardPrepared();
      setHandleSpent(true);
      if (result.ok && result.body) {
        setSignDone(
          `签名请求已返回：${controller.approvalStatus(result.body).label}。以服务端状态为准。`,
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
      signedRef.current = false;
      ignoreChangeRef.current = false;
      discardPrepared();
      setHandleSpent(true);
      setSignError(ipcErrorText(failure));
    } finally {
      setSigning(false);
    }
  };

  const handleSimulateApprove = async (): Promise<void> => {
    if (!approval || !simulation || !canSimulate || signing || denyBusy) return;
    setSigning(true);
    setSignError(null);
    setSignDone(null);
    ignoreChangeRef.current = true;
    try {
      const result = simulation.simulateApproval(approval.id);
      if (!result.ok) {
        ignoreChangeRef.current = false;
        setSignError(result.message ?? "模拟批准被拒绝。");
        return;
      }
      signedRef.current = true;
      discardPrepared();
      setHandleSpent(true);
      setSignDone(
        "模拟批准已应用：未生成 signature，未调用真实签名或 GitHub；交付已关闭，也没有写入 merge queue。仅修改开发场景内存。",
      );
      await controller.ensureApproval(approval.id);
    } finally {
      setSigning(false);
    }
  };

  const handleDeny = async (): Promise<void> => {
    if (!approval || !canDeny || denyBusy || signing) return;
    setDenyBusy(true);
    setDenyError(null);
    try {
      if (scenario && simulation) {
        ignoreChangeRef.current = true;
        const result = simulation.simulateDenial(approval.id);
        if (!result.ok) {
          ignoreChangeRef.current = false;
          setDenyError(result.message ?? "模拟拒绝被拒绝。");
          return;
        }
        discardPrepared();
        setSignDone(
          "模拟拒绝已应用：未生成 signature，未调用真实签名或 GitHub；交付已放弃。仅修改开发场景内存。",
        );
        await controller.ensureApproval(approval.id);
        return;
      }
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
            <section
              className="ov-approval__signed"
              aria-label={simulated ? "演示载荷信息" : "已签名信息"}
            >
              <h3 className="ov-settings__h3">
                {simulated ? "待签名载荷（模拟，未生成签名）" : "已签名信息"}
              </h3>
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
                <summary>
                  {simulated ? "技术细节（演示载荷）" : "技术细节（已签名）"}
                </summary>
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
              {unsignedPr ? (
                <div className="ov-approval__snapshot">
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">PR 标题：</span>
                    <UntrustedText value={unsignedPr.title} />
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">检查：</span>
                    {checksText(unsignedPr.checks)}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">GitHub review：</span>
                    {reviewText(unsignedPr.review_state)}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">合并状态：</span>
                    {unsignedPr.mergeable_state}
                    {unsignedPr.in_merge_queue
                      ? " · 已在 merge queue"
                      : " · 不在 merge queue"}
                  </p>
                  <p className="ov-approval__line">
                    <span className="ov-settings__meta">快照：</span>
                    {formatDateTime(unsignedPr.synced_at)}
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
                {unsignedPr?.review_state !== "approved" &&
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
              {unsignedPr ? (
                <ul className="ov-gates">
                  <li>
                    <span
                      className={`tone-dot ${
                        unsignedPr.checks.required_ok ? "tone-ok" : "tone-warn"
                      }`}
                      aria-hidden="true"
                    />
                    必需 checks @{shortSha(sheet.signed.subject_digest)}：
                    {checksText(unsignedPr.checks)}
                  </li>
                  <li>
                    <span
                      className={`tone-dot ${
                        unsignedPr.review_state === "approved"
                          ? "tone-ok"
                          : "tone-warn"
                      }`}
                      aria-hidden="true"
                    />
                    GitHub review：{reviewText(unsignedPr.review_state)}
                  </li>
                  <li>
                    <span
                      className={`tone-dot ${
                        unsignedPr.in_merge_queue ? "tone-run" : "tone-idle"
                      }`}
                      aria-hidden="true"
                    />
                    merge queue：{unsignedPr.in_merge_queue ? "已排队" : "未排队"}
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

              {simulateBlockText ? (
                <div role="status" className="ov-approval__block">
                  {simulateBlockText}
                </div>
              ) : null}
              <div className="ov-approval__actions">
                {scenario ? (
                  <button
                    type="button"
                    className="ov-btn ov-btn--primary"
                    disabled={!canSimulate || signing || denyBusy}
                    title={!canSimulate ? simulateBlockText ?? undefined : undefined}
                    onClick={() => void handleSimulateApprove()}
                  >
                    {signing ? "正在应用模拟批准…" : "模拟批准（不生成签名）"}
                  </button>
                ) : null}
                <button
                  type="button"
                  className={`ov-btn${scenario ? "" : " ov-btn--primary"}`}
                  disabled={signBlocked || challengeExpired || signing || denyBusy}
                  title={signBlocked ? blockText : undefined}
                  onClick={() => void handleSign()}
                >
                  {signing
                    ? "等待系统确认…"
                    : scenario
                      ? "生产签名批准（示例模式拒绝）"
                      : signButtonLabel(platform)}
                </button>
                <button
                  type="button"
                  className="ov-btn"
                  disabled={!canDeny || denyBusy || signing}
                  title={!canDeny ? denyBlockText : undefined}
                  onClick={() => void handleDeny()}
                >
                  {denyBusy ? "正在拒绝…" : scenario ? "模拟拒绝" : "拒绝"}
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
                {scenario
                  ? "模拟批准只改变开发场景内存中的审批状态，不生成 signature、证书或网络写入；生产签名路径在示例模式始终拒绝。"
                  : "签名只提交外壳持有的 prepared_id；界面不会伪造签名、配对或保存成功。"}
              </p>
            </section>
          </>
        ) : null}
      </div>
    </Modal>
  );
}

function findApprovalDelivery(
  state: DesktopState,
  approval: Approval | null,
): import("@/api/types").Delivery | null {
  if (!approval?.delivery_id) return null;
  for (const data of Object.values(state.projectData)) {
    const delivery = data.deliveries.find(
      (item) => item.id === approval.delivery_id,
    );
    if (delivery) return delivery;
  }
  return null;
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
        { label: "远端 OID", value: target.expected_remote_oid ?? "—", mono: true },
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


/** 未签名区展示与闸门判断共用的 PR 视图：审批单快照与交付缓存的 PR 都满足这个形状。 */
type GatePr = NonNullable<ApprovalSheetModel["unsigned"]["pr"]>;

/** 审批单快照与交付缓存里的 PR 取同步时间较新者；同样新时保守地沿用快照。 */
function newerPr(snapshot: GatePr | null | undefined, live: GatePr | null): GatePr | null {
  if (!snapshot) return live;
  if (!live) return snapshot;
  return Date.parse(live.synced_at) > Date.parse(snapshot.synced_at) ? live : snapshot;
}

function unsignedGateIssues(pr: GatePr | null): string[] {
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
