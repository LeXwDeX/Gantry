# ConfigView.injection 视图（送审草案）

> 状态：送审，尚未批准，非权威；M0 定义契约与校验类型，M1 控制面仍拒绝非空，M2 才启用私有包仓库代理；当前运行时仍拒绝非空。
> 来源：以下正文逐字摘自 [M0 契约冻结提案 §3.2](../m0-contract-proposal.md)，含原编号。
> 范围：本补充只送审 `ConfigView.injection` 非空视图；不新增配置键、路由或错误码，不改变 settings 读写契约。

### 3.2 推荐规格

引用结构及下列四项选择已确认：字段名与语义沿用 settings 的 `hosts` / `kind` / `header`，但视图使用独立的 `InjectionView` / `InjectionCredentialView`；`credential` 仅含 `{secret_ref}`，不复用接受 `present` 的 `SecretRefDoc`。以下 JSON 与字段表是推荐规格，完整契约仍需独立契约 PR 审查，不是生产契约：

```json
{
  "injection": {
    "npm_private": {
      "hosts": ["npm.corp.example"],
      "kind": "bearer",
      "credential": { "secret_ref": "registry.npm.corp" }
    },
    "cargo_private": {
      "hosts": ["cargo.corp.example"],
      "kind": "header",
      "header": "x-api-key",
      "credential": { "secret_ref": "registry.cargo.corp" }
    }
  }
}
```

#### 3.2.1 字段表

| 字段 | 必填 | 类型与来源 | 约束 |
| --- | --- | --- | --- |
| `injection` | 是 | `{id → InjectionView}`；空对象合法 | `id` 是配置键，不是实体 ID，不新增前缀。推荐 `^[a-z0-9][a-z0-9._-]{0,63}$`，与 `SecretName` 同一规则；依据是该 id 会成为 `/r/<job_token>/<injection_id>/…` 的路径段（`egress-control §4.6`），必须 URL 安全。大小写敏感、不得含 `/`、不得为空。 |
| `injection.<id>.hosts` | 是 | `string[]`，非空 | 每项是 DNS 主机名（无协议、端口、路径）；允许一层前缀通配 `*.example.com`；去重并保留顺序。必须被某个分组的 `hosts` 覆盖（V13 原文）；推荐再收紧为被 `profiles` 引用的分组覆盖。不得含 GitHub 域名族（V10 原文）；IP 字面量、裸 `*`、多层通配由 `egress` G5 与运行时规则禁止。 |
| `injection.<id>.kind` | 是 | `bearer` / `header` | 决定注入头。 |
| `injection.<id>.header` | 条件 | 非空字符串 | `kind=bearer` 时必须省略（包括 `null`）；`kind=header` 时必须提供。使用 HTTP field-name token 语法；大小写不敏感拒绝逐跳/框架头：`Host`、`Content-Length`、`Transfer-Encoding`、`Connection`、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Upgrade`。`header` 模式下也拒绝 `Authorization`（推荐增补；V13 原文只列逐跳/框架头），避免与 `bearer` 的隐式头重叠。 |
| `injection.<id>.credential` | 是 | `InjectionCredentialView`：`{secret_ref}` | 只含 `secret_ref`；满足 `^[a-z0-9][a-z0-9._-]{0,63}$`。不得含 `value`、`present` 或字符串引用；视图永不输出 secret 值。`SecretRefDoc` 接受 `present`，因此不能直接复用为视图类型。 |
| `present` | — | 不出现在 ConfigView | settings 的 GET 响应有 `present`；下发视图只含引用。 |
| 模型 provider | — | 不重复出现在 `injection` | 由 `models.providers` 自动派生为只读注入项；`injection` 只放私有包仓库。 |

`kind=bearer` 的隐式头名按 `Authorization` 计；`kind=header` 使用显式头名。`(host, header)` 唯一性按大小写不敏感的最终头名判定，因此同一 host 上不能有两个 `bearer`，也不能有重复的自定义头名。

#### 3.2.2 校验与错误

校验按以下顺序执行，前一步失败不继续推断后一步：

1. **形态**：对象、必填字段、类型、未知字段拒绝（`additionalProperties:false`）。
2. **V10**：`injection.hosts` 不得含 GitHub 域名族（V10 原文）。
3. **`egress` G5**：不得含 IP 字面量、裸 `*`、多层通配。
4. **V13**：每个 host 必须被某个分组覆盖（推荐再要求该分组被 `profiles` 引用）；`(host, header)` 唯一；`header` 不是逐跳/框架头。
5. **引用**：`credential` 只校验 `secret_ref` 格式；不解析、不回显 secret 值。

V11 是契约要求 Egress 对视图 `providers` 执行的复核，不因本节只讨论 `injection` 而免除；当前核心校验尚未实现 V11，属于契约 PR 后的实现工作。

错误映射：

| 情形 | 响应 | 位置 |
| --- | --- | --- |
| 字段缺失、类型错误、未知字段、`header` 与 `kind` 不匹配、`credential` 为字符串或含未知字段 | `422 validation_failed`（settings 写入）或 `422 config_invalid`（视图复核） | `errors[].pointer` 相对被校验对象；视图复核时相对视图，如 `/injection/npm_private/header`。 |
| 视图中的 `credential` 含 `value` 或 `present` | `422 config_invalid` | settings 写入仍允许 `value` / `present`；只在只读下发视图中拒绝。 |
| host 不在分组、`(host, header)` 重复、逐跳/框架头、GitHub 域名族/IP/裸 `*` | `422 config_invalid` | 同上；保留旧视图，`health=degraded`。 |
| `secret_ref` 格式不合法 | `422 validation_failed` / `422 config_invalid` | 指向 `…/credential/secret_ref`。 |
| `secret_ref` 合法但 secret 未激活 | 视图仍可应用 | 状态响应 `unresolved_secret_refs` 列出引用（不列值），`health=degraded`；不是 `config_invalid`。 |

`credential_missing` 仍只用于签发令牌时 provider 凭据未激活（`egress-control §6`），不用于视图校验。

#### 3.2.3 正负例

| 正例 | 关键负例 |
| --- | --- |
| `npm_private` 的 `bearer` 示例；`cargo_private` 的 `header` 示例；两个不同 host 各一个 `bearer` | `injection` 不是对象；id 含 `/`、大写或为空；`hosts` 缺失/空/含 `api.github.com`/`127.0.0.1`/`*`/`**.example.com`/`https://npm.corp.example`/`npm.corp.example:443`；host 不在分组；`kind` 缺失或未知；`kind=bearer` 带 `header`（包括 `null`）；`kind=header` 缺 `header` 或 `header` 为空、为 `Host` / `Content-Length` / `Transfer-Encoding` / `Authorization`；`credential` 缺失、为字符串、带 `value` 或 `present`、`secret_ref` 非法；未知字段；同一 host 两个 `bearer`；同一 `(host, header)` 大小写不同但重复；视图含 secret 值。 |

