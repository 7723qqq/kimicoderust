pub use crate::injection::{SYSTEM_REMINDER_PREFIX, SYSTEM_REMINDER_SUFFIX};

pub fn wrap_system_reminder(content: &str) -> String {
    format!(
        "{SYSTEM_REMINDER_PREFIX}{}{SYSTEM_REMINDER_SUFFIX}",
        trim_js(content)
    )
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub fn system_reminder_content(text: &str) -> Option<&str> {
    text.strip_prefix(SYSTEM_REMINDER_PREFIX)
        .and_then(|text| text.strip_suffix(SYSTEM_REMINDER_SUFFIX))
}
