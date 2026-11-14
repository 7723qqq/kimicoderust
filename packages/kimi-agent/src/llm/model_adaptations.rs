use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::llm::model_family::resolve_model_family;

const MAX_ADAPTATION_BYTES: usize = 32 * 1024;
const DEEPSEEK_ADAPTATION: &str = include_str!("./model-adaptations/curated/deepseek.md");

pub const DEEPSEEK_ADAPTATION_SOURCE_ID: &str =
    "packages/agent-core-v2/src/app/agentProfileCatalog/model-adaptations/curated/deepseek.md";

#[derive(Debug, Clone, Default)]
pub struct LoadAdaptationInput {
    pub model: String,
    pub candidates: Option<Vec<String>>,
    pub dir: Option<PathBuf>,
}

pub fn adaptation_file_stem(model: &str) -> String {
    let tail = model.rsplit('/').next().unwrap_or(model);
    let mut stem = String::new();
    let mut dashed = false;
    for ch in tail.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            stem.push(ch);
            dashed = false;
        } else if !dashed {
            stem.push('-');
            dashed = true;
        }
    }
    stem.trim_matches(['-', '.']).to_string()
}

pub fn load_model_adaptation(
    input: &LoadAdaptationInput,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let stems = input.candidates.clone().unwrap_or_else(|| {
        if input.dir.is_none() {
            family_adaptation_prefixes(&input.model)
        } else {
            vec![adaptation_file_stem(&input.model)]
        }
    });
    let usable: Vec<String> = stems.into_iter().filter(|stem| !stem.is_empty()).collect();
    if usable.is_empty() {
        return None;
    }
    if input.dir.is_none() {
        for stem in usable {
            if stem == "deepseek" {
                return clamp_adaptation(DEEPSEEK_ADAPTATION, &stem, warnings);
            }
        }
        return None;
    }
    load_from(input.dir.as_deref()?, &usable, warnings)
}

pub fn has_curated_adaptation(name: &str) -> bool {
    resolve_model_family(name).is_some_and(|family| family.id == "deepseek")
}

pub fn load_curated_adaptation(name: &str, warnings: &mut Vec<String>) -> Option<String> {
    if !has_curated_adaptation(name) {
        return None;
    }
    let stems = family_adaptation_prefixes(name);
    if stems.is_empty() {
        return None;
    }
    let adaptation = load_model_adaptation(
        &LoadAdaptationInput {
            model: name.to_string(),
            candidates: Some(stems),
            dir: None,
        },
        warnings,
    )?;
    Some(render_curated_adaptation(name, &adaptation))
}

pub fn render_curated_adaptation(model: &str, adaptation: &str) -> String {
    [
        format!("# Model Adaptation: {model}"),
        String::new(),
        "Curated guidance for this model family, authored for this repository and hand-written."
            .to_string(),
        "These are standing instructions for how to work, not measured tendencies and not model probes."
            .to_string(),
        "Where they conflict with the instructions above, the instructions above win.".to_string(),
        String::new(),
        strip_generated_header(adaptation),
    ]
    .join("\n")
}

pub fn truncate_at_section(text: &str, limit: usize) -> String {
    if utf16_len(text) <= limit {
        return text.to_string();
    }
    let mut boundaries = Vec::new();
    let mut fence_spans = Vec::new();
    let mut fence = None;
    let mut open_fence_at = None;
    let mut offset = 0;
    for line in text.split('\n') {
        if let Some(marker) = fence_marker(line) {
            if fence.is_none() {
                fence = Some(marker);
                open_fence_at = Some(offset);
            } else if fence == Some(marker) {
                fence = None;
                fence_spans.push((open_fence_at.take().unwrap_or(0), offset + utf16_len(line)));
            }
        } else if fence.is_none() && is_level_two_heading(line) {
            boundaries.push(offset);
        }
        offset += utf16_len(line) + 1;
    }
    if let Some(start) = open_fence_at {
        fence_spans.push((start, offset));
    }
    let cut = boundaries
        .into_iter()
        .filter(|boundary| *boundary != 0 && *boundary <= limit)
        .last();
    if let Some(cut) = cut.filter(|cut| *cut > 0) {
        return slice_utf16(text, 0, cut).trim_end().to_string();
    }
    let enclosing = fence_spans
        .into_iter()
        .find(|(start, end)| *start > 0 && *start < limit && limit < *end);
    let safe_limit = enclosing.map_or(limit, |(start, _)| limit.min(start));
    slice_utf16(text, 0, safe_limit).trim_end().to_string()
}

fn family_adaptation_prefixes(name: &str) -> Vec<String> {
    resolve_model_family(name)
        .map(|family| vec![family.id.to_string()])
        .unwrap_or_default()
}

fn clamp_adaptation(text: &str, stem: &str, warnings: &mut Vec<String>) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if utf16_len(trimmed) <= MAX_ADAPTATION_BYTES {
        return Some(trimmed.to_string());
    }
    let truncated = truncate_at_section(trimmed, MAX_ADAPTATION_BYTES);
    warnings.push(format!(
        "model adaptation {stem}.md exceeds {MAX_ADAPTATION_BYTES} bytes; truncated to {}",
        utf16_len(&truncated)
    ));
    Some(truncated)
}

