//! `SecretValue` 误用编译失败（secrets.md §12.3）：不能 `Clone` / `Serialize` / `format!("{}")`。

#![cfg(unix)]

#[test]
fn secret_value_misuse_does_not_compile() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
