use regex::Regex;
use serde_json::Number;
use std::sync::LazyLock;

pub(super) fn rejects_string(name: &str, text: &str) -> bool {
    match name {
        "date" => !is_date(text),
        "time" => !is_time(text, true),
        "date-time" => !is_date_time(text, true),
        "iso-time" => !is_time(text, false),
        "iso-date-time" => !is_date_time(text, false),
        "duration" => !is_duration(text),
        "uri" => !is_uri(text),
        "uri-reference" => !URI_REFERENCE_RE.is_match(text),
        "uri-template" => !URI_TEMPLATE_RE.is_match(text),
        "url" => !is_url(text),
        "email" => !EMAIL_RE.is_match(text),
        "hostname" => !is_hostname(text),
        "ipv4" => !IPV4_RE.is_match(text),
        "ipv6" => !IPV6_RE.is_match(text),
        "regex" => !is_regex(text),
        "uuid" => !UUID_RE.is_match(text),
        "json-pointer" => !JSON_POINTER_RE.is_match(text),
        "json-pointer-uri-fragment" => !JSON_POINTER_FRAGMENT_RE.is_match(text),
        "relative-json-pointer" => !RELATIVE_JSON_POINTER_RE.is_match(text),
        "byte" => !is_byte(text),

        "password" | "binary" => false,
        _ => false,
    }
}

pub(super) fn rejects_number(name: &str, number: &Number) -> bool {
    match name {
        "int32" => !is_int32(number),
        "int64" => !is_integer(number),
        "float" | "double" => false,
        _ => false,
    }
}

fn is_int32(number: &Number) -> bool {
    number.as_f64().is_some_and(|value| {
        value.fract() == 0.0 && (-2147483648.0..=2147483647.0).contains(&value)
    })
}

fn is_integer(number: &Number) -> bool {
    number.as_f64().is_some_and(|value| value.fract() == 0.0)
}

fn is_date(text: &str) -> bool {
    let Some(captures) = DATE_RE.captures(text) else {
        return false;
    };
    let year: u32 = captures[1].parse().unwrap_or(0);
    let month: u32 = captures[2].parse().unwrap_or(0);
    let day: u32 = captures[3].parse().unwrap_or(0);
    let last_day = if month == 2 && is_leap_year(year) {
        29
    } else {
        DAYS_IN_MONTH.get(month as usize).copied().unwrap_or(0)
    };
    (1..=last_day).contains(&day) && (1..=12).contains(&month)
}

fn is_leap_year(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

const DAYS_IN_MONTH: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

fn is_time(text: &str, strict_time_zone: bool) -> bool {
    let Some(captures) = TIME_RE.captures(text) else {
        return false;
    };
    let hour: i64 = captures[1].parse().unwrap_or(0);
    let minute: i64 = captures[2].parse().unwrap_or(0);
    let second: f64 = captures[3].parse().unwrap_or(0.0);
    let has_zone = captures.get(4).is_some();
    let zone_sign = if captures.get(5).map(|zone| zone.as_str()) == Some("-") {
        -1
    } else {
        1
    };
    let zone_hour: i64 = captures
        .get(6)
        .and_then(|zone| zone.as_str().parse().ok())
        .unwrap_or(0);
    let zone_minute: i64 = captures
        .get(7)
        .and_then(|zone| zone.as_str().parse().ok())
        .unwrap_or(0);
    if zone_hour > 23 || zone_minute > 59 || (strict_time_zone && !has_zone) {
        return false;
    }
    if hour <= 23 && minute <= 59 && second < 60.0 {
        return true;
    }
    let utc_minute = minute - zone_minute * zone_sign;
    let utc_hour = hour - zone_hour * zone_sign - if utc_minute < 0 { 1 } else { 0 };
    (utc_hour == 23 || utc_hour == -1) && (utc_minute == 59 || utc_minute == -1) && second < 61.0
}

fn is_date_time(text: &str, strict_time_zone: bool) -> bool {
    let Some((date, time)) = split_date_time(text) else {
        return false;
    };
    is_date(date) && is_time(time, strict_time_zone)
}

fn split_date_time(text: &str) -> Option<(&str, &str)> {
    let mut separator = None;
    for (index, character) in text.char_indices() {
        if character == 't' || character == 'T' || is_js_space(character) {
            if separator.is_some() {
                return None;
            }
            separator = Some((index, character.len_utf8()));
        }
    }
    let (index, width) = separator?;
    Some((&text[..index], &text[index + width..]))
}

fn is_duration(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('P') else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    duration_date_time_part(rest) || duration_week_part(rest)
}

fn duration_date_time_part(text: &str) -> bool {
    let mut rest = text;
    if let Some(tail) = take_digits_and_unit(rest, b'Y') {
        rest = tail;
    }
    if let Some(tail) = take_digits_and_unit(rest, b'M') {
        rest = tail;
    }
    if let Some(tail) = take_digits_and_unit(rest, b'D') {
        rest = tail;
    }
    if let Some(after_t) = rest.strip_prefix('T')
        && after_t.as_bytes().first().is_some_and(u8::is_ascii_digit)
    {
        let mut tail = after_t;
        if let Some(next) = take_digits_and_unit(tail, b'H') {
            tail = next;
        }
        if let Some(next) = take_digits_and_unit(tail, b'M') {
            tail = next;
        }
        if let Some(next) = take_digits_and_unit(tail, b'S') {
            tail = next;
        }
        rest = tail;
    }
    rest.is_empty()
}

fn duration_week_part(text: &str) -> bool {
    let Some(tail) = take_digits_and_unit(text, b'W') else {
        return false;
    };
    tail.is_empty()
}

fn take_digits_and_unit<'a>(text: &'a str, unit: u8) -> Option<&'a str> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == 0 || index >= bytes.len() || bytes[index] != unit {
        return None;
    }
    Some(&text[index + 1..])
}

