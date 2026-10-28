use gantry_core::ports::secrets::SecretValue;
trait DisplayValue: std::fmt::Display {}
impl<T: std::fmt::Display> DisplayValue for T {}
fn format_value<T: DisplayValue>(value: T) -> String {
    format!("{}", value)
}
fn main() {
    let _text = format_value(String::from("x"));
    let _text = format_value(SecretValue::from_bytes(b"x".to_vec()));
}
