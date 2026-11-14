use serde_json::{Map, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

pub fn diff_records(
    previous: Option<&Map<String, Value>>,
    current: Option<&Map<String, Value>>,
) -> RecordDiff {
    let empty = Map::new();
    let previous = previous.unwrap_or(&empty);
    let current = current.unwrap_or(&empty);
    let mut diff = RecordDiff::default();
    for (key, value) in current {
        if !previous.contains_key(key) {
            diff.added.push(key.clone());
        } else if previous.get(key).is_some_and(|old| !deep_equal(old, value)) {
            diff.changed.push(key.clone());
        }
    }
    for key in previous.keys() {
        if !current.contains_key(key) {
            diff.removed.push(key.clone());
        }
    }
    diff
}

pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) if a == 0.0 && b == 0.0 => a.is_sign_negative() == b.is_sign_negative(),
            (Some(a), Some(b)) => a == b || (a.is_nan() && b.is_nan()),
            _ => false,
        },
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| deep_equal(value, other)))
        }
        _ => a == b,
    }
}
