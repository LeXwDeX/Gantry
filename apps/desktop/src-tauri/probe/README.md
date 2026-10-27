# Secure Enclave 探针（#102）

回答 [m1-acceptance.md §4 注](../../../../docs/m1-acceptance.md#4-外部资源最小清单c-级) 的问题：**未签名 / ad-hoc 签名的开发构建，能否创建并使用带「用户在场」访问控制的 Secure Enclave 密钥，且重启后仍可用。**

## 结论（2026-10-27，Apple M4 Pro，macOS 27.2，无任何代码签名身份）

| 检查 | 结果 | 怎么验证的 |
| --- | --- | --- |
| 钥匙串持久化 SE 密钥（`kSecAttrIsPermanent = true`） | **不能**：`-34018 errSecMissingEntitlement` | 自动 |
| 非持久 SE 密钥 + `privateKeyUsage \| userPresence` | 能创建 | 自动 |
| 私钥导出 | 被拒（`-4 export not implemented`） | 自动 |
| SE 包装块（`kSecAttrTokenOID` / CryptoKit `dataRepresentation`）落盘，**新进程**恢复同一对密钥 | 能（公钥一致） | 自动 |
| mTLS 客户端密钥在"不允许交互"的 `LAContext` 下签名 | 成功，验签通过（无提示） | 自动 |
| 审批签名密钥在"不允许交互"的 `LAContext` 下签名 | **失败**（`LAError -1004 notInteractive`）：访问控制在密钥级生效，不会悄悄降级 | 自动 |
| 审批签名密钥经 Touch ID 后签名、验签 | 待手动 | 见下"手动验证" |

所以开发构建**不需要** Apple 签名证书就能拿到 B 级证据：两把密钥都建成非持久 SE 密钥，把 SE 加密的包装块存进应用数据目录（权限 0600）。包装块只能由本机 SE 解开，不是私钥明文；删除包装块即销毁密钥。`gantry-desktop-shell` 的 `SecureEnclaveKeyStore` 按这个方案实现，其硬件测试（`--ignored`）在本机通过。

注意两点：

1. `kSecAttrTokenOID`（键名 `"toid"`）不在 Security.framework 公开头文件里；CryptoKit 的 `SecureEnclave.P256.Signing.PrivateKey(dataRepresentation:)` 是同一机制的公开 API。若 Apple 改动，备选是用 Swift 小桥接调 CryptoKit。
2. 包装块文件不受钥匙串访问组保护：同一用户下的其他进程拿到文件，就能无提示地用 mTLS 客户端密钥签名（审批签名密钥仍要 Touch ID，但对方可以自拟提示文案）。签名发布版应改存钥匙串（带访问组授权），这条留给代码签名到位后处理（m1-acceptance §4 的 macOS 签名项）。

## 自动部分

```bash
# Swift 探针（与 Rust 实现无关的独立对照）
swiftc -O apps/desktop/src-tauri/probe/se-probe.swift -o /tmp/se-probe
/tmp/se-probe auto
/tmp/se-probe create /tmp/se-keys && /tmp/se-probe reload /tmp/se-keys

# Rust 实现的硬件测试（不弹框）
cargo test -p gantry-desktop-shell --all-features -- --ignored secure_enclave
```

全部输出 `PASS`、退出码 0 即通过。CI 的 macOS 虚拟机没有 SE，所以硬件测试默认 `#[ignore]`。

## 手动验证（需要人在场，不能冒充通过）

在本机终端（不是 SSH 会话）执行：

```bash
cargo run -p gantry-desktop-shell --example se_presence -- /tmp/gantry-se-manual
```

逐项记录：

1. 弹出 Touch ID，提示文案含 `merge acme/web#12 @ 9f2c4e7a1b3d`；按指纹 → 输出 `PASS 用户在场后审批签名成功且验签通过`。
2. 再运行一次（相当于重启应用）：输出"复用已有密钥"，两个指纹与第一次相同，再次弹 Touch ID 并通过。
3. 弹框时点"取消" → 输出 `UserCancelled`，没有签名。
4. 锁屏后（`pmset displaysleepnow`，或在弹框前锁屏）运行 → 不签名。
5. 连续按错手指直到回落密码 → 输入登录密码可通过（`userPresence` 允许密码回落，这是 P-47 的"用户在场"而非"仅生物识别"）。
6. 完成后删除 `/tmp/gantry-se-manual`。

Swift 探针的等价手动步骤：`/tmp/se-probe interactive /tmp/se-keys`。
