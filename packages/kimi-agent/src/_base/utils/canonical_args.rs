pub fn canonical_telemetry_args(args: &serde_json::Value) -> String {
    stringify_json_value(args)
}

fn stringify_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(stringify_json_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        serde_json::Value::Object(values) => {
            let mut indexes = Vec::new();
            let mut names = Vec::new();
            for key in values.keys() {
                if let Some(index) = js_array_index(key) {
                    indexes.push((index, key));
                } else {
                    names.push(key);
                }
            }
            indexes.sort_by_key(|(index, _)| *index);
            names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
            let mut keys = indexes
                .into_iter()
                .map(|(_, key)| key)
                .collect::<Vec<_>>();
            keys.extend(names);
            let entries = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap(),
                        stringify_json_value(&values[key])
                    )
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Number(value) => js_number_to_string(value),
        _ => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn js_array_index(key: &str) -> Option<u32> {
    if key.is_empty()
        || (key.len() > 1 && key.starts_with('0'))
        || !key.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX).then_some(index)
}

fn js_number_to_string(value: &serde_json::Number) -> String {
    let Some(value) = value.as_f64() else {
        return value.to_string();
    };
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value.is_sign_negative();
    let Some(value) = serde_json::Number::from_f64(value.abs()) else {
        return "null".to_string();
    };
    let raw = value.to_string();
    let exponent_at = raw.find('e').or_else(|| raw.find('E'));
    let (mantissa, exponent) = match exponent_at {
        Some(index) => (
            &raw[..index],
            raw[index + 1..].parse::<i32>().unwrap_or(0),
        ),
        None => (raw.as_str(), 0),
    };
    let mut decimal_position = mantissa.find('.').unwrap_or(mantissa.len()) as i32 + exponent;
    let mut digits = mantissa.replace('.', "");
    let leading_zeros = digits.bytes().take_while(|byte| *byte == b'0').count();
    if leading_zeros > 0 {
        digits.drain(..leading_zeros);
        decimal_position -= leading_zeros as i32;
    }
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let length = digits.len() as i32;
    let mut output = String::new();
    if negative {
        output.push('-');
    }
    if length <= decimal_position && decimal_position <= 21 {
        output.push_str(&digits);
        output.extend(std::iter::repeat_n('0', (decimal_position - length) as usize));
    } else if decimal_position > 0 && decimal_position <= 21 {
        let split = decimal_position as usize;
        output.push_str(&digits[..split]);
        output.push('.');
        output.push_str(&digits[split..]);
    } else if decimal_position <= 0 && decimal_position > -6 {
        output.push_str("0.");
        output.extend(std::iter::repeat_n('0', (-decimal_position) as usize));
        output.push_str(&digits);
    } else {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        let exponent = decimal_position - 1;
        if exponent >= 0 {
            output.push_str(&format!("e+{exponent}"));
        } else {
            output.push_str(&format!("e{exponent}"));
        }
    }
    output
}

pub fn is_plain_record(value: &serde_json::Value) -> bool {
    value.is_object()
}
