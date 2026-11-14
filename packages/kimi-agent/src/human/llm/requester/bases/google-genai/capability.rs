use crate::human::llm::capability::ModelCapability;

const GEMINI_CATALOGUED_PREFIXES: [&str; 6] = [
    "gemini-1.5-pro",
    "gemini-1.5-flash",
    "gemini-2.0-flash",
    "gemini-2.0-pro",
    "gemini-2.5-pro",
    "gemini-2.5-flash",
];

pub fn get_google_gen_ai_model_capability(model_name: &str) -> Option<ModelCapability> {
    let normalized = model_name.to_lowercase();
    if !normalized.starts_with("gemini-") {
        return None;
    }
    if !GEMINI_CATALOGUED_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return None;
    }
    Some(ModelCapability {
        image_in: true,
        video_in: true,
        audio_in: true,
        thinking: normalized.starts_with("gemini-2.5-") || normalized.contains("thinking"),
        tool_use: true,
        pdf_in: None,
        dynamically_loaded_tools: None,
        unknown_capability: false,
    })
}
