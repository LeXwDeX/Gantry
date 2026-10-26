/**
 * 设置表单的字段描述与编辑原语。
 *
 * 共享机器可读 schema 尚未交付；这里的描述按 docs/interfaces/config.md
 * §2.2、§3–§10 与 §13.2 的文字契约手工转写，不冒充生成代码。
 * 字段本身不直接读写 adapter；页面负责加载、预览、提交和回读。
 */
import type { ReactNode } from "react";
import type { SettingsCatalog, SettingsIssue } from "@/api/types";
import { issueText } from "./settingsData";

export type Path = string[];

export type FieldKind =
  | "text"
  | "integer"
  | "number"
  | "boolean"
  | "enum"
  | "lines";

export interface FieldSpec {
  path: Path;
  label: string;
  kind: FieldKind;
  options?: string[];
  optionLabels?: Record<string, string>;
  placeholder?: string;
  help?: string;
  unit?: string;
  min?: number;
  max?: number;
  required?: boolean;
  readOnly?: boolean;
  readOnlyText?: string;
}

export interface TableColumnSpec {
  key: string;
  label: string;
  kind: FieldKind | "enum-list" | "when" | "secret-ref";
  path?: Path;
  options?: string[] | ((entry: Record<string, unknown>) => string[]);
  placeholder?: string;
  help?: string;
  min?: number;
  max?: number;
  required?: boolean;
  lockedValue?: (key: string) => unknown | undefined;
  lockedText?: (key: string) => string | undefined;
}

export interface TableSpec {
  path: Path;
  title: string;
  description?: string;
  entryLabel: string;
  addLabel?: string;
  columns: TableColumnSpec[];
  fixedKeys?: string[];
  createEntry?: (existing: Record<string, unknown>) => unknown;
  minWidth?: number;
  emptyText?: string;
}

export interface ProjectOverrideField extends FieldSpec {
  tightenOnly: boolean;
  /** lines 字段清空时写入 [] 而不是删除覆盖键；用于显式表达空列表覆盖。 */
  keepEmptyList?: boolean;
}

export interface ProjectOverrideTable extends TableSpec {
  tightenOnly: boolean;
}

export type ProjectResource =
  | "models"
  | "levels"
  | "policies"
  | "guardian"
  | "sandbox"
  | "egress";

export const PROJECT_RESOURCES: ProjectResource[] = [
  "models",
  "levels",
  "policies",
  "guardian",
  "sandbox",
  "egress",
];

export const PROJECT_RESOURCE_LABELS: Record<ProjectResource, string> = {
  models: "模型",
  levels: "档位与预算",
  policies: "危险动作策略",
  guardian: "后台守护",
  sandbox: "沙盒",
  egress: "出网档案",
};

export const INBOX_KINDS = [
  "approval_needed",
  "decision_needed",
  "spec_needed",
  "budget_exhausted",
  "escalation",
  "security_finding",
];

export const LEVELS = ["None", "Low", "Medium", "High", "Max", "Ultimate"];
export const BUDGET_LEVELS = ["low", "medium", "high", "max", "ultimate"] as const;

export function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

export function cloneJson<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

export function getAtPath(value: unknown, path: Path): unknown {
  let current: unknown = value;
  for (const key of path) {
    if (Array.isArray(current)) {
      if (!/^\d+$/.test(key)) return undefined;
      current = current[Number(key)];
      continue;
    }
    const record = asRecord(current);
    if (!record) return undefined;
    current = record[key];
  }
  return current;
}

/** 不可变写入；next 为 undefined 时删除该键。 */
export function setAtPath(root: unknown, path: Path, next: unknown): unknown {
  if (path.length === 0) return next;
  const base = cloneJson(asRecord(root) ?? {});
  let current: Record<string, unknown> = base;
  for (let index = 0; index < path.length - 1; index += 1) {
    const key = path[index];
    const child = current[key];
    const fallback = asRecord(child) ?? {};
    current[key] = cloneJson(fallback);
    current = current[key] as Record<string, unknown>;
  }
  const last = path[path.length - 1];
  if (next === undefined) delete current[last];
  else current[last] = cloneJson(next);
  return base;
}

export function deleteAtPath(root: unknown, path: Path): unknown {
  return setAtPath(root, path, undefined);
}

export function renameRecordKey(
  root: unknown,
  path: Path,
  oldKey: string,
  newKey: string,
): unknown {
  const record = asRecord(getAtPath(root, path));
  if (!record || !newKey || oldKey === newKey || record[newKey] !== undefined) {
    return root;
  }
  const renamed: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(record)) {
    renamed[key === oldKey ? newKey : key] = value;
  }
  return setAtPath(root, path, renamed);
}

export function pathPointer(path: Path, prefix = "/"): string {
  return prefix + path.map(escapePointer).join("/");
}