fn is_uri(text: &str) -> bool {
    (text.contains('/') || text.contains(':')) && URI_RE.is_match(text)
}

fn is_url(text: &str) -> bool {
    let Some(rest) = strip_prefix_ascii_case(text, "https://")
        .or_else(|| strip_prefix_ascii_case(text, "http://"))
        .or_else(|| strip_prefix_ascii_case(text, "ftp://"))
    else {
        return false;
    };

    if rest.chars().any(is_js_space) {
        return false;
    }

    let mut candidates = vec![0usize];
    for (index, byte) in rest.bytes().enumerate() {
        if byte == b'@' {
            candidates.push(index + 1);
        }
    }
    candidates
        .into_iter()
        .any(|start| url_authority_ok(&rest[start..]))
}

fn url_authority_ok(text: &str) -> bool {
    let authority = text.split('/').next().unwrap_or("");
    let host = match authority.rfind(':') {
        Some(index) => {
            let port = &authority[index + 1..];
            if !(2..=5).contains(&port.len()) || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return false;
            }
            &authority[..index]
        }
        None => authority,
    };
    url_ipv4_ok(host) || URL_HOST_RE.is_match(host)
}

fn url_ipv4_ok(host: &str) -> bool {
    if !URL_IPV4_RE.is_match(host) {
        return false;
    }
    let octets: Vec<u32> = host
        .split('.')
        .filter_map(|octet| octet.parse().ok())
        .collect();
    let [first, second, ..] = octets[..] else {
        return false;
    };
    if first == 10 || first == 127 {
        return false;
    }
    if first == 169 && second == 254 {
        return false;
    }
    if first == 192 && second == 168 {
        return false;
    }
    if first == 172 && (16..=31).contains(&second) {
        return false;
    }
    true
}

fn is_hostname(text: &str) -> bool {
    let length = text.chars().count();
    let length_ok = if text.ends_with('.') {
        (2..=254).contains(&length)
    } else {
        (1..=253).contains(&length)
    };
    length_ok && HOSTNAME_RE.is_match(text)
}

fn is_regex(text: &str) -> bool {
    !Z_ANCHOR_RE.is_match(text) && super::regex_js::validate_with_flags(text, "").is_ok()
}

fn is_byte(text: &str) -> bool {
    let mut start = 0usize;
    for (index, character) in text.char_indices() {
        if is_line_terminator(character) {
            if BYTE_RE.is_match(&text[start..index]) {
                return true;
            }
            start = index + character.len_utf8();
        }
    }
    BYTE_RE.is_match(&text[start..])
}

