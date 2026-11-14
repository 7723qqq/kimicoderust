use crate::human::llm::message::ContentPart;

pub fn is_vacuous_content_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, meta } => {
            let has_thought_signature = meta
                .as_ref()
                .and_then(|meta| meta.get("thought_signature_b64"))
                .is_some();
            !has_thought_signature && text.trim().is_empty()
        }
        ContentPart::Think {
            think, encrypted, ..
        } => encrypted.is_none() && think.trim().is_empty(),
        _ => false,
    }
}
