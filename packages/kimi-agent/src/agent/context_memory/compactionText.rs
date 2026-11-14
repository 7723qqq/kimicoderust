use crate::human::llm::message::ContentPart;

pub fn extract_text(content: &[ContentPart]) -> String {
    let mut text = String::new();
    for part in content {
        if let ContentPart::Text { text: part_text, .. } = part {
            text.push_str(part_text);
        }
    }
    text
}

pub fn truncate_text_to_tokens(text: &str, max_tokens: i64) -> String {
    if max_tokens <= 0 {
        return String::new();
    }
    let mut ascii_count = 0i64;
    let mut non_ascii_count = 0i64;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        if character as u32 <= 127 {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ascii_count.div_ceil(4) + non_ascii_count > max_tokens {
            break;
        }
        end = index + character.len_utf8();
    }
    text[..end].to_string()
}

pub fn truncate_text_to_tokens_from_end(text: &str, max_tokens: i64) -> String {
    if max_tokens <= 0 {
        return String::new();
    }
    let mut ascii_count = 0i64;
    let mut non_ascii_count = 0i64;
    let mut start = text.len();
    for (index, character) in text.char_indices().rev() {
        if character as u32 <= 127 {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ascii_count.div_ceil(4) + non_ascii_count > max_tokens {
            break;
        }
        start = index;
    }
    text[start..].to_string()
}