fn is_js_space(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{000b}' | '\u{000c}' | '\r' | ' ' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn strip_prefix_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("ajv-formats pattern is a compile-time constant")
}

const DATE_PATTERN: &str = r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$";
const TIME_PATTERN: &str =
    r"(?i)^([0-9]{2}):([0-9]{2}):([0-9]{2}(?:\.[0-9]+)?)(z|([+-])([0-9]{2})(?::?([0-9]{2}))?)?$";
const URI_REFERENCE_PATTERN: &str = r##"(?i)^(?:[a-z][a-z0-9+\-.]*:)?(?:\/?\/(?:(?:[a-z0-9\-._~!$&'()*+,;=:]|%[0-9a-f]{2})*@)?(?:\[(?:(?:(?:(?:[0-9a-f]{1,4}:){6}|::(?:[0-9a-f]{1,4}:){5}|(?:[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){4}|(?:(?:[0-9a-f]{1,4}:){0,1}[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){3}|(?:(?:[0-9a-f]{1,4}:){0,2}[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){2}|(?:(?:[0-9a-f]{1,4}:){0,3}[0-9a-f]{1,4})?::[0-9a-f]{1,4}:|(?:(?:[0-9a-f]{1,4}:){0,4}[0-9a-f]{1,4})?::)(?:[0-9a-f]{1,4}:[0-9a-f]{1,4}|(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?))|(?:(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4})?::[0-9a-f]{1,4}|(?:(?:[0-9a-f]{1,4}:){0,6}[0-9a-f]{1,4})?::)|[Vv][0-9a-f]+\.[a-z0-9\-._~!$&'()*+,;=:]+)\]|(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)|(?:[a-z0-9\-._~!$&'"()*+,;=]|%[0-9a-f]{2})*)(?::[0-9]*)?(?:\/(?:[a-z0-9\-._~!$&'"()*+,;=:@]|%[0-9a-f]{2})*)*|\/(?:(?:[a-z0-9\-._~!$&'"()*+,;=:@]|%[0-9a-f]{2})+(?:\/(?:[a-z0-9\-._~!$&'"()*+,;=:@]|%[0-9a-f]{2})*)*)?|(?:[a-z0-9\-._~!$&'"()*+,;=:@]|%[0-9a-f]{2})+(?:\/(?:[a-z0-9\-._~!$&'"()*+,;=:@]|%[0-9a-f]{2})*)*)?(?:\?(?:[a-z0-9\-._~!$&'"()*+,;=:@/?]|%[0-9a-f]{2})*)?(?:#(?:[a-z0-9\-._~!$&'"()*+,;=:@/?]|%[0-9a-f]{2})*)?$"##;
const URI_TEMPLATE_PATTERN: &str = r##"(?i)^(?:(?:[^\x00-\x20"'<>%\\^`{|}]|%[0-9a-f]{2})|\{[+#./;?&=,!@|]?(?:[a-z0-9_]|%[0-9a-f]{2})+(?::[1-9][0-9]{0,3}|\*)?(?:,(?:[a-z0-9_]|%[0-9a-f]{2})+(?::[1-9][0-9]{0,3}|\*)?)*\})*$"##;
const EMAIL_PATTERN: &str = r##"(?i)^[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$"##;
const HOSTNAME_PATTERN: &str =
    r##"(?i)^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[-0-9a-z]{0,61}[0-9a-z])?)*\.?$"##;
const IPV4_PATTERN: &str = r"^(?:(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])\.){3}(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])$";
const IPV6_PATTERN: &str = r##"(?i)^((([0-9a-f]{1,4}:){7}([0-9a-f]{1,4}|:))|(([0-9a-f]{1,4}:){6}(:[0-9a-f]{1,4}|((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3})|:))|(([0-9a-f]{1,4}:){5}(((:[0-9a-f]{1,4}){1,2})|:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3})|:))|(([0-9a-f]{1,4}:){4}(((:[0-9a-f]{1,4}){1,3})|((:[0-9a-f]{1,4})?:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}))|:))|(([0-9a-f]{1,4}:){3}(((:[0-9a-f]{1,4}){1,4})|((:[0-9a-f]{1,4}){0,2}:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}))|:))|(([0-9a-f]{1,4}:){2}(((:[0-9a-f]{1,4}){1,5})|((:[0-9a-f]{1,4}){0,3}:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}))|:))|(([0-9a-f]{1,4}:){1}(((:[0-9a-f]{1,4}){1,6})|((:[0-9a-f]{1,4}){0,4}:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}))|:))|(:(((:[0-9a-f]{1,4}){1,7})|((:[0-9a-f]{1,4}){0,5}:((25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])(\.(25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9]?[0-9])){3}))|:)))$"##;
const UUID_PATTERN: &str = r"(?i)^(?:urn:uuid:)?[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$";
const JSON_POINTER_PATTERN: &str = r"^(?:\/(?:[^~/]|~0|~1)*)*$";
const JSON_POINTER_FRAGMENT_PATTERN: &str =
    r"(?i)^#(?:\/(?:[a-z0-9_\-.!$&'()*+,;:=@]|%[0-9a-f]{2}|~0|~1)*)*$";
