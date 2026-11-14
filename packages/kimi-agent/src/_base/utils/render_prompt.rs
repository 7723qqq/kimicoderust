use std::collections::HashMap;

use once_cell::sync::Lazy;
use regex::Regex;

static PROMPT_VARIABLE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap());

pub fn render_prompt(template: &str, vars: &HashMap<String, serde_json::Value>) -> String {
    PROMPT_VARIABLE
        .replace_all(template, |captures: &regex::Captures<'_>| {
            let name = captures.get(1).unwrap().as_str();
            match vars.get(name) {
                Some(serde_json::Value::String(value)) => value.clone(),
                Some(serde_json::Value::Number(value)) => js_number_to_string(value),
                _ => captures.get(0).unwrap().as_str().to_string(),
            }
        })
        .into_owned()
}

fn js_number_to_string(value: &serde_json::Number) -> String {
    let Some(value) = value.as_f64() else {
        return value.to_string();
    };
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value.is_sign_negative();
    let absolute = value.abs();
    if !absolute.is_finite() {
        if absolute.is_nan() {
            return "NaN".to_string();
        }
        return if negative {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    let value = serde_json::Number::from_f64(absolute).unwrap();
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
