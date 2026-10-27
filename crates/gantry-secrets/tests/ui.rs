//! `SecretValue` 误用编译失败（secrets.md §12.3）：不能 `Clone` / `format!("{}")`。
//!
//! "不能 `Serialize`" 不放在 trybuild 里：rustc 显示 `Serialize` 还是 `serde::Serialize`
//! 取决于编译时的依赖图（单包测试与全工作区测试不同），快照不稳定。改由下面的编译期
//! 探针判定，结果确定。

#![cfg(unix)]

#[test]
fn secret_value_misuse_does_not_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}

/// 编译期探针：`Probe<T>` 的固有方法只在 `T: Serialize` 时存在，否则回落到 trait 默认方法。
/// 方法解析在编译期完成，所以断言的是类型系统事实，而不是运行期行为。
struct Probe<T>(std::marker::PhantomData<T>);

trait NotSerialize {
    fn is_serialize(&self) -> bool {
        false
    }
}

impl<T> NotSerialize for Probe<T> {}

impl<T: serde::Serialize> Probe<T> {
    #[allow(dead_code)]
    fn is_serialize(&self) -> bool {
        true
    }
}

#[test]
fn secret_value_is_not_serialize() {
    use gantry_core::ports::secrets::SecretValue;
    assert!(!Probe::<SecretValue>(std::marker::PhantomData).is_serialize());
    // 对照：普通可序列化类型被探针识别为 Serialize。
    assert!(Probe::<String>(std::marker::PhantomData).is_serialize());
}