const RELATIVE_JSON_POINTER_PATTERN: &str = r"^(?:0|[1-9][0-9]*)(?:#|(?:\/(?:[^~/]|~0|~1)*)*)$";
const BYTE_PATTERN: &str = r"^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$";
const URL_IPV4_PATTERN: &str = r"^(?:[1-9][0-9]?|1[0-9][0-9]|2[01][0-9]|22[0-3])(?:\.(?:1?[0-9]{1,2}|2[0-4][0-9]|25[0-5])){2}(?:\.(?:[1-9][0-9]?|1[0-9][0-9]|2[0-4][0-9]|25[0-4]))$";
const URL_HOST_PATTERN: &str = r##"(?i)^(?:(?:[a-z0-9\u{00a1}-\u{ffff}]+-)*[a-z0-9\u{00a1}-\u{ffff}]+)(?:\.(?:[a-z0-9\u{00a1}-\u{ffff}]+-)*[a-z0-9\u{00a1}-\u{ffff}]+)*(?:\.(?:[a-z\u{00a1}-\u{ffff}]{2,}))$"##;
const URI_PATTERN: &str = r##"(?i)^(?:[a-z][a-z0-9+\-.]*:)(?:\/?\/(?:(?:[a-z0-9\-._~!$&'()*+,;=:]|%[0-9a-f]{2})*@)?(?:\[(?:(?:(?:(?:[0-9a-f]{1,4}:){6}|::(?:[0-9a-f]{1,4}:){5}|(?:[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){4}|(?:(?:[0-9a-f]{1,4}:){0,1}[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){3}|(?:(?:[0-9a-f]{1,4}:){0,2}[0-9a-f]{1,4})?::(?:[0-9a-f]{1,4}:){2}|(?:(?:[0-9a-f]{1,4}:){0,3}[0-9a-f]{1,4})?::[0-9a-f]{1,4}:|(?:(?:[0-9a-f]{1,4}:){0,4}[0-9a-f]{1,4})?::)(?:[0-9a-f]{1,4}:[0-9a-f]{1,4}|(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?))|(?:(?:[0-9a-f]{1,4}:){0,5}[0-9a-f]{1,4})?::[0-9a-f]{1,4}|(?:(?:[0-9a-f]{1,4}:){0,6}[0-9a-f]{1,4})?::)|[Vv][0-9a-f]+\.[a-z0-9\-._~!$&'()*+,;=:]+)\]|(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)|(?:[a-z0-9\-._~!$&'()*+,;=]|%[0-9a-f]{2})*)(?::[0-9]*)?(?:\/(?:[a-z0-9\-._~!$&'()*+,;=:@]|%[0-9a-f]{2})*)*|\/(?:(?:[a-z0-9\-._~!$&'()*+,;=:@]|%[0-9a-f]{2})+(?:\/(?:[a-z0-9\-._~!$&'()*+,;=:@]|%[0-9a-f]{2})*)*)?|(?:[a-z0-9\-._~!$&'()*+,;=:@]|%[0-9a-f]{2})+(?:\/(?:[a-z0-9\-._~!$&'()*+,;=:@]|%[0-9a-f]{2})*)*)(?:\?(?:[a-z0-9\-._~!$&'()*+,;=:@/?]|%[0-9a-f]{2})*)?(?:#(?:[a-z0-9\-._~!$&'()*+,;=:@/?]|%[0-9a-f]{2})*)?$"##;

