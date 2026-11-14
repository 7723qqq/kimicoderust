use std::any::Any;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

pub fn is_empty_diff(diff: &RecordDiff) -> bool {
    diff.added.is_empty() && diff.removed.is_empty() && diff.changed.is_empty()
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|value| *value < u32::MAX as u64)
        .map(|value| value as u32)
}

fn js_keys<T>(map: &IndexMap<String, T>) -> Vec<&String> {
    let mut indices = map
        .keys()
        .filter_map(|key| array_index(key).map(|index| (index, key)))
        .collect::<Vec<_>>();
    indices.sort_by_key(|(index, _)| *index);
    indices
        .into_iter()
        .map(|(_, key)| key)
        .chain(map.keys().filter(|key| array_index(key).is_none()))
        .collect()
}

pub fn diff_records<T: PartialEq + Serialize + Any>(
    previous: &IndexMap<String, T>,
    current: &IndexMap<String, T>,
) -> RecordDiff {
    let mut diff = RecordDiff::default();
    for key in js_keys(current) {
        match previous.get(key) {
            None => diff.added.push(key.clone()),
            Some(previous) if !deep_equal(previous, &current[key]) => {
                diff.changed.push(key.clone());
            }
            _ => {}
        }
    }
    for key in js_keys(previous) {
        if !current.contains_key(key) {
            diff.removed.push(key.clone());
        }
    }
    diff
}

fn object_is_f64(left: f64, right: f64) -> bool {
    if left.is_nan() || right.is_nan() {
        return left.is_nan() && right.is_nan();
    }
    if left == 0.0 && right == 0.0 {
        return left.is_sign_negative() == right.is_sign_negative();
    }
    left == right
}

fn object_is_number(left: &Number, right: &Number) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => object_is_f64(left, right),
        _ => left == right,
    }
}

fn deep_equal_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => object_is_number(left, right),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| deep_equal_value(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| deep_equal_value(left, right))
                })
        }
        _ => left == right,
    }
}

pub fn deep_equal<T: PartialEq + Serialize + Any>(left: &T, right: &T) -> bool {
    let left_any = left as &dyn Any;
    let right_any = right as &dyn Any;
    if let (Some(left), Some(right)) = (
        left_any.downcast_ref::<f64>(),
        right_any.downcast_ref::<f64>(),
    ) {
        return object_is_f64(*left, *right);
    }
    if let (Some(left), Some(right)) = (
        left_any.downcast_ref::<f32>(),
        right_any.downcast_ref::<f32>(),
    ) {
        return object_is_f64(f64::from(*left), f64::from(*right));
    }
    if let (Some(left), Some(right)) = (
        left_any.downcast_ref::<Value>(),
        right_any.downcast_ref::<Value>(),
    ) {
        return deep_equal_value(left, right);
    }
    if let (Ok(left), Ok(right)) = (serde_json::to_value(left), serde_json::to_value(right)) {
        return deep_equal_value(&left, &right);
    }
    left == right
}
