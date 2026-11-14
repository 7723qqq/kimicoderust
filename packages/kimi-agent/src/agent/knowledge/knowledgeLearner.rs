use std::sync::Arc;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::json;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::Role;
use crate::agent::knowledge::knowledge::{
    IAgentKnowledgeService, KnowledgeAddInput, KnowledgeAddSource, KnowledgeCategory,
};
use crate::app::event::event_bus::IEventBus;
use crate::human::llm::message::ContentPart;

pub const LEARNED_CONFIDENCE: f64 = 0.4;

static CORRECTION_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"不对[，,]?应该",
        r"错了[，,]?是",
        r"不是.*而是",
        r"应该用.*不是",
        r"用\s*\S+\s*不要用",
        r"别用.*用",
        r"(?i-u)must use|should use|don't use|do not use",
        r"规范是|标准是|约定是",
        r"记住[：:]",
        r"以后.*要",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid correction pattern"))
    .collect()
});

static STYLE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i-u)import|命名|格式|style|naming").expect("valid style pattern"));

static ARCHITECTURE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i-u)架构|设计|依赖|architecture").expect("valid architecture pattern")
});

static WORKFLOW_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i-u)流程|提交|commit|push|workflow").expect("valid workflow pattern")
});

static TYPESCRIPT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i-u)typescript|\.ts").expect("valid typescript pattern"));

static RUST_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i-u)rust|\.rs").expect("valid rust pattern"));

static IMPORT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i-u)import").expect("valid import pattern"));

static GIT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i-u)git|commit|push").expect("valid git pattern"));

pub struct KnowledgeLearner {
    _registration: Box<dyn IDisposable>,
}

impl KnowledgeLearner {
    pub fn new(
        knowledge: Arc<dyn IAgentKnowledgeService>,
        event_bus: Arc<dyn IEventBus>,
        context_memory: Arc<dyn IAgentContextMemoryService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        let registration = event_bus.subscribe_type(
            "turn.ended",
            Arc::new(move |_event| {
                on_turn_ended(
                    knowledge.as_ref(),
                    context_memory.as_ref(),
                    log.as_ref(),
                );
            }),
        );
        Self {
            _registration: registration,
        }
    }
}

fn on_turn_ended(
    knowledge: &dyn IAgentKnowledgeService,
    context_memory: &dyn IAgentContextMemoryService,
    log: &dyn ILogService,
) {
    let history = context_memory.get();
    let start = history.len().saturating_sub(5);
    for message in history[start..].iter().rev() {
        if message.role != Role::User {
            continue;
        }
        for part in &message.content {
            let ContentPart::Text { text, .. } = part else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if CORRECTION_PATTERNS
                .iter()
                .any(|pattern| pattern.is_match(text))
            {
                learn_from_correction(knowledge, log, text);
                return;
            }
        }
    }
}

fn learn_from_correction(
    knowledge: &dyn IAgentKnowledgeService,
    log: &dyn ILogService,
    text: &str,
) {
    let title = slice_utf16(text, 60).replace(['\n', '\r'], " ");
    let title = title.trim_matches(is_js_whitespace);
    if utf16_len(title) < 5 {
        return;
    }

    let mut category = KnowledgeCategory::Pitfall;
    if STYLE_PATTERN.is_match(text) {
        category = KnowledgeCategory::CodingStyle;
    }
    if ARCHITECTURE_PATTERN.is_match(text) {
        category = KnowledgeCategory::Architecture;
    }
    if WORKFLOW_PATTERN.is_match(text) {
        category = KnowledgeCategory::Workflow;
    }

    let mut tags = Vec::new();
    if TYPESCRIPT_PATTERN.is_match(text) {
        tags.push("typescript".to_string());
    }
    if RUST_PATTERN.is_match(text) {
        tags.push("rust".to_string());
    }
    if IMPORT_PATTERN.is_match(text) {
        tags.push("import".to_string());
    }
    if GIT_PATTERN.is_match(text) {
        tags.push("git".to_string());
    }

    let entry = knowledge.add(KnowledgeAddInput {
        title: title.to_string(),
        category,
        content: text.to_string(),
        tags: Some(tags),
        scope: None,
        source: Some(KnowledgeAddSource::AiLearned),
        confidence: Some(LEARNED_CONFIDENCE),
    });

    match entry {
        Some(entry) => log.info(
            "knowledge learned from a user correction",
            Some(json!({
                "id": entry.id,
                "category": entry.category.as_str(),
                "title": entry.title,
            })),
        ),
        None => log.warn(
            "knowledge learn failed; the correction was not stored",
            Some(json!({
                "title": title,
                "category": category.as_str(),
            })),
        ),
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
