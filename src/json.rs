use sonic_rs::{from_str, to_string, Value};

pub fn compact_json(input: &str) -> Option<String> {
    let value: Value = from_str(input).ok()?;
    to_string(&value).ok()
}
