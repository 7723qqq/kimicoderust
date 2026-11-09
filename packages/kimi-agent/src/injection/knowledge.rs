//! Knowledge-base reminder (v2 `KnowledgeInjection`,
//! `agent/knowledge/knowledgeInjection.ts`).
//!
//! On the first step of every turn the provider extracts search signals from
//! the most recent user message, queries the local knowledge database with the
//! injection confidence floor, and renders at most five standards in under 800
//! tokens. The learner (v2 `KnowledgeLearner`) runs at turn end and stores a
//! user correction as an `ai-learned` entry.
//!
//! v2 registers both only for the `main` agent (`knowledgeService.ts:46-49`).

use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use super::InjectionContext;
use crate::turn_loop::types::LLMMessage;

/// v2 `MAX_INJECTION_TOKENS` (`knowledgeInjection.ts:7`).
const MAX_INJECTION_TOKENS: usize = 800;
/// v2 `MAX_ENTRIES` (`knowledgeInjection.ts:8`).
const MAX_ENTRIES: u32 = 5;
/// v2 `INJECTION_CONFIDENCE_FLOOR` (`knowledgeService.ts:24`).
const INJECTION_CONFIDENCE_FLOOR: f64 = 0.5;
/// v2 `LEARNED_CONFIDENCE` (`knowledgeLearner.ts:8`).
const LEARNED_CONFIDENCE: f64 = 0.4;

/// v2 `CORRECTION_PATTERNS` (`knowledgeLearner.ts:10-21`).
static CORRECTION_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"不对[，,]?应该",
        r"错了[，,]?是",
        r"不是.*而是",
        r"应该用.*不是",
        r"用\s*\S+\s*不要用",
        r"别用.*用",
        r"must use|should use|don't use|do not use",
        r"规范是|标准是|约定是",
        r"记住[：:]",
        r"以后.*要",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid correction pattern"))
    .collect()
});

/// v2 `extractSignals`'s file-path probe (`knowledgeInjection.ts:65`).
static SCOPE_PATH_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:[\w./\\-]+\.(?:ts|js|rs|vue|tsx|jsx|json|md))")
        .expect("valid scope path pattern")
});

/// v2 `learnFromCorrection`'s category and tag probes
/// (`knowledgeLearner.ts:62-71`).
static STYLE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)import|命名|格式|style|naming").expect("valid style pattern"));
static ARCHITECTURE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)架构|设计|依赖|architecture").expect("valid architecture pattern")
});
static WORKFLOW_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)流程|提交|commit|push|workflow").expect("valid workflow pattern")
});
static TYPESCRIPT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)typescript|\.ts").expect("valid typescript pattern"));
static RUST_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)rust|\.rs").expect("valid rust pattern"));
static IMPORT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)import").expect("valid import pattern"));
static GIT_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)git|commit|push").expect("valid git pattern"));

#[derive(Deserialize)]
struct InjectedEntry {
    category: String,
    title: String,
    content: String,
}

#[derive(Deserialize)]
struct InjectedResult {
    entry: InjectedEntry,
}

struct Signals {
    query: String,
    scope_path: Option<String>,
    tags: Vec<String>,
}

/// Register the `knowledge` reminder provider.
///
/// `workspace_root` resolves the database
/// (`<workspace>/.kimi-code/knowledge.db`) — the session's work dir, not the
/// engine process's cwd. The provider stays silent when no database exists,
/// matching v2's `openExisting` read path, which never creates one; a search
/// or decode failure is logged once per turn instead of disappearing.
pub fn register_knowledge_injection(
    registry: &mut crate::injection::InjectionRegistry,
    workspace_root: Option<PathBuf>,
) {
    let mut warned = false;
    registry.register(
        "knowledge",
        Box::new(move |context: &InjectionContext| {
            // Injection only reads, but it shares the process-global
            // connection with the knowledge tests; without this guard a turn
            // running in parallel re-points the singleton at another database
            // mid-assertion (the `total == 0` / 140-mismatch failure).
            #[cfg(test)]
            let _guard = {
                crate::knowledge::isolate_test_db();
                crate::knowledge::test_lock()
            };
            if !context.is_new_turn || context.injected.iter().any(|name| name == "knowledge") {
                return None;
            }
            let root = workspace_root.as_deref()?;
            let signals = extract_signals(context.history);
            if signals.query.is_empty() && signals.scope_path.is_none() {
                return None;
            }
            if !crate::knowledge::open_existing(root) {
                return None;
            }
            let tags = (!signals.tags.is_empty()).then(|| signals.tags.join(","));
            let json = match crate::knowledge::search(
                signals.query,
                signals.scope_path,
                tags,
                MAX_ENTRIES,
                INJECTION_CONFIDENCE_FLOOR,
            ) {
                Ok(json) => json,
                Err(error) => {
                    if !warned {
                        warned = true;
                        tracing::warn!(%error, "knowledge search failed; skipping the knowledge injection");
                    }
                    return None;
                }
            };
            let results: Vec<InjectedResult> = match serde_json::from_str(&json) {
                Ok(results) => results,
                Err(error) => {
                    if !warned {
                        warned = true;
                        tracing::warn!(%error, "knowledge search returned an unexpected shape; skipping the knowledge injection");
                    }
                    return None;
                }
            };
            if results.is_empty() {
                return None;
            }
            Some(format_injection(&results))
        }),
    );
}

