// Secure Enclave 最小探针（#102，m1-acceptance §4 注）。
//
// 问题：未签名 / 临时签名（ad-hoc）的开发构建能否创建并使用带「用户在场」访问控制的
// Secure Enclave 密钥，且重启后仍可用。
//
// 用法：
//   swiftc -O se-probe.swift -o se-probe
//   ./se-probe auto                 # 无需用户在场的检查，全部自动
//   ./se-probe create <dir>         # 生成两把密钥，把 SE 包装块写入 <dir>（不会弹 Touch ID）
//   ./se-probe reload <dir>         # 新进程中从 <dir> 恢复两把密钥并自动检查（不会弹 Touch ID）
//   ./se-probe interactive <dir>    # 手动：用审批密钥签名，会弹 Touch ID（需要人在场）
//
// 输出每行一个 `PASS` / `FAIL` / `INFO` 结论，退出码 = FAIL 数。

import CryptoKit
import Foundation
import LocalAuthentication
import Security

var failures = 0
func pass(_ s: String) { print("PASS  \(s)") }
func fail(_ s: String) { print("FAIL  \(s)"); failures += 1 }
func info(_ s: String) { print("INFO  \(s)") }

/// `kSecAttrTokenOID` 不在公开头文件里，键名是 "toid"（CryptoKit 的 dataRepresentation 即此值）。
let tokenOIDKey = "toid"

let message = Data("gantry/approval/v1\n{\"probe\":true}".utf8)

func presenceAcl() -> SecAccessControl {
    SecAccessControlCreateWithFlags(
        nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, [.privateKeyUsage, .userPresence], nil)!
}

func silentAcl() -> SecAccessControl {
    SecAccessControlCreateWithFlags(
        nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, [.privateKeyUsage], nil)!
}

/// 不允许交互的上下文：若密钥要求用户在场，签名必须失败而不是弹框。
func noInteractionContext() -> LAContext {
    let ctx = LAContext()
    ctx.interactionNotAllowed = true
    return ctx
}

func describe(_ error: Error) -> String {
    let ns = error as NSError
    return "\(ns.domain) \(ns.code): \(ns.localizedDescription)"
}

// MARK: - Security.framework 路径（Rust 实现将走这条路，security-framework crate）

func probeSecurityFramework() {
    // 1. 持久化到钥匙串的 SE 密钥：需要 keychain-access-groups 授权（entitlement）。
    var err: Unmanaged<CFError>?
    let permanentAttrs: [String: Any] = [
        kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
        kSecAttrKeySizeInBits as String: 256,
        kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
        kSecPrivateKeyAttrs as String: [
            kSecAttrIsPermanent as String: true,
            kSecAttrApplicationTag as String: Data("dev.gantry.probe.permanent".utf8),
            kSecAttrAccessControl as String: presenceAcl(),
        ] as [String: Any],
    ]
    if let key = SecKeyCreateRandomKey(permanentAttrs as CFDictionary, &err) {
        info("Security.framework 持久化 SE 密钥创建成功（本构建有钥匙串授权）")
        let del: [String: Any] = [
            kSecClass as String: kSecClassKey,
            kSecAttrApplicationTag as String: Data("dev.gantry.probe.permanent".utf8),
        ]
        SecItemDelete(del as CFDictionary)
        _ = key
    } else {
        let e = err!.takeRetainedValue() as Error
        info("Security.framework 持久化 SE 密钥（kSecAttrIsPermanent=true）失败：\(describe(e))")
    }

    // 2. 不持久化的 SE 密钥 + 用户在场访问控制。
    err = nil
    let ephemeralAttrs: [String: Any] = [
        kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
        kSecAttrKeySizeInBits as String: 256,
        kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
        kSecPrivateKeyAttrs as String: [
            kSecAttrIsPermanent as String: false,
            kSecAttrAccessControl as String: presenceAcl(),
        ] as [String: Any],
    ]
    guard let key = SecKeyCreateRandomKey(ephemeralAttrs as CFDictionary, &err) else {
        fail("Security.framework 非持久 SE 密钥 + userPresence 创建失败：\(describe(err!.takeRetainedValue() as Error))")
        return
    }
    pass("Security.framework 非持久 SE 密钥 + userPresence 可创建")

    // 3. 不可导出。
    err = nil
    if SecKeyCopyExternalRepresentation(key, &err) == nil {
        pass("SE 私钥导出被拒：\(describe(err!.takeRetainedValue() as Error))")
    } else {
        fail("SE 私钥竟可导出")
    }

    // 4. 包装块（token OID）能否经 Security.framework 恢复——决定 Rust 侧能否不靠 CryptoKit 持久化。
    let attrs = SecKeyCopyAttributes(key) as! [String: Any]
    if let toid = attrs[tokenOIDKey] as? Data {
        info("SecKeyCopyAttributes 含 kSecAttrTokenOID（\(toid.count) 字节）")
        err = nil
        let restoreAttrs: [String: Any] = [
            kSecAttrKeyType as String: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeyClass as String: kSecAttrKeyClassPrivate,
            kSecAttrKeySizeInBits as String: 256,
            kSecAttrTokenID as String: kSecAttrTokenIDSecureEnclave,
            tokenOIDKey: toid,
            kSecUseAuthenticationContext as String: noInteractionContext(),
        ]
        if let restored = SecKeyCreateWithData(Data() as CFData, restoreAttrs as CFDictionary, &err) {
            let samePub = SecKeyCopyExternalRepresentation(SecKeyCopyPublicKey(restored)!, nil) as Data?
                == SecKeyCopyExternalRepresentation(SecKeyCopyPublicKey(key)!, nil) as Data?
            samePub
                ? pass("Security.framework 可由 kSecAttrTokenOID 恢复同一把 SE 密钥（SecKeyCreateWithData）")
                : fail("由 kSecAttrTokenOID 恢复的密钥公钥不同")
            // 恢复时绑定"不允许交互"的上下文：带 userPresence 的密钥签名必须失败。
            err = nil
            if SecKeyCreateSignature(
                restored, .ecdsaSignatureMessageX962SHA256, message as CFData, &err) == nil
            {
                pass("Security.framework 恢复的审批密钥不经用户在场无法签名：\(describe(err!.takeRetainedValue() as Error))")
            } else {
                fail("Security.framework 恢复的审批密钥在不允许交互时竟签名成功")
            }
        } else {
            fail("SecKeyCreateWithData(kSecAttrTokenOID) 恢复失败：\(describe(err!.takeRetainedValue() as Error))")
        }
    } else {
        fail("SecKeyCopyAttributes 不含 kSecAttrTokenOID")
    }
}