function escapePointer(part: string): string {
  return part.replace(/~/g, "~0").replace(/\//g, "~1");
}

export function issuesAtPath(issues: SettingsIssue[], path: Path): SettingsIssue[] {
  const pointer = pathPointer(path);
  return issues.filter((issue) => issue.pointer === pointer);
}

export function issuesUnderPath(issues: SettingsIssue[], path: Path): SettingsIssue[] {
  const prefix = pathPointer(path);
  return issues.filter(
    (issue) => issue.pointer === prefix || issue.pointer.startsWith(`${prefix}/`),
  );
}

export function formatInline(value: unknown): string {
  if (value === undefined || value === null || value === "") return "—";
  if (typeof value === "boolean") return value ? "是" : "否";
  if (typeof value === "number") return Number.isFinite(value) ? String(value) : "—";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    return value.length === 0 ? "—" : value.map(formatInline).join("、");
  }
  const record = asRecord(value);
  if (record) {
    const entries = Object.entries(record);
    return entries.length === 0
      ? "—"
      : entries.map(([key, entry]) => `${key}: ${formatInline(entry)}`).join("；");
  }
  return String(value);
}

export function linesToArray(text: string): string[] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter(Boolean);
}

export function resolveOptions(
  column: TableColumnSpec,
  entry: Record<string, unknown>,
): string[] {
  if (Array.isArray(column.options)) return column.options;
  if (typeof column.options === "function") return column.options(entry);
  return [];
}

export function collectRequiredIssues(
  specs: FieldSpec[],
  value: unknown,
): SettingsIssue[] {
  const issues: SettingsIssue[] = [];
  for (const spec of specs) {
    const current = getAtPath(value, spec.path);
    const pointer = pathPointer(spec.path);
    if (spec.required && isEmptyValue(current)) {
      issues.push({
        pointer,
        code: "required",
        message: `${spec.label}是必填项。`,
      });
      continue;
    }
    if (current === undefined || current === null || current === "") continue;
    if (spec.kind === "integer" && !Number.isInteger(current)) {
      issues.push({ pointer, code: "type", message: `${spec.label}必须是整数。` });
    } else if (
      spec.kind === "number" &&
      (typeof current !== "number" || !Number.isFinite(current))
    ) {
      issues.push({ pointer, code: "type", message: `${spec.label}必须是数字。` });
    } else if (
      (spec.kind === "integer" || spec.kind === "number") &&
      typeof current === "number"
    ) {
      if (spec.min !== undefined && current < spec.min) {
        issues.push({
          pointer,
          code: "range",
          message: `${spec.label}不得小于 ${spec.min}。`,
        });
      }
      if (spec.max !== undefined && current > spec.max) {
        issues.push({
          pointer,
          code: "range",
          message: `${spec.label}不得大于 ${spec.max}。`,
        });
      }
    } else if (
      spec.kind === "enum" &&
      spec.options &&
      typeof current === "string" &&
      !spec.options.includes(current)
    ) {
      issues.push({
        pointer,
        code: "enum",
        message: `${spec.label}不在允许取值内。`,
      });
    } else if (
      spec.kind === "lines" &&
      (!Array.isArray(current) ||
        current.some((entry) => typeof entry !== "string"))
    ) {
      issues.push({ pointer, code: "type", message: `${spec.label}必须是字符串列表。` });
    }
  }
  return issues;
}

function isEmptyValue(value: unknown): boolean {
  return (
    value === undefined ||
    value === null ||
    value === "" ||
    (Array.isArray(value) && value.length === 0)
  );
}

export function FieldIssues({
  issues,
}: {
  issues: SettingsIssue[];
}): ReactNode {
  if (issues.length === 0) return null;
  return (
    <span className="ov-field-error" role="alert">
      {issues.map((issue) => (
        <span key={`${issue.pointer}-${issue.code}-${issue.message ?? ""}`}>
          {issue.message ?? issueText(issue)}
        </span>
      ))}
    </span>
  );
}

export function secretKey(path: Path): string {
  return JSON.stringify(path);
}

export function splitSecretKey(key: string): Path {
  return JSON.parse(key) as Path;
}

export function injectSecrets(
  value: unknown,
  secrets: Record<string, string>,
): unknown {
  let next = cloneJson(value);
  for (const [key, secret] of Object.entries(secrets)) {
    if (!secret) continue;
    next = setAtPath(next, [...splitSecretKey(key), "value"], secret);
  }
  return next;
}

/** 回读比较用：忽略只写的 value 与只读 present。 */
export function normalizeForCompare(value: unknown): unknown {
  const copy = cloneJson(value);
  return scrubReadOnly(copy);
}

function scrubReadOnly(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(scrubReadOnly);
  const record = asRecord(value);
  if (!record) return value;
  const clean: Record<string, unknown> = {};
  const secretRef = typeof record.secret_ref === "string";
  for (const [key, entry] of Object.entries(record)) {
    if (key === "value" && secretRef) continue;
    if (key === "present" && secretRef) continue;
    clean[key] = scrubReadOnly(entry);
  }
  return clean;
}

interface FieldControlProps {
  spec: FieldSpec;
  value: unknown;
  disabled?: boolean;
  secret?: string;
  onSecretChange?: (value: string) => void;
  onChange: (value: unknown) => void;
  issues: SettingsIssue[];
}