/// v2 `extractSignals` (`knowledgeInjection.ts:39-75`).
fn extract_signals(history: &[LLMMessage]) -> Signals {
    let last_user_text = history
        .iter()
        .rev()
        .find(|message| message.role == "user" && !message.content.is_empty())
        .map(|message| message.content.as_str())
        .unwrap_or_default();

    let head: String = last_user_text.chars().take(200).collect();
    let cleaned = head.replace(['`', '"', '\'', '(', ')', '[', ']', '{', '}'], " ");
    let query = cleaned
        .split_whitespace()
        .filter(|word| word.chars().count() > 2)
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");

    let mut tags = Vec::new();
    if last_user_text.contains(".ts") || last_user_text.contains("typescript") {
        tags.push("typescript".to_string());
    }
    if last_user_text.contains(".rs") || last_user_text.contains("rust") {
        tags.push("rust".to_string());
    }
    if last_user_text.contains("test") {
        tags.push("testing".to_string());
    }
    if last_user_text.contains("import") {
        tags.push("import".to_string());
    }
    tags.dedup();

    Signals {
        query,
        scope_path: SCOPE_PATH_PATTERN
            .find(last_user_text)
            .map(|found| found.as_str().to_string()),
        tags,
    }
}

/// v2 `formatInjection` (`knowledgeInjection.ts:77-92`).
fn format_injection(results: &[InjectedResult]) -> String {
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
            result.entry.category,
            result.entry.title,
            first_line
        );
        let line_tokens = line.chars().count().div_ceil(4);
        if tokens_used + line_tokens > MAX_INJECTION_TOKENS {
            break;
        }
        lines.push(line);
        lines.push(String::new());
        tokens_used += line_tokens;
    }

    lines.join("\n")
}

/// v2 `KnowledgeLearner.onTurnEnded` (`knowledgeLearner.ts:34-53`): scan the
/// last five messages for a user correction and store the first match.
pub fn learn_from_turn(history: &[LLMMessage], workspace_root: &Path) {
    // Tests must never resolve (and then write) the developer's real user
    // database, and the learning path is the one that writes: pin the test
    // database and serialize against the knowledge tests that hold the same
    // global connection.
    #[cfg(test)]
    let _guard = {
        crate::knowledge::isolate_test_db();
        crate::knowledge::test_lock()
    };
    let start = history.len().saturating_sub(5);
    for message in history[start..].iter().rev() {
        if message.role != "user" || message.content.is_empty() {
            continue;
        }
        if CORRECTION_PATTERNS
            .iter()
            .any(|pattern| pattern.is_match(&message.content))
        {
            learn_from_correction(&message.content, workspace_root);
            return;
        }
    }
}

/// v2 `learnFromCorrection` (`knowledgeLearner.ts:55-94`).
fn learn_from_correction(text: &str, workspace_root: &Path) {
    let title = text
        .chars()
        .take(60)
        .collect::<String>()
        .replace(['\n', '\r'], " ");
    let title = title.trim();
    if title.chars().count() < 5 {
        return;
    }
    // v2 `add` goes through `ensureDatabase`, the write path that creates the
    // user database when neither the project nor the user one exists.
    if !crate::knowledge::ensure_open(workspace_root) {
        return;
    }

    // v2 `learnFromCorrection` starts at `pitfall` and lets each later match
    // override the previous one, so a text matching both a style word and an
    // architecture word lands on `architecture`.
    let mut category = "pitfall";
    if STYLE_PATTERN.is_match(text) {
        category = "coding-style";
    }
    if ARCHITECTURE_PATTERN.is_match(text) {
        category = "architecture";
    }
    if WORKFLOW_PATTERN.is_match(text) {
        category = "workflow";
    }
    let category = category.to_string();

    let mut tags = Vec::new();
    if TYPESCRIPT_PATTERN.is_match(text) {
        tags.push("typescript");
    }
    if RUST_PATTERN.is_match(text) {
        tags.push("rust");
    }
    if IMPORT_PATTERN.is_match(text) {
        tags.push("import");
    }
    if GIT_PATTERN.is_match(text) {
        tags.push("git");
    }

    match crate::knowledge::add(
        title.to_string(),
        category.to_string(),
        text.to_string(),
        tags.join(","),
        None,
        "ai-learned".to_string(),
        LEARNED_CONFIDENCE,
    ) {
        Ok(entry) => tracing::info!(entry = %entry, "knowledge learned from a user correction"),
        Err(error) => tracing::warn!(
            %error,
            title,
            category,
            "knowledge learn failed; the correction was not stored"
        ),
    }
}