// MARK: - CryptoKit 路径（dataRepresentation 由 SE 加密包装，可落盘，无需钥匙串授权）

struct Keys {
    let tls: SecureEnclave.P256.Signing.PrivateKey
    let approval: SecureEnclave.P256.Signing.PrivateKey
}

func createKeys() throws -> Keys {
    Keys(
        tls: try SecureEnclave.P256.Signing.PrivateKey(accessControl: silentAcl()),
        approval: try SecureEnclave.P256.Signing.PrivateKey(accessControl: presenceAcl()))
}

func checkKeys(_ keys: Keys, label: String) {
    if keys.tls.publicKey.rawRepresentation != keys.approval.publicKey.rawRepresentation {
        pass("\(label)：两把密钥公钥不同")
    } else {
        fail("\(label)：两把密钥公钥相同")
    }
    // mTLS 客户端密钥：不允许交互也能签名（后台无提示）。
    do {
        let k = try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: keys.tls.dataRepresentation, authenticationContext: noInteractionContext())
        let sig = try k.signature(for: message)
        keys.tls.publicKey.isValidSignature(sig, for: message)
            ? pass("\(label)：mTLS 客户端密钥无提示签名成功且验签通过")
            : fail("\(label)：mTLS 客户端密钥签名验签失败")
    } catch {
        fail("\(label)：mTLS 客户端密钥无提示签名失败：\(describe(error))")
    }
    // 审批签名密钥：不允许交互时必须失败（密钥级访问控制生效，且不会悄悄降级）。
    do {
        let k = try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: keys.approval.dataRepresentation, authenticationContext: noInteractionContext())
        _ = try k.signature(for: message)
        fail("\(label)：审批签名密钥在不允许交互时竟签名成功（访问控制未生效）")
    } catch {
        pass("\(label)：审批签名密钥不经用户在场无法签名：\(describe(error))")
    }
}

func write(_ keys: Keys, to dir: URL) throws {
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    try keys.tls.dataRepresentation.write(to: dir.appendingPathComponent("tls.seblob"))
    try keys.approval.dataRepresentation.write(to: dir.appendingPathComponent("approval.seblob"))
}

func read(from dir: URL) throws -> Keys {
    Keys(
        tls: try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: Data(contentsOf: dir.appendingPathComponent("tls.seblob"))),
        approval: try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: Data(contentsOf: dir.appendingPathComponent("approval.seblob"))))
}

// MARK: - main

let args = CommandLine.arguments
let mode = args.count > 1 ? args[1] : "auto"
info("SecureEnclave.isAvailable = \(SecureEnclave.isAvailable)")
guard SecureEnclave.isAvailable else {
    fail("本机无 Secure Enclave")
    exit(1)
}

do {
    switch mode {
    case "auto":
        probeSecurityFramework()
        checkKeys(try createKeys(), label: "CryptoKit 新建")
    case "create":
        let keys = try createKeys()
        try write(keys, to: URL(fileURLWithPath: args[2]))
        pass("已写入 SE 包装块（包装块只能在本机 SE 解开，不是私钥明文）")
        checkKeys(keys, label: "create")
    case "reload":
        checkKeys(try read(from: URL(fileURLWithPath: args[2])), label: "新进程恢复")
    case "interactive":
        let keys = try read(from: URL(fileURLWithPath: args[2]))
        let ctx = LAContext()
        ctx.localizedReason = "签名 merge acme/web#12 @ 9f2c4e7a1b3d"
        let k = try SecureEnclave.P256.Signing.PrivateKey(
            dataRepresentation: keys.approval.dataRepresentation, authenticationContext: ctx)
        do {
            let sig = try k.signature(for: message)
            keys.approval.publicKey.isValidSignature(sig, for: message)
                ? pass("用户在场后审批签名成功且验签通过（raw r‖s \(sig.rawRepresentation.count) 字节）")
                : fail("审批签名验签失败")
        } catch {
            info("审批签名未完成（取消 / 失败 / 锁屏）：\(describe(error))")
        }
    default:
        fail("未知模式 \(mode)")
    }
} catch {
    fail("异常：\(describe(error))")
}
exit(Int32(failures))
