use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{ContextMessage, Role};
use crate::agent::knowledge::knowledge::{IAgentKnowledgeService, KnowledgeSearchResult};
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::{ContextInjectionContent, ContextInjectionResult};
use crate::human::llm::message::ContentPart;
use crate::wire::record::WireAsyncError;

const MAX_INJECTION_TOKENS: usize = 800;

const MAX_ENTRIES: usize = 5;

static SCOPE_PATH_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:[A-Za-z0-9_./\\-]+\.(?:ts|js|rs|vue|tsx|jsx|json|md))")
        .expect("valid scope path pattern")
});

struct Signals {
    query: String,
    scope_path: Option<String>,
    tags: Vec<String>,
}

pub struct KnowledgeInjection {
    _registration: Arc<dyn Fn() + Send + Sync>,
}

impl KnowledgeInjection {
    pub fn new(
        knowledge: Arc<dyn IAgentKnowledgeService>,
        reminders: Arc<dyn ReminderService>,
        context_memory: Arc<dyn IAgentContextMemoryService>,
    ) -> Self {
        let registration = reminders.register(
            "knowledge",
            Arc::new(move |context| {
                let knowledge = knowledge.clone();
                let context_memory = context_memory.clone();
                Box::pin(async move {
                    if !context.is_new_turn {
                        return Ok(None);
                    }
                    let injection = get_injection(knowledge.as_ref(), context_memory.as_ref())
                        .map_err(|error| error.to_string())?;
                    Ok(injection.map(|content| ContextInjectionResult {
                        content: ContextInjectionContent::Text(content),
                        disclosure: None,
                    }))
                })
            }),
        );
        Self {
            _registration: registration,
        }
    }
}

fn get_injection(
    knowledge: &dyn IAgentKnowledgeService,
    context_memory: &dyn IAgentContextMemoryService,
) -> Result<Option<String>, WireAsyncError> {
    let signals = extract_signals(&context_memory.get());
    if signals.query.is_empty() && signals.scope_path.is_none() {
        return Ok(None);
    }
    let results = knowledge.search(
        &signals.query,
        signals.scope_path.as_deref(),
        Some(signals.tags.as_slice()),
        Some(MAX_ENTRIES),
    )?;
    if results.is_empty() {
        return Ok(None);
    }
    Ok(Some(format_injection(&results)))
}

fn extract_signals(history: &[ContextMessage]) -> Signals {
    let mut tags: Vec<String> = Vec::new();

    let mut last_user_text = String::new();
    for message in history.iter().rev() {
        if message.role != Role::User || message.content.is_empty() {
            continue;
        }
        for part in &message.content {
            if let ContentPart::Text { text, .. } = part
                && !text.is_empty()
            {
                last_user_text = text.clone();
                break;
            }
        }
        if !last_user_text.is_empty() {
            break;
        }
    }

    let head = slice_utf16(&last_user_text, 200);
    let cleaned = head.replace(['`', '"', '\'', '(', ')', '[', ']', '{', '}'], " ");
    let query = cleaned
        .split(is_js_whitespace)
        .filter(|word| utf16_len(word) > 2)
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");

    let scope_path = SCOPE_PATH_PATTERN
        .find(&last_user_text)
        .map(|found| found.as_str().to_string());

    if last_user_text.contains(".ts") || last_user_text.contains("typescript") {
        push_tag(&mut tags, "typescript");
    }
    if last_user_text.contains(".rs") || last_user_text.contains("rust") {
        push_tag(&mut tags, "rust");
    }
    if last_user_text.contains("test") {
        push_tag(&mut tags, "testing");
    }
    if last_user_text.contains("import") {
        push_tag(&mut tags, "import");
    }

    Signals {
        query,
        scope_path,
        tags,
    }
}

fn format_injection(results: &[KnowledgeSearchResult]) -> String {
    let mut lines = vec![
        "[Knowledge Base — Relevant Standards]".to_string(),
        String::new(),
    ];
    let mut tokens_used = 10;

    for (index, result) in results.iter().enumerate() {
        let first_line = result.entry.content.split('\n').next().unwrap_or_default();
        let line = format!(
            "{}. [{}] {}\n   {}",
            index + 1,
            result.entry.category.as_str(),
            result.entry.title,
            first_line
        );
        let line_tokens = utf16_len(&line).div_ceil(4);
        if tokens_used + line_tokens > MAX_INJECTION_TOKENS {
            break;
        }
        lines.push(line);
        lines.push(String::new());
        tokens_used += line_tokens;
    }

    lines.join("\n")
}

fn push_tag(tags: &mut Vec<String>, tag: &str) {
    if !tags.iter().any(|existing| existing == tag) {
        tags.push(tag.to_string());
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn slice_utf16(text: &str, max: usize) -> String {
    String::from_utf16_lossy(&text.encode_utf16().take(max).collect::<Vec<_>>())
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}