function FieldControl({
  spec,
  value,
  disabled,
  secret,
  onSecretChange,
  onChange,
  issues,
}: FieldControlProps): ReactNode {
  if (spec.readOnly) {
    return (
      <span className="sr-readonly">
        {spec.readOnlyText ?? formatInline(value)}
      </span>
    );
  }
  if (spec.kind === "boolean") {
    return (
      <input
        type="checkbox"
        className="sr-checkbox"
        aria-label={spec.label}
        checked={value === true}
        disabled={disabled}
        onChange={(event) => onChange(event.target.checked)}
      />
    );
  }
  if (spec.kind === "enum") {
    const options = spec.options ?? [];
    const selected = typeof value === "string" ? value : "";
    return (
      <select
        className="ov-select"
        aria-label={spec.label}
        value={selected}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      >
        {selected && !options.includes(selected) ? (
          <option value={selected}>{selected}（目录未列出）</option>
        ) : null}
        {!selected ? <option value="">未配置</option> : null}
        {options.map((option) => (
          <option value={option} key={option}>
            {spec.optionLabels?.[option] ?? option}
          </option>
        ))}
      </select>
    );
  }
  if (spec.kind === "lines") {
    const text = Array.isArray(value)
      ? value.map((entry) => String(entry)).join("\n")
      : "";
    return (
      <textarea
        className="ov-textarea sr-lines"
        aria-label={spec.label}
        value={text}
        disabled={disabled}
        placeholder={spec.placeholder}
        spellCheck={false}
        onChange={(event) => onChange(linesToArray(event.target.value))}
      />
    );
  }
  if (spec.kind === "integer" || spec.kind === "number") {
    return (
      <input
        type="number"
        className="ov-input"
        aria-label={spec.label}
        value={typeof value === "number" && Number.isFinite(value) ? String(value) : ""}
        min={spec.min}
        max={spec.max}
        step={spec.kind === "integer" ? 1 : "any"}
        disabled={disabled}
        placeholder={spec.placeholder}
        onChange={(event) => {
          const text = event.target.value;
          if (text === "") {
            onChange(undefined);
            return;
          }
          const parsed = Number(text);
          if (Number.isFinite(parsed)) onChange(parsed);
        }}
      />
    );
  }
  return (
    <input
      type="text"
      className="ov-input"
      aria-label={spec.label}
      value={typeof value === "string" ? value : ""}
      disabled={disabled}
      placeholder={spec.placeholder}
      spellCheck={false}
      onChange={(event) => onChange(event.target.value)}
    />
  );
}

export function Field({
  spec,
  value,
  disabled,
  secret,
  onSecretChange,
  onChange,
  issues,
}: FieldControlProps): ReactNode {
  const fieldIssues = issuesAtPath(issues, spec.path);
  return (
    <div className={`sr-field${spec.kind === "lines" ? " sr-field--wide" : ""}`}>
      <label className="sr-field__label">
        <span>
          {spec.label}
          {spec.required ? <span className="sr-required"> *</span> : null}
          {spec.unit ? <span className="sr-field__unit">（{spec.unit}）</span> : null}
        </span>
        <FieldControl
          spec={spec}
          value={value}
          disabled={disabled}
          secret={secret}
          onSecretChange={onSecretChange}
          onChange={onChange}
          issues={issues}
        />
      </label>
      {spec.help ? <span className="sr-field__help">{spec.help}</span> : null}
      <FieldIssues issues={fieldIssues} />
    </div>
  );
}

export function FieldGrid({
  fields,
  value,
  issues,
  disabled,
  secrets,
  onSecretChange,
  onChange,
}: {
  fields: FieldSpec[];
  value: unknown;
  issues: SettingsIssue[];
  disabled?: boolean;
  secrets?: Record<string, string>;
  onSecretChange?: (path: Path, value: string) => void;
  onChange: (path: Path, value: unknown) => void;
}): ReactNode {
  return (
    <div className="sr-field-grid">
      {fields.map((spec) => (
        <Field
          key={pathPointer(spec.path)}
          spec={spec}
          value={getAtPath(value, spec.path)}
          issues={issues}
          disabled={disabled}
          secret={secrets?.[secretKey(spec.path)]}
          onSecretChange={
            onSecretChange
              ? (next) => onSecretChange(spec.path, next)
              : undefined
          }
          onChange={(next) => onChange(spec.path, next)}
        />
      ))}
    </div>
  );
}