#### 3.2.4 里程碑与现有约束

- 非空 `injection` 的启用阶段：M0 定义契约与校验类型，不启用代理能力；M1 继续拒绝非空；M2 才启用私有包仓库代理。
- 不新增 `injection_rev`；沿用整个 `ConfigView.rev`。
- 现有生产类型与 schema 仍拒绝非空 `injection`；只有契约批准并另获实施授权后才可能改变。当前 `ConfigView.injection` 用 `#[serde(default)]` 允许缺省为空对象；§3.2.1 将其列为必填是候选收紧，须在契约 PR 明确是否保留缺省。
- `settings` 侧读写契约不改：写入的 `credential` 仍可带 `value` 完成写入/轮换，读取可带 `present`；下发视图只含 `secret_ref`。
- 视图新增 `InjectionView` / `InjectionCredentialView`，不复用 `EgressInjection.credential`（即 `SecretRefDoc`，其反序列化接受 `present`）。settings 的 `EgressInjection` 与 `EgressInjectionPatch` 保持不变。`InjectionView` 与 `InjectionCredentialView` 都必须使用 `deny_unknown_fields` / `additionalProperties:false`，在反序列化和 schema 两层拒绝 `present` / `value`。
- settings GET 的 `present` 与当前 `SecretRefDoc.Serialize` 只输出 `secret_ref` 的差异是既有实现缺口，不因视图只含 `secret_ref` 而改变 settings 契约；由实现 PR 修复，不在本草案把两者混为一谈。
- “凭据只发往列出的主机”及复核失败行为是已有安全约束。本轮未接线，控制面行为须由后续另获授权的实现验证，不能用 serde/schema 校验冒充 HTTP 服务验证。

确认者：`X+C+U`。
