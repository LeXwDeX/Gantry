use gantry_core::ports::secrets::SecretValue;

fn needs_serialize<T: serde::Serialize>(_: &T) {}

fn main() {
    let value = SecretValue::from_bytes(b"x".to_vec());
    needs_serialize(&value);
}
