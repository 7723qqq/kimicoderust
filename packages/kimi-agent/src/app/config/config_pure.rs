use serde_json::{Map, Value};

pub fn is_plain_object(value: &Value) -> bool {
    value.is_object()
}

pub fn deep_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| deep_equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| deep_equal(value, other)))
        }
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => a == b,
    }
}

pub fn deep_merge(base: Option<&Value>, patch: Option<&Value>) -> Option<Value> {
    let patch = patch.filter(|value| !value.is_null());
    let (Some(Value::Object(base)), Some(Value::Object(patch))) = (base, patch) else {
        return patch.or(base).cloned();
    };
    let mut out = base.clone();
    for (key, patch_value) in patch {
        let merged = match (out.get(key), patch_value) {
            (Some(Value::Object(_)), Value::Object(_)) => {
                deep_merge(out.get(key), Some(patch_value)).unwrap_or(Value::Null)
            }
            _ => patch_value.clone(),
        };
        out.insert(key.clone(), merged);
    }
    Some(Value::Object(out))
}

pub fn omit_undefined(value: &Map<String, Option<Value>>) -> Map<String, Value> {
    value
        .iter()
        .filter_map(|(key, value)| value.as_ref().map(|value| (key.clone(), value.clone())))
        .collect()
}

pub fn describe_unknown_error(error: &dyn std::error::Error) -> String {
    error.to_string()
}