export function SecretRefEditor({
  label,
  value,
  secret,
  issues,
  disabled,
  required,
  removable,
  scenario = true,
  onRemove,
  onConfigure,
  onChange,
  onSecretChange,
}: {
  label: string;
  value: unknown;
  secret: string;
  issues: SettingsIssue[];
  disabled?: boolean;
  required?: boolean;
  removable?: boolean;
  scenario?: boolean;
  onRemove?: () => void;
  onConfigure?: () => void;
  onChange: (next: Record<string, unknown>) => void;
  onSecretChange: (next: string) => void;
}): ReactNode {
  const record = asRecord(value);
  if (!record) {
    return (
      <div className="sr-secret">
        <div className="sr-secret__head">
          <span className="sr-secret__name">{label}</span>
          <span className="sr-secret__present">未配置</span>
        </div>
        <p className="sr-field__help">
          只保存 secret_ref；值只写不读，不进入界面状态存储、日志或持久化。
        </p>
        {onConfigure ? (
          <button
            type="button"
            className="ov-btn ov-btn--sm"
            disabled={disabled}
            onClick={onConfigure}
          >
            配置引用
          </button>
        ) : null}
      </div>
    );
  }
  const ref = typeof record.secret_ref === "string" ? record.secret_ref : "";
  const present = record.present === true;
  return (
    <div className="sr-secret">
      <div className="sr-secret__head">
        <span className="sr-secret__name">
          {label}
          {required ? <span className="sr-required"> *</span> : null}
        </span>
        <span className={`sr-secret__present${present ? " sr-secret__present--set" : ""}`}>
          {present ? "已设置 ●●●●" : "未设置"}
        </span>
        {removable && onRemove ? (
          <button
            type="button"
            className="ov-btn ov-btn--sm ov-btn--danger-ghost"
            disabled={disabled}
            onClick={onRemove}
          >
            移除
          </button>
        ) : null}
      </div>
      <label className="sr-field">
        <span className="sr-field__label">secret_ref</span>
        <input
          type="text"
          className="ov-input ov-mono"
          aria-label={`${label} secret_ref`}
          value={ref}
          disabled={disabled}
          spellCheck={false}
          onChange={(event) =>
            onChange({ ...record, secret_ref: event.target.value })
          }
        />
      </label>
      <label className="sr-field">
        <span className="sr-field__label">新值（只写）</span>
        <input
          type="password"
          className="ov-input"
          aria-label={`${label} 新值`}
          autoComplete="new-password"
          spellCheck={false}
          value={secret}
          disabled={disabled}
          placeholder={
            scenario
              ? "仅测试占位值，例如 test-placeholder；勿输入真实凭据"
              : "输入新值以轮换；只写不读"
          }
          onChange={(event) => onSecretChange(event.target.value)}
        />
      </label>
      <span className="sr-field__help">
        {scenario
          ? "仅测试占位值，保存预览发出后立即从界面清除；不进入全局状态、持久化或日志。"
          : "值只写不读；保存预览发出后立即从界面清除，不进入日志或持久化。"}
      </span>
      <FieldIssues issues={issues} />
    </div>
  );
}

function EnumListEditor({
  options,
  value,
  disabled,
  ariaLabel,
  onChange,
}: {
  options: string[];
  value: unknown;
  disabled?: boolean;
  ariaLabel: string;
  onChange: (next: string[]) => void;
}): ReactNode {
  const selected = Array.isArray(value) ? value.map(String) : [];
  const unknown = selected.filter((entry) => !options.includes(entry));
  return (
    <div className="sr-check-list" role="group" aria-label={ariaLabel}>
      {options.map((option) => (
        <label className="sr-check-list__item" key={option}>
          <input
            type="checkbox"
            checked={selected.includes(option)}
            disabled={disabled}
            onChange={(event) => {
              const next = event.target.checked
                ? [...selected, option]
                : selected.filter((entry) => entry !== option);
              onChange(next);
            }}
          />
          <span>{option}</span>
        </label>
      ))}
      {unknown.map((option) => (
        <label className="sr-check-list__item" key={option}>
          <input type="checkbox" checked disabled readOnly />
          <span>{option}（当前值不在目录中）</span>
        </label>
      ))}
    </div>
  );
}

function WhenEditor({
  value,
  disabled,
  onChange,
  labelPrefix,
}: {
  value: unknown;
  disabled?: boolean;
  onChange: (next: Record<string, unknown>) => void;
  labelPrefix: string;
}): ReactNode {
  const record = asRecord(value) ?? {};
  const kind =
    record.paths !== undefined
      ? "paths"
      : record.diff !== undefined
        ? "diff"
        : record.labels !== undefined
          ? "labels"
          : "";
  const selectKind = (next: string) => {
    if (next === "paths") onChange({ paths: [] });
    else if (next === "labels") onChange({ labels: [] });
    else if (next === "diff") onChange({ diff: { files_gte: 1, lines_gte: 1 } });
    else onChange({});
  };
  const diff = asRecord(record.diff) ?? {};
  return (
    <div className="sr-when">
      <select
        className="ov-select ov-select--sm"
        aria-label={`${labelPrefix} 条件类型`}
        value={kind}
        disabled={disabled}
        onChange={(event) => selectKind(event.target.value)}
      >
        <option value="">未配置</option>
        <option value="paths">路径</option>
        <option value="diff">diff 规模</option>
        <option value="labels">标签</option>
      </select>
      {kind === "paths" || kind === "labels" ? (
        <textarea
          className="ov-textarea sr-lines sr-lines--sm"
          aria-label={`${labelPrefix} ${kind === "paths" ? "路径 globs" : "标签"}`}
          value={
            Array.isArray(record[kind])
              ? (record[kind] as unknown[]).map(String).join("\n")
              : ""
          }
          disabled={disabled}
          placeholder="每行一项"
          spellCheck={false}
          onChange={(event) =>
            onChange({ ...record, [kind]: linesToArray(event.target.value) })
          }
        />
      ) : null}
      {kind === "diff" ? (
        <div className="sr-when__diff">
          <label className="sr-inline-field">
            <span>文件数 ≥</span>
            <input
              type="number"
              className="ov-input ov-input--sm"
              aria-label={`${labelPrefix} 改动文件数下限`}
              min={1}
              step={1}
              value={typeof diff.files_gte === "number" ? String(diff.files_gte) : ""}
              disabled={disabled}
              onChange={(event) => {
                const text = event.target.value;
                const nextDiff = { ...diff };
                if (text === "") delete nextDiff.files_gte;
                else nextDiff.files_gte = Number(text);
                onChange({ ...record, diff: nextDiff });
              }}
            />
          </label>
          <label className="sr-inline-field">
            <span>行数 ≥</span>
            <input
              type="number"
              className="ov-input ov-input--sm"
              aria-label={`${labelPrefix} 改动行数下限`}
              min={1}
              step={1}
              value={typeof diff.lines_gte === "number" ? String(diff.lines_gte) : ""}
              disabled={disabled}
              onChange={(event) => {
                const text = event.target.value;
                const nextDiff = { ...diff };
                if (text === "") delete nextDiff.lines_gte;
                else nextDiff.lines_gte = Number(text);
                onChange({ ...record, diff: nextDiff });
              }}
            />
          </label>
        </div>
      ) : null}
    </div>
  );
}