static DATE_RE: LazyLock<Regex> = LazyLock::new(|| compile(DATE_PATTERN));
static TIME_RE: LazyLock<Regex> = LazyLock::new(|| compile(TIME_PATTERN));
static URI_RE: LazyLock<Regex> = LazyLock::new(|| compile(URI_PATTERN));
static URI_REFERENCE_RE: LazyLock<Regex> = LazyLock::new(|| compile(URI_REFERENCE_PATTERN));
static URI_TEMPLATE_RE: LazyLock<Regex> = LazyLock::new(|| compile(URI_TEMPLATE_PATTERN));
static EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| compile(EMAIL_PATTERN));
static HOSTNAME_RE: LazyLock<Regex> = LazyLock::new(|| compile(HOSTNAME_PATTERN));
static IPV4_RE: LazyLock<Regex> = LazyLock::new(|| compile(IPV4_PATTERN));
static IPV6_RE: LazyLock<Regex> = LazyLock::new(|| compile(IPV6_PATTERN));
static UUID_RE: LazyLock<Regex> = LazyLock::new(|| compile(UUID_PATTERN));
static JSON_POINTER_RE: LazyLock<Regex> = LazyLock::new(|| compile(JSON_POINTER_PATTERN));
static JSON_POINTER_FRAGMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(JSON_POINTER_FRAGMENT_PATTERN));
static RELATIVE_JSON_POINTER_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(RELATIVE_JSON_POINTER_PATTERN));
static BYTE_RE: LazyLock<Regex> = LazyLock::new(|| compile(BYTE_PATTERN));
static URL_IPV4_RE: LazyLock<Regex> = LazyLock::new(|| compile(URL_IPV4_PATTERN));
static URL_HOST_RE: LazyLock<Regex> = LazyLock::new(|| compile(URL_HOST_PATTERN));
static Z_ANCHOR_RE: LazyLock<Regex> = LazyLock::new(|| compile(r"[^\\]\\Z"));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accepts(name: &str, text: &str) -> bool {
        !rejects_string(name, text)
    }

    fn accepts_number(name: &str, value: serde_json::Value) -> bool {
        let number = value.as_number().cloned().expect("must be a number");
        !rejects_number(name, &number)
    }

    #[test]
    fn email_matches_ajv() {
        assert!(accepts("email", "A@B.CO"));
        assert!(!accepts("email", "a@b"));
        assert!(!accepts("email", "not-an-email"));
        assert!(accepts("email", "first.last+tag@example.co.uk"));
    }

    #[test]
    fn date_respects_leap_years() {
        assert!(accepts("date", "2020-02-29"));
        assert!(!accepts("date", "2021-02-30"));
        assert!(!accepts("date", "2021-13-01"));
        assert!(!accepts("date", ""));
        assert!(!accepts("date", "2021-1-1"));
    }

    #[test]
    fn only_the_strict_time_formats_require_a_zone() {
        assert!(!accepts("time", "12:00:00"));
        assert!(accepts("time", "12:00:00Z"));
        assert!(accepts("time", "23:59:60Z"));
        assert!(!accepts("time", "24:00:00Z"));
        assert!(accepts("iso-time", "12:00:00"));
        assert!(!accepts("time", "12:00:00+24:00"));
    }

    #[test]
    fn date_time_accepts_a_space_or_t_separator() {
        assert!(accepts("date-time", "2020-01-01T00:00:00Z"));
        assert!(accepts("date-time", "2020-01-01 00:00:00Z"));
        assert!(!accepts("date-time", "2020-01-01T00:00:00"));
        assert!(!accepts("date-time", "2020-01-01T00:00:00Z extra"));
        assert!(accepts("iso-date-time", "2020-01-01T00:00:00"));
    }

    #[test]
    fn duration_follows_the_lookahead_free_grammar() {
        for valid in ["P1Y", "P1M", "P1D", "PT5H", "P1Y2M3DT4H5M6S", "P1W", "P0Y"] {
            assert!(accepts("duration", valid), "{valid} must be accepted");
        }
        for invalid in ["P", "PT", "P1Y1W", "P1YT", "PT5", "1Y", "P1H"] {
            assert!(!accepts("duration", invalid), "{invalid} must be rejected");
        }
    }

    #[test]
    fn uri_needs_a_scheme_like_fragment() {
        assert!(accepts("uri", "http://example.com/a"));
        assert!(accepts("uri", "a:b"));
        assert!(!accepts("uri", "not a uri"));
    }

    #[test]
    fn url_port_private_ranges_and_schemes_match_ajv() {
        for valid in [
            "https://EXAMPLE.com",
            "ftp://example.com",
            "http://example.com:8080/x",
            "http://user:pass@example.com",
            "http://10.0.0.1.example.com",
            "http://172.32.0.1",
            "http://223.255.255.254",
            "http://a.co",
        ] {
            assert!(accepts("url", valid), "{valid} must be accepted");
        }
        for invalid in [
            "http://localhost:3000/x",
            "http://10.0.0.1",
            "http://127.0.0.1",
            "http://169.254.1.1",
            "http://192.168.1.1",
            "http://172.16.0.1",
            "http://224.0.0.1",
            "http://example.com.",
            "http://example.com:9",
            "http://example.com:80:80",
            "http://example.com?x=1",
            "http://example",
            "http://-a.com",
            "http://a-.com",
            "HTTP://EXAMPLE.COM/A B",
        ] {
            assert!(!accepts("url", invalid), "{invalid} must be rejected");
        }
    }

    #[test]
    fn hostname_bounds_the_length_like_the_lookahead() {
        assert!(accepts("hostname", "example.com"));
        assert!(accepts("hostname", "example.com."));
        assert!(!accepts("hostname", "-bad.com"));

        assert!(!accepts("hostname", &"a".repeat(64)));
        let longest = vec!["a"; 127].join(".");
        assert_eq!(longest.len(), 253);
        assert!(accepts("hostname", &longest));
        let too_long = vec!["a"; 128].join(".");
        assert_eq!(too_long.len(), 255);
        assert!(!accepts("hostname", &too_long));
        assert!(accepts(
            "hostname",
            &format!("{}.", vec!["a"; 126].join("."))
        ));
    }

    #[test]
    fn ip_addresses_are_checked() {
        assert!(accepts("ipv4", "1.2.3.4"));
        assert!(!accepts("ipv4", "256.1.1.1"));
        assert!(accepts("ipv6", "::1"));
        assert!(!accepts("ipv6", "1.2.3.4"));
    }

    #[test]
    fn uuid_json_pointers_and_base64_follow_the_table() {
        assert!(accepts(
            "uuid",
            "urn:uuid:123e4567-e89b-12d3-a456-426614174000"
        ));
        assert!(!accepts("uuid", "123e4567"));
        assert!(accepts("json-pointer", ""));
        assert!(!accepts("json-pointer", "a"));
        assert!(accepts("json-pointer-uri-fragment", "#/a"));
        assert!(accepts("relative-json-pointer", "0#"));
        assert!(accepts("byte", "AAAA\nBBBB"));
        assert!(!accepts("byte", "AAA"));
        assert!(accepts("byte", ""));
    }

    #[test]
    fn the_regex_format_compiles_and_rejects_z_anchors() {
        assert!(accepts("regex", "^a+$"));
        assert!(accepts("regex", "(?<=a)b"));
        assert!(!accepts("regex", "["));
        assert!(!accepts("regex", "(?i)abc"));
        assert!(!accepts("regex", r"a\Z"));
    }

    #[test]
    fn number_formats_follow_the_integer_bounds() {
        assert!(accepts_number("int32", json!(2147483647)));
        assert!(accepts_number("int32", json!(-2147483648)));
        assert!(!accepts_number("int32", json!(2147483648u32)));
        assert!(!accepts_number("int32", json!(1.5)));
        assert!(accepts_number("int64", json!(1e300)));
        assert!(!accepts_number("int64", json!(1.5)));
        assert!(accepts_number("float", json!(1.5)));
        assert!(accepts_number("double", json!(1.5)));
    }

    #[test]
    fn unchecked_and_unknown_formats_never_reject() {
        assert!(accepts("password", "anything"));
        assert!(accepts("binary", "anything"));
        assert!(accepts("nonsense-xyz", "anything"));
        assert!(accepts_number("nonsense-xyz", json!(1.5)));

        assert!(accepts("int32", "not a number"));
        assert!(accepts_number("email", json!(123)));
    }
}
