use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::_base::log::log::ILogger;
use crate::human::llm::model_family::{family_adaptation_prefixes, resolve_model_family};

const MAX_ADAPTATION_BYTES: usize = 32 * 1024;
const DEEPSEEK_ADAPTATION: &str = include_str!("../../llm/model-adaptations/curated/deepseek.md");

#[derive(Clone, Default)]
pub struct LoadAdaptationInput {
    pub model: String,
    pub candidates: Option<Vec<String>>,
    pub log: Option<Arc<dyn ILogger>>,
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

pub async fn load_model_adaptation(input: &LoadAdaptationInput) -> Option<String> {
    let stems = input.candidates.clone().unwrap_or_else(|| {
        if input.dir.is_none() {
            family_adaptation_prefixes(&input.model).to_vec()
        } else {
            vec![adaptation_file_stem(&input.model)]
        }
    });
    let usable = stems
        .into_iter()
        .filter(|stem| !stem.is_empty())
        .collect::<Vec<_>>();
    if usable.is_empty() {
        return None;
    }
    if input.dir.is_none() {
        for stem in usable {
            if stem == "deepseek" {
                return clamp_adaptation(DEEPSEEK_ADAPTATION, &stem, input.log.as_ref());
            }
        }
        return None;
    }
    load_from(input.dir.as_deref()?, &usable, input.log.as_ref()).await
}

fn clamp_adaptation(
    text: &str,
    stem: &str,
    log: Option<&Arc<dyn ILogger>>,
) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if utf16_len(trimmed) <= MAX_ADAPTATION_BYTES {
        return Some(trimmed.to_string());
    }
    let truncated = truncate_at_section(trimmed, MAX_ADAPTATION_BYTES);
    if let Some(log) = log {
        log.warn(
            &format!(
                "model adaptation {stem}.md exceeds {MAX_ADAPTATION_BYTES} bytes; truncated to {}",
                utf16_len(&truncated)
            ),
            None,
        );
    }
    Some(truncated)
}

async fn load_from(
    dir: &Path,
    stems: &[String],
    log: Option<&Arc<dyn ILogger>>,
) -> Option<String> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound
                && let Some(log) = log
            {
                log.warn(
                    &format!("model adaptations directory unreadable: {error}"),
                    None,
                );
            }
            return None;
        }
    };
    let mut names = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => names.push(entry.file_name().to_string_lossy().into_owned()),
            Ok(None) => break,
            Err(error) => {
                if let Some(log) = log {
                    log.warn(
                        &format!("model adaptations directory unreadable: {error}"),
                        None,
                    );
                }
                return None;
            }
        }
    }
    let by_lower_name = names
        .into_iter()
        .map(|name| (name.to_lowercase(), name))
        .collect::<HashMap<_, _>>();
    let mut matched = None;
    for stem in stems {
        if let Some(found) = by_lower_name.get(&format!("{stem}.md")) {
            matched = Some(found.clone());
            break;
        }
    }
    let matched = matched?;
    match tokio::fs::read(dir.join(&matched)).await {
        Ok(bytes) => clamp_adaptation(
            &String::from_utf8_lossy(&bytes),
            matched.strip_suffix(".md").unwrap_or(&matched),
            log,
        ),
        Err(error) => {
            if let Some(log) = log {
                log.warn(
                    &format!("model adaptation {matched} unreadable: {error}"),
                    None,
                );
            }
            None
        }
    }
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
    let mut cut = None;
    for boundary in boundaries {
        if boundary == 0 {
            continue;
        }
        if boundary <= limit {
            cut = Some(boundary);
        } else {
            break;
        }
    }
    if let Some(cut) = cut.filter(|cut| *cut > 0) {
        return slice_utf16(text, 0, cut).trim_end().to_string();
    }
    let enclosing = fence_spans
        .into_iter()
        .find(|(start, end)| *start > 0 && *start < limit && limit < *end);
    let safe_limit = enclosing.map_or(limit, |(start, _)| limit.min(start));
    slice_utf16(text, 0, safe_limit).trim_end().to_string()
}

pub const DEEPSEEK_ADAPTATION_SOURCE_ID: &str =
    "packages/agent-core-v2/src/app/agentProfileCatalog/model-adaptations/curated/deepseek.md";

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

pub fn has_curated_adaptation(name: &str) -> bool {
    resolve_model_family(name).is_some_and(|family| family.curated_adaptation == Some(true))
}

pub async fn load_curated_adaptation(
    name: &str,
    log: Option<Arc<dyn ILogger>>,
) -> Option<String> {
    if !has_curated_adaptation(name) {
        return None;
    }
    let stems = family_adaptation_prefixes(name).to_vec();
    if stems.is_empty() {
        return None;
    }
    let adaptation = load_model_adaptation(&LoadAdaptationInput {
        model: name.to_string(),
        candidates: Some(stems),
        log,
        dir: None,
    })
    .await?;
    Some(render_curated_adaptation(name, &adaptation))
}

fn strip_generated_header(adaptation: &str) -> String {
    let lines = adaptation.split('\n').collect::<Vec<_>>();
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
            continue;
        }
        break;
    }
    lines[index..].join("\n").trim().to_string()
}

fn fence_marker(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next()?;
    if !matches!(marker, '`' | '~') {
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
    let units = text.encode_utf16().collect::<Vec<_>>();
    String::from_utf16_lossy(&units[start.min(units.len())..end.min(units.len())])
}