function TableCell({
  column,
  entry,
  entryKey,
  tablePath,
  value,
  secrets,
  disabled,
  scenario,
  issues,
  onSecretChange,
  onChange,
}: {
  column: TableColumnSpec;
  entry: Record<string, unknown>;
  entryKey: string;
  tablePath: Path;
  value: unknown;
  secrets: Record<string, string>;
  disabled?: boolean;
  scenario?: boolean;
  issues: SettingsIssue[];
  onSecretChange: (path: Path, next: string) => void;
  onChange: (path: Path, next: unknown) => void;
}): ReactNode {
  const cellPath = column.path ?? [column.key];
  const fullPath = [...tablePath, entryKey, ...cellPath];
  const current = getAtPath(entry, cellPath);
  const locked = column.lockedValue?.(entryKey);
  if (locked !== undefined) {
    return (
      <span className="sr-readonly">
        {column.lockedText?.(entryKey) ?? formatInline(locked)}
      </span>
    );
  }
  if (column.kind === "secret-ref") {
    const secret = secrets[secretKey(fullPath)] ?? "";
    return (
      <SecretRefEditor
        label={column.label}
        value={current}
        secret={secret}
        issues={issues}
        disabled={disabled}
        required={column.required}
        scenario={scenario}
        onConfigure={() => onChange(fullPath, { secret_ref: "", present: false })}
        onChange={(next) => onChange(fullPath, next)}
        onSecretChange={(next) => onSecretChange(fullPath, next)}
      />
    );
  }
  if (column.kind === "enum-list") {
    return (
      <EnumListEditor
        options={resolveOptions(column, entry)}
        value={current}
        disabled={disabled}
        ariaLabel={`${entryKey} ${column.label}`}
        onChange={(next) => onChange(fullPath, next)}
      />
    );
  }
  if (column.kind === "when") {
    return (
      <div>
        <WhenEditor
          value={current}
          disabled={disabled}
          labelPrefix={`${entryKey} ${column.label}`}
          onChange={(next) => onChange(fullPath, next)}
        />
        <FieldIssues issues={issues} />
      </div>
    );
  }
  if (column.kind === "boolean") {
    return (
      <div>
        <input
          type="checkbox"
          className="sr-checkbox"
          aria-label={`${entryKey} ${column.label}`}
          checked={current === true}
          disabled={disabled}
          onChange={(event) => onChange(fullPath, event.target.checked)}
        />
        <FieldIssues issues={issues} />
      </div>
    );
  }
  if (column.kind === "lines") {
    return (
      <div>
        <textarea
          className="ov-textarea sr-lines sr-lines--sm"
          aria-label={`${entryKey} ${column.label}`}
          value={
            Array.isArray(current) ? current.map(String).join("\n") : ""
          }
          disabled={disabled}
          placeholder={column.placeholder ?? "每行一项"}
          spellCheck={false}
          onChange={(event) =>
            onChange(fullPath, linesToArray(event.target.value))
          }
        />
        <FieldIssues issues={issues} />
      </div>
    );
  }
  if (column.kind === "enum") {
    const options = resolveOptions(column, entry);
    const selected = typeof current === "string" ? current : "";
    return (
      <div>
        <select
          className="ov-select ov-select--sm"
          aria-label={`${entryKey} ${column.label}`}
          value={selected}
          disabled={disabled}
          onChange={(event) => onChange(fullPath, event.target.value || undefined)}
        >
          {!selected ? <option value="">未配置</option> : null}
          {selected && !options.includes(selected) ? (
            <option value={selected}>{selected}（未列出）</option>
          ) : null}
          {options.map((option) => (
            <option value={option} key={option}>
              {option}
            </option>
          ))}
        </select>
        <FieldIssues issues={issues} />
      </div>
    );
  }
  if (column.kind === "integer" || column.kind === "number") {
    return (
      <div>
        <input
          type="number"
          className="ov-input ov-input--sm"
          aria-label={`${entryKey} ${column.label}`}
          min={column.min}
          max={column.max}
          step={column.kind === "integer" ? 1 : "any"}
          value={typeof current === "number" ? String(current) : ""}
          disabled={disabled}
          onChange={(event) => {
            const text = event.target.value;
            if (text === "") {
              onChange(fullPath, undefined);
              return;
            }
            const parsed = Number(text);
            if (Number.isFinite(parsed)) onChange(fullPath, parsed);
          }}
        />
        <FieldIssues issues={issues} />
      </div>
    );
  }
  return (
    <div>
      <input
        type="text"
        className="ov-input ov-input--sm"
        aria-label={`${entryKey} ${column.label}`}
        value={typeof current === "string" ? current : ""}
        disabled={disabled}
        placeholder={column.placeholder}
        spellCheck={false}
        onChange={(event) => onChange(fullPath, event.target.value)}
      />
      <FieldIssues issues={issues} />
    </div>
  );
}

