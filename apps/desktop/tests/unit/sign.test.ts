import { describe, expect, it } from "vitest";
import {
  approvalStatusLabel,
  mergeGateIssues,
  signAvailability,
  signBlockText,
  signButtonLabel,
  shortSha,
  type SignAvailabilityInput,
} from "@/store/sign";
import { ScenarioIpcAdapter } from "@/ipc/scenario/adapter";
import { buildScenarioData } from "@/ipc/scenario/data";
import { TEST_NOW } from "./helpers";

async function fixture(): Promise<{
  input: SignAvailabilityInput;
  approval: ReturnType<typeof buildScenarioData>["approvals"][number];
  pr: NonNullable<ReturnType<typeof buildScenarioData>["deliveries"][number]["pr"]>;
}> {
  const data = buildScenarioData("default");
  const approval = data.approvals[0];
  const delivery = data.deliveries.find(
    (item) => item.id === approval.delivery_id,
  );
  if (!delivery?.pr) throw new Error("fixture missing PR");
  const adapter = new ScenarioIpcAdapter("default");
  const sheet = await adapter.approvalPrepare({ approvalId: approval.id });
  return {
    input: {
      conn: data.connection,
      me: data.me,
      approval,
      pr: delivery.pr,
      sheet,
      now: TEST_NOW + 1_000,
    },
    approval,
    pr: delivery.pr,
  };
}

describe("signAvailability", () => {
  it("默认示例即使连接、角色、闸门、challenge 均满足也不可签", async () => {
    const { input } = await fixture();
    expect(signAvailability(input)).toEqual({ ok: false, reason: "offline" });
  });

  it("外壳允许时可签", async () => {
    const { input } = await fixture();
    if (!input.sheet) throw new Error("sheet missing");
    expect(
      signAvailability({
        ...input,
        sheet: { ...input.sheet, can_sign: { ok: true } },
      }),
    ).toEqual({ ok: true });
  });

  it("连接中断优先于其他条件", async () => {
    const { input } = await fixture();
    expect(
      signAvailability({ ...input, conn: { ...input.conn, phase: "reconnecting" } }),
    ).toEqual({ ok: false, reason: "offline" });
  });

  it("非审批设备不能签名", async () => {
    const { input } = await fixture();
    expect(
      signAvailability({
        ...input,
        me: { ...input.me, device: { ...input.me.device, role: "operator" } },
        approverDeviceName: "书房 Mac",
      }),
    ).toEqual({
      ok: false,
      reason: "not_approver",
      approverDeviceName: "书房 Mac",
    });
  });

  it("审批已非 pending", async () => {
    const { input } = await fixture();
    expect(
      signAvailability({
        ...input,
        approval: { ...input.approval, status: "granted" },
      }),
    ).toEqual({ ok: false, reason: "not_pending" });
  });

  it("PR head 与 subject_digest 不一致为 stale", async () => {
    const { input, pr } = await fixture();
    expect(
      signAvailability({
        ...input,
        pr: { ...pr, head_sha: "f".repeat(40) },
      }),
    ).toEqual({ ok: false, reason: "stale" });
  });

  it("GitHub 闸门未满足时列出缺项", async () => {
    const { input, pr } = await fixture();
    const result = signAvailability({
      ...input,
      pr: {
        ...pr,
        checks: { ...pr.checks, required_ok: false },
        review_state: "changes_requested",
      },
    });
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.reason).toBe("gates_unmet");
      expect(result.gates).toContain("必需 checks 未通过");
      expect(result.gates).toContain("GitHub review 要求修改");
    }
  });

  it("challenge 过期后要求重新载入", async () => {
    const { input } = await fixture();
    if (!input.sheet) throw new Error("sheet missing");
    expect(
      signAvailability({
        ...input,
        sheet: { ...input.sheet, can_sign: { ok: true } },
        now: Date.parse(input.sheet.challenge_expires_at) + 1_000,
      }),
    ).toEqual({ ok: false, reason: "expired" });
  });

  it("缺少展示模型不可签名", async () => {
    const { input } = await fixture();
    expect(signAvailability({ ...input, sheet: null })).toEqual({
      ok: false,
      reason: "expired",
    });
  });

  it("展示模型与审批不一致时自检失败", async () => {
    const { input } = await fixture();
    if (!input.sheet) throw new Error("sheet missing");
    expect(
      signAvailability({
        ...input,
        sheet: {
          ...input.sheet,
          signed: { ...input.sheet.signed, subject_digest: "0".repeat(40) },
        },
      }),
    ).toEqual({ ok: false, reason: "self_check_failed" });
  });

  it("外壳自检失败透传", async () => {
    const { input } = await fixture();
    if (!input.sheet) throw new Error("sheet missing");
    expect(
      signAvailability({
        ...input,
        sheet: { ...input.sheet, can_sign: { ok: false, reason: "self_check_failed" } },
      }),
    ).toEqual({ ok: false, reason: "self_check_failed" });
  });
});

describe("审批文案与闸门", () => {
  it("按平台给出系统确认文案", () => {
    expect(signButtonLabel("macos")).toContain("Touch ID");
    expect(signButtonLabel("windows")).toContain("Windows Hello");
    expect(signButtonLabel("ios")).toBe("签名批准");
  });

  it("短 SHA 固定 7 位，展示模型 12 位", async () => {
    const { input } = await fixture();
    expect(shortSha(input.approval.subject_digest)).toHaveLength(7);
    expect(input.sheet?.signed.digest_head12).toHaveLength(12);
  });

  it("审批状态文案由同一函数生成", async () => {
    const data = buildScenarioData("default");
    const approval = data.approvals[0];
    expect(approvalStatusLabel(approval).label).toBe("待签名");
    expect(
      approvalStatusLabel({ ...approval, status: "granted" }).label,
    ).toContain("排队 merge");
    expect(
      approvalStatusLabel(
        { ...approval, status: "consumed" },
        { prState: "merged", headSha: approval.subject_digest },
      ).label,
    ).toContain("已合并");
    expect(
      approvalStatusLabel(
        { ...approval, status: "consumed" },
        { deliveryStage: "closed" },
      ).label,
    ).toContain("交付完成");
    expect(
      approvalStatusLabel({ ...approval, status: "denied" }).label,
    ).toContain("已拒绝");
  });

  it("mergeGateIssues 只列未满足项", async () => {
    const { pr } = await fixture();
    expect(mergeGateIssues(pr)).toEqual([]);
    const codes = mergeGateIssues({ ...pr, draft: true, state: "closed" }).map(
      (g) => g.code,
    );
    expect(codes).toContain("state");
    expect(codes).toContain("draft");
  });

  it("拒绝原因有明确中文文案", async () => {
    const { input } = await fixture();
    const result = signAvailability({ ...input, sheet: null });
    if (!result.ok) {
      expect(signBlockText(result)).toContain("重新载入");
    }
  });
});
