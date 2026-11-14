pub fn parse_boolean_env(value: Option<&str>) -> Option<bool> {
    let normalized = value?.trim().to_lowercase();
    if normalized.is_empty() {
        return None;
    }
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub fn parse_number_env(value: Option<&str>) -> Option<f64> {
    let normalized = value?.trim();
    if normalized.is_empty() {
        return None;
    }
    let parsed = if let Some(value) = normalized.strip_prefix("0x").or_else(|| normalized.strip_prefix("0X")) {
        parse_prefixed_integer(value, 16)
    } else if let Some(value) = normalized.strip_prefix("0b").or_else(|| normalized.strip_prefix("0B")) {
        parse_prefixed_integer(value, 2)
    } else if let Some(value) = normalized.strip_prefix("0o").or_else(|| normalized.strip_prefix("0O")) {
        parse_prefixed_integer(value, 8)
    } else {
        normalized.parse::<f64>().ok()
    }?;
    parsed.is_finite().then_some(parsed)
}

fn parse_prefixed_integer(input: &str, radix: u32) -> Option<f64> {
    if input.is_empty() {
        return None;
    }
    let bits_per_digit = match radix {
        2 => 1,
        8 => 3,
        16 => 4,
        _ => return None,
    };
    let digits = input
        .chars()
        .map(|character| character.to_digit(radix))
        .collect::<Option<Vec<_>>>()?;
    let mut bits = Vec::with_capacity(digits.len() * bits_per_digit);
    for digit in digits {
        for shift in (0..bits_per_digit).rev() {
            bits.push(((digit >> shift) & 1) as u8);
        }
    }
    let first_one = bits.iter().position(|bit| *bit != 0);
    let Some(first_one) = first_one else {
        return Some(0.0);
    };
    let bits = &bits[first_one..];
    if bits.len() <= 53 {
        let value = bits
            .iter()
            .fold(0_u64, |value, bit| (value << 1) | u64::from(*bit));
        return Some(value as f64);
    }
    let mut significand = bits[..53]
        .iter()
        .fold(0_u64, |value, bit| (value << 1) | u64::from(*bit));
    let guard = bits[53] != 0;
    let sticky = bits[54..].contains(&1);
    if guard && (sticky || significand & 1 == 1) {
        significand += 1;
    }
    let mut shift = bits.len() - 53;
    if significand == 1_u64 << 53 {
        significand >>= 1;
        shift += 1;
    }
    if shift + 52 > 1023 {
        return None;
    }
    let value = (significand as f64) * 2_f64.powi(shift as i32);
    value.is_finite().then_some(value)
}
