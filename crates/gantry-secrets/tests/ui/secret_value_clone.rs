use gantry_core::ports::secrets::SecretValue;

fn main() {
    let value = SecretValue::from_bytes(b"x".to_vec());
    let _copy = value.clone();
}
