pub fn slice_on_code_point_boundary(text: &str, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if end >= units.len() {
        return text.to_string();
    }
    let mut boundary = end;
    if boundary > 0 && is_high_surrogate(units[boundary - 1]) {
        boundary -= 1;
    }
    String::from_utf16(&units[..boundary]).unwrap_or_default()
}

pub fn code_point_boundary_start(text: &str, start: usize) -> usize {
    let units: Vec<u16> = text.encode_utf16().collect();
    if start > 0
        && start < units.len()
        && is_low_surrogate(units[start])
        && is_high_surrogate(units[start - 1])
    {
        return start + 1;
    }
    start
}

fn is_high_surrogate(code_unit: u16) -> bool {
    (0xd800..=0xdbff).contains(&code_unit)
}

fn is_low_surrogate(code_unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&code_unit)
}