export function RecordTable({
  spec,
  value,
  issues,
  secrets,
  disabled,
  scenario,
  onSecretChange,
  onChange,
}: {
  spec: TableSpec;
  value: unknown;
  issues: SettingsIssue[];
  secrets: Record<string, string>;
  disabled?: boolean;
  scenario?: boolean;
  onSecretChange: (path: Path, next: string) => void;
  onChange: (path: Path, next: unknown) => void;
}): ReactNode {
  const record = asRecord(getAtPath(value, spec.path)) ?? {};
  const entries = Object.entries(record);
  const fixed = new Set(spec.fixedKeys ?? []);
  const addEntry = () => {
    let index = 1;
    let key = `new_${spec.entryLabel}_${index}`;
    while (record[key] !== undefined) {
      index += 1;
      key = `new_${spec.entryLabel}_${index}`;
    }
    const created = spec.createEntry?.(record) ?? {};
    onChange(spec.path, { ...record, [key]: created });
  };
  const rename = (oldKey: string, nextKey: string) => {
    const trimmed = nextKey.trim();
    if (!trimmed || trimmed === oldKey || record[trimmed] !== undefined) return;
    const renamed: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(record)) {
      renamed[key === oldKey ? trimmed : key] = entry;
    }
    onChange(spec.path, renamed);
  };
  return (
    <div className="sr-table-block">
      <div className="ov-settings__section-head">
        <h2 className="ov-settings__h2">{spec.title}</h2>
        <button
          type="button"
          className="ov-btn ov-btn--sm"
          disabled={disabled}
          onClick={addEntry}
        >
          {spec.addLabel ?? `新增${spec.entryLabel}`}
        </button>
      </div>
      {spec.description ? (
        <p className="ov-settings__muted">{spec.description}</p>
      ) : null}
      <div
        className="ov-table-wrap sr-table-wrap"
        tabIndex={0}
        role="region"
        aria-label={spec.title}
      >
        <table className="ov-table sr-table">
          <thead>
            <tr>
              <th>{spec.entryLabel}</th>
              {spec.columns.map((column) => (
                <th key={column.key}>{column.label}</th>
              ))}
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            {entries.length === 0 ? (
              <tr>
                <td colSpan={spec.columns.length + 2} className="ov-settings__muted">
                  {spec.emptyText ?? "暂无条目。"}
                </td>
              </tr>
            ) : (
              entries.map(([key, rawEntry]) => {
                const entry = asRecord(rawEntry) ?? {};
                const isFixed = fixed.has(key);
                return (
                  <tr key={key}>
                    <td>
                      {isFixed ? (
                        <span className="ov-mono">{key}</span>
                      ) : (
                        <input
                          type="text"
                          className="ov-input ov-input--sm ov-mono"
                          aria-label={`${key} 名称`}
                          defaultValue={key}
                          disabled={disabled}
                          onBlur={(event) => rename(key, event.target.value)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") event.currentTarget.blur();
                          }}
                        />
                      )}
                      {isFixed ? (
                        <span className="sr-fixed-tag">固定项</span>
                      ) : null}
                    </td>
                    {spec.columns.map((column) => {
                      const cellPath = [...spec.path, key, ...(column.path ?? [column.key])];
                      const cellIssues = issuesAtPath(issues, cellPath);
                      return (
                        <td key={column.key}>
                          <TableCell
                            column={column}
                            entry={entry}
                            entryKey={key}
                            tablePath={spec.path}
                            value={value}
                            secrets={secrets}
                            disabled={disabled}
                            scenario={scenario}
                            issues={cellIssues}
                            onSecretChange={onSecretChange}
                            onChange={onChange}
                          />
                          {column.help ? (
                            <span className="sr-field__help">{column.help}</span>
                          ) : null}
                        </td>
                      );
                    })}
                    <td>
                      <button
                        type="button"
                        className="ov-btn ov-btn--sm ov-btn--danger-ghost"
                        aria-label={`删除 ${key}`}
                        disabled={disabled || isFixed}
                        onClick={() => {
                          const next = { ...record };
                          delete next[key];
                          onChange(spec.path, next);
                        }}
                      >
                        删除
                      </button>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
      <TableLevelIssues issues={issues} base={spec.path} />
    </div>
  );
}

function TableLevelIssues({
  issues,
  base,
}: {
  issues: SettingsIssue[];
  base: Path;
}): ReactNode {
  const prefix = pathPointer(base);
  const relevant = issues.filter(
    (issue) => issue.pointer === prefix || issue.pointer.startsWith(`${prefix}/`),
  );
  if (relevant.length === 0) return null;
  return (
    <div className="sr-table-issues" role="alert">
      {relevant.map((issue) => (
        <span key={`${issue.pointer}-${issue.code}-${issue.message ?? ""}`}>
          {issue.pointer || "（整体）"}：{issue.message ?? issueText(issue)}
        </span>
      ))}
    </div>
  );
}

/** 项目覆盖页可复用的只读继承 + 覆盖输入行。 */
export function ProjectOverrideFieldRow({
  field,
  effectiveValue,
  overrideValue,
  disabled,
  onChange,
  onClear,
}: {
  field: ProjectOverrideField;
  effectiveValue: unknown;
  overrideValue: unknown;
  disabled?: boolean;
  onChange: (next: unknown) => void;
  onClear: () => void;
}): ReactNode {
  const hasOverride = overrideValue !== undefined;
  return (
    <div className="sr-project-row">
      <div className="sr-project-row__label">
        <span>{field.label}</span>
        {field.tightenOnly ? (
          <span className="sr-tighten">只能收紧</span>
        ) : null}
      </div>
      <span className="sr-project-row__effective" title="生效值（只读）">
        {formatInline(effectiveValue)}
      </span>
      <div className="sr-project-row__control">
        {field.kind === "boolean" ? (
          <select
            className="ov-select ov-select--sm"
            aria-label={`${field.label} 项目覆盖`}
            value={hasOverride ? (overrideValue ? "true" : "false") : ""}
            disabled={disabled}
            onChange={(event) => {
              if (event.target.value === "") onChange(undefined);
              else onChange(event.target.value === "true");
            }}
          >
            <option value="">继承全局</option>
            <option value="true">是</option>
            <option value="false">否</option>
          </select>
        ) : field.kind === "enum" ? (
          <select
            className="ov-select ov-select--sm"
            aria-label={`${field.label} 项目覆盖`}
            value={typeof overrideValue === "string" ? overrideValue : ""}
            disabled={disabled}
            onChange={(event) => onChange(event.target.value || undefined)}
          >
            <option value="">继承全局</option>
            {(field.options ?? []).map((option) => (
              <option value={option} key={option}>
                {option}
              </option>
            ))}
          </select>
        ) : field.kind === "lines" ? (
          <textarea
            className="ov-textarea sr-lines sr-lines--sm"
            aria-label={`${field.label} 项目覆盖`}
            value={
              Array.isArray(overrideValue)
                ? overrideValue.map(String).join("\n")
                : ""
            }
            disabled={disabled}
            placeholder={`继承：${formatInline(effectiveValue)}`}
            spellCheck={false}
            onChange={(event) => {
              const lines = linesToArray(event.target.value);
              onChange(lines.length === 0 && !field.keepEmptyList ? undefined : lines);
            }}
          />
        ) : (
          <input
            type={field.kind === "integer" || field.kind === "number" ? "number" : "text"}
            className="ov-input ov-input--sm"
            aria-label={`${field.label} 项目覆盖`}
            value={
              typeof overrideValue === "number" || typeof overrideValue === "string"
                ? String(overrideValue)
                : ""
            }
            min={field.min}
            max={field.max}
            step={field.kind === "integer" ? 1 : "any"}
            disabled={disabled}
            placeholder={`继承：${formatInline(effectiveValue)}`}
            onChange={(event) => {
              const text = event.target.value;
              if (text === "") {
                onChange(undefined);
                return;
              }
              if (field.kind === "integer" || field.kind === "number") {
                const parsed = Number(text);
                if (Number.isFinite(parsed)) onChange(parsed);
              } else {
                onChange(text);
              }
            }}
          />
        )}
      </div>
      <button
        type="button"
        className="ov-btn ov-btn--sm"
        disabled={disabled || !hasOverride}
        onClick={onClear}
      >
        恢复继承
      </button>
    </div>
  );
}

/** 六类项目覆盖字段：只列 config.md §2.2 允许的键。 */
export function projectOverrideFields(
  resource: ProjectResource,
  catalog: SettingsCatalog | null,
  globalValue?: unknown,
): ProjectOverrideField[] {
  const fields: ProjectOverrideField[] = [];
  const push = (
    path: Path,
    label: string,
    kind: FieldKind,
    extra: Partial<ProjectOverrideField> = {},
  ) => {
    fields.push({ path, label, kind, tightenOnly: false, ...extra });
  };
  if (resource === "models") {
    for (const tier of ["planner", "worker", "utility"] as const) {
      const tierLabel = tier === "planner" ? "大脑" : tier === "worker" ? "干活" : "打杂";
      push([tier, "provider"], `${tierLabel} 提供方`, "text");
      push([tier, "model"], `${tierLabel} 模型`, "text");
      const efforts = new Set<string>();
      for (const capability of Object.values(catalog?.models.capabilities ?? {})) {
        for (const effort of capability.reasoning_efforts) efforts.add(effort);
      }
      if (tier !== "utility") efforts.add("by_level");
      push([tier, "reasoning_effort"], `${tierLabel} 推理深度`, "enum", {
        options: [...efforts],
      });
      push([tier, "adapter"], `${tierLabel} 适配器`, "text");
      for (const key of [
        "request_timeout_s",
        "total_timeout_s",
        "attempt_wall_s",
        "max_turns",
      ]) {
        push([tier, key], `${tierLabel} ${key}`, "integer", { min: 1 });
      }
    }
  } else if (resource === "levels") {
    push(["default_level"], "默认档位上限", "enum", { options: LEVELS });
    push(["none", "daily_tokens"], "None 档每日 token 上限", "integer", {
      min: 1,
      tightenOnly: true,
    });
    for (const level of BUDGET_LEVELS) {
      for (const key of [
        "tokens_by_tier.utility",
        "tokens_by_tier.planner",
        "tokens_by_tier.worker",
        "nodes",
        "rounds",
        "wall_time_s",
        "concurrency",
        "forge_writes",
        "forge_writes_per_hour",
      ]) {
        push(
          [level, "budget", ...key.split(".")],
          `${level} ${key}`,
          "integer",
          { min: 1, tightenOnly: true },
        );
      }
    }
  } else if (resource === "policies") {
    for (const [id, definition] of Object.entries(
      catalog?.policies?.actions ?? {},
    )) {
      push(["actions", id], `动作 ${id}`, "enum", {
        options: definition.allowed,
        tightenOnly: true,
      });
    }
    push(["unlisted_action"], "未列动作兜底", "enum", {
      options: ["approve", "deny"],
      tightenOnly: true,
    });
    push(["approval", "ttl_s"], "审批有效期", "integer", {
      min: 60,
      max: 604_800,
      tightenOnly: true,
    });
    for (const key of [
      "max_files",
      "max_bytes",
      "max_changed_lines",
      "max_file_bytes",
    ]) {
      push(["patch", key], `补丁 ${key}`, "integer", {
        min: 1,
        tightenOnly: true,
      });
    }
    push(["escalation", "enabled"], "自动加码开关", "boolean", {
      tightenOnly: true,
    });
  } else if (resource === "guardian") {
    for (const [id, definition] of Object.entries(
      catalog?.guardian?.triage ?? {},
    )) {
      if (definition.locked) continue;
      push(["triage", id], `分流 ${id}`, "enum", {
        options: definition.allowed,
        tightenOnly: true,
      });
    }
    for (const key of ["same_class_repeats", "stalled_rounds"]) {
      push(["no_progress", key], `无进展 ${key}`, "integer", {
        min: 1,
        tightenOnly: true,
      });
    }
    push(["status_checklist", "enabled"], "状态清单开关", "boolean");
    push(
      ["status_checklist", "min_update_interval_s"],
      "状态清单最小更新间隔",
      "integer",
      { min: 0 },
    );
  } else if (resource === "sandbox") {
    for (const key of ["cpu_cores", "mem_mb", "disk_mb", "pids", "wall_time_s"]) {
      push(["limits", key], `限制 ${key}`, key === "cpu_cores" ? "number" : "integer", {
        min: key === "cpu_cores" ? 0.1 : 1,
        tightenOnly: true,
      });
    }
    push(["devcontainer", "mode"], "devcontainer 模式", "enum", {
      options: ["off", "safe_subset"],
      tightenOnly: true,
    });
  } else if (resource === "egress") {
    const profiles = asRecord(asRecord(globalValue)?.profiles) ?? {};
    for (const name of Object.keys(profiles)) {
      push(["profiles", name, "groups"], `档案 ${name} 的分组`, "lines", {
        tightenOnly: true,
        help: "只能收窄为全局档案分组的子集；每行一个分组名。",
      });
    }
  }
  return fields;
}

/** 项目页的动态表：仍只使用 §2.2 允许覆盖的子树。 */
export function projectOverrideTables(
  resource: ProjectResource,
): ProjectOverrideTable[] {
  if (resource === "policies") {
    return [
      {
        path: ["paths"],
        tightenOnly: true,
        title: "路径规则覆盖",
        entryLabel: "规则 id",
        addLabel: "新增路径规则",
        description:
          "可新增 id 或增加 globs / 提高处置；workflows 与 sensitive_files 固定 deny。",
        fixedKeys: ["workflows", "sensitive_files"],
        createEntry: () => ({ globs: [], disposition: "approve" }),
        columns: [
          { key: "globs", label: "globs", kind: "lines", placeholder: "每行一项" },
          {
            key: "disposition",
            label: "处置",
            kind: "enum",
            options: ["approve", "deny"],
          },
        ],
      },
      {
        path: ["escalation", "rules"],
        tightenOnly: true,
        title: "自动加码规则覆盖",
        entryLabel: "规则 id",
        addLabel: "新增加码规则",
        description: "只能收紧或新增；when 恰好一种条件。",
        createEntry: () => ({ when: { paths: [] }, raise_to: "High" }),
        columns: [
          { key: "when", label: "when", kind: "when" },
          {
            key: "raise_to",
            label: "raise_to",
            kind: "enum",
            options: ["Medium", "High", "Max"],
          },
        ],
      },
    ];
  }
  return [];
}

export function inheritedValueText(value: unknown): string {
  return formatInline(value);
}

export function isProjectOverridable(
  resource: ProjectResource,
  path: Path,
  catalog: SettingsCatalog | null,
  globalValue?: unknown,
): boolean {
  const pointer = pathPointer(path);
  return (
    projectOverrideFields(resource, catalog, globalValue).some(
      (field) => pathPointer(field.path) === pointer,
    ) ||
    projectOverrideTables(resource).some(
      (table) =>
        pathPointer(path) === pathPointer(table.path) ||
        pointer.startsWith(`${pathPointer(table.path)}/`),
    )
  );
}