fn load_from(dir: &Path, stems: &[String], warnings: &mut Vec<String>) -> Option<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                warnings.push(format!("model adaptations directory unreadable: {error}"));
            }
            return None;
        }
    };
    let mut by_lower_name = HashMap::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        by_lower_name.insert(name.to_lowercase(), name);
    }
    let mut matched = None;
    for stem in stems {
        if let Some(found) = by_lower_name.get(&format!("{stem}.md")) {
            matched = Some(found.clone());
            break;
        }
    }
    let matched = matched?;
    let path = dir.join(&matched);
    match fs::read(path) {
        Ok(bytes) => clamp_adaptation(
            &String::from_utf8_lossy(&bytes),
            matched.strip_suffix(".md").unwrap_or(&matched),
            warnings,
        ),
        Err(error) => {
            warnings.push(format!("model adaptation {matched} unreadable: {error}"));
            None
        }
    }
}

fn strip_generated_header(adaptation: &str) -> String {
    let lines: Vec<&str> = adaptation.split('\n').collect();
    let mut index = 0;
    if lines
        .first()
        .is_some_and(|line| line.starts_with("# Model Adaptation:"))
    {
        index += 1;
    }
    while index < lines.len() {
        let line = lines[index];
        let provenance = line.starts_with('#')
            && !line.starts_with("##")
            && line[1..].chars().next().is_none_or(char::is_whitespace);
        if line.trim().is_empty() || provenance {
            index += 1;
        } else {
            break;
        }
    }
    lines[index..].join("\n").trim().to_string()
}

fn fence_marker(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') || trimmed.chars().take(3).count() < 3 {
        return None;
    }
    let count = trimmed.chars().take_while(|ch| *ch == marker).count();
    (count >= 3).then_some(marker)
}

fn is_level_two_heading(line: &str) -> bool {
    line.strip_prefix("##")
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_whitespace)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn slice_utf16(text: &str, start: usize, end: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    String::from_utf16_lossy(&units[start.min(units.len())..end.min(units.len())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stems_match_the_v2_sanitizer() {
        assert_eq!(
            adaptation_file_stem("openrouter/DeepSeek R1"),
            "deepseek-r1"
        );
        assert_eq!(adaptation_file_stem("  ..--A B--..  "), "a-b");
        assert_eq!(adaptation_file_stem("xopdeepseek-chat"), "xopdeepseek-chat");
    }

    #[test]
    fn curated_adaptation_is_limited_to_the_deepseek_family() {
        assert!(has_curated_adaptation("openrouter/deepseek/deepseek-chat"));
        assert!(!has_curated_adaptation("deepseek-chat-clone"));
        assert!(!has_curated_adaptation("kimi-k2"));
        let rendered = load_curated_adaptation("deepseek-chat", &mut Vec::new()).unwrap();
        assert!(rendered.starts_with("# Model Adaptation: deepseek-chat\n\nCurated guidance"));
        assert!(rendered.contains("## Autonomy and persistence"));
        assert!(!rendered.contains("Not measured: no script generated"));
    }

    #[test]
    fn directory_loading_matches_case_insensitively_and_preserves_candidate_order() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Other.md"), "other").unwrap();
        fs::write(dir.path().join("DeepSeek.MD"), "  deepseek body  ").unwrap();
        let loaded = load_model_adaptation(
            &LoadAdaptationInput {
                model: "deepseek-chat".into(),
                candidates: Some(vec!["deepseek".into(), "other".into()]),
                dir: Some(dir.path().to_path_buf()),
            },
            &mut Vec::new(),
        );
        assert_eq!(loaded.as_deref(), Some("deepseek body"));
    }

    #[test]
    fn missing_directory_and_empty_files_return_none() {
        let dir = tempdir().unwrap();
        let missing = load_model_adaptation(
            &LoadAdaptationInput {
                model: "x".into(),
                candidates: Some(vec!["x".into()]),
                dir: Some(dir.path().join("missing")),
            },
            &mut Vec::new(),
        );
        assert_eq!(missing, None);
        fs::write(dir.path().join("x.md"), " \n").unwrap();
        let empty = load_model_adaptation(
            &LoadAdaptationInput {
                model: "x".into(),
                candidates: None,
                dir: Some(dir.path().to_path_buf()),
            },
            &mut Vec::new(),
        );
        assert_eq!(empty, None);
    }

    #[test]
    fn truncation_prefers_a_section_boundary() {
        let text = format!("{}\n\n## Next\n{}", "a".repeat(20), "b".repeat(50));
        assert_eq!(truncate_at_section(&text, 30), "a".repeat(20));
    }

    #[test]
    fn truncation_does_not_cut_through_a_fence() {
        let text = format!("intro\n\n```rust\n{}\n```\n\n## Next\nbody", "x".repeat(80));
        assert_eq!(truncate_at_section(&text, 30), "intro");
    }

    #[test]
    fn truncation_uses_the_hard_limit_outside_fences() {
        assert_eq!(
            truncate_at_section(&"abcdefgh".repeat(10), 17),
            "abcdefghabcdefgha"
        );
    }

    #[test]
    fn oversized_adaptations_warn_and_clamp() {
        let mut warnings = Vec::new();
        let loaded = clamp_adaptation(
            &"a".repeat(MAX_ADAPTATION_BYTES + 1),
            "deepseek",
            &mut warnings,
        )
        .unwrap();
        assert_eq!(utf16_len(&loaded), MAX_ADAPTATION_BYTES);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("exceeds 32768 bytes"));
    }
}
