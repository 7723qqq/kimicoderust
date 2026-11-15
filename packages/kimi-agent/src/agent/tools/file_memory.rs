use crate::rpc::types::BoxFuture;
use crate::rpc::types::ContentPart;
use crate::turn_loop::types::ExecutableToolResult;

pub trait FileMemoryProvider: Send + Sync {
    fn file_block<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>>;
}

pub async fn with_file_memory(
    mut result: ExecutableToolResult,
    path: &str,
    service: Option<&dyn FileMemoryProvider>,
) -> ExecutableToolResult {
    if result.is_error {
        return result;
    }
    let Some(service) = service else {
        return result;
    };
    let Ok(block) = service.file_block(path).await else {
        return result;
    };
    if block.is_empty() {
        return result;
    }
    if result.parts.is_empty() {
        result.content = format!("{}\n{}", result.content, block);
    } else {
        result.parts.push(ContentPart::Text {
            text: format!("\n{block}"),
            meta: None,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    struct ProbeMemory {
        block: Result<String, String>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl FileMemoryProvider for ProbeMemory {
        fn file_block<'a>(&'a self, path: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(path.to_string());
                self.block.clone()
            })
        }
    }

    fn result(content: &str) -> ExecutableToolResult {
        ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: content.to_string(),
            is_error: false,
            note: None,
            display: None,
        }
    }

    fn memory(block: Result<&str, &str>, seen: Arc<Mutex<Vec<String>>>) -> ProbeMemory {
        ProbeMemory {
            block: block.map(str::to_string).map_err(str::to_string),
            seen,
        }
    }

    #[tokio::test]
    async fn a_successful_result_gets_the_block_appended_after_a_newline() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = memory(Ok("Related commits:\n- abc"), seen.clone());
        let merged = with_file_memory(
            result("Replaced 1 occurrence in a.rs"),
            "a.rs",
            Some(&provider),
        )
        .await;
        assert_eq!(
            merged.content,
            "Replaced 1 occurrence in a.rs\nRelated commits:\n- abc"
        );
        assert_eq!(*seen.lock().unwrap(), vec!["a.rs".to_string()]);
    }

    #[tokio::test]
    async fn a_block_joins_the_parts_when_the_result_carries_them() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = memory(Ok("memory"), seen);
        let mut base = result("Replaced 1 occurrence in a.rs");
        base.parts.push(ContentPart::Text {
            text: "image".to_string(),
            meta: None,
        });
        let merged = with_file_memory(base, "a.rs", Some(&provider)).await;
        assert_eq!(merged.content, "Replaced 1 occurrence in a.rs");
        match merged.parts.last() {
            Some(ContentPart::Text { text, .. }) => assert_eq!(text, "\nmemory"),
            other => panic!("expected an appended text part, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn error_results_are_returned_untouched() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = memory(Ok("memory"), seen.clone());
        let mut base = result("boom");
        base.is_error = true;
        let merged = with_file_memory(base, "a.rs", Some(&provider)).await;
        assert_eq!(merged.content, "boom");
        assert!(merged.is_error);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_provider_is_a_noop() {
        let merged = with_file_memory(result("ok"), "a.rs", None).await;
        assert_eq!(merged.content, "ok");
    }

    #[tokio::test]
    async fn an_empty_block_is_a_noop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = memory(Ok(""), seen);
        let merged = with_file_memory(result("ok"), "a.rs", Some(&provider)).await;
        assert_eq!(merged.content, "ok");
        assert!(merged.parts.is_empty());
    }

    #[tokio::test]
    async fn a_provider_failure_is_a_noop() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = memory(Err("index unavailable"), seen);
        let merged = with_file_memory(result("ok"), "a.rs", Some(&provider)).await;
        assert_eq!(merged.content, "ok");
        assert!(merged.parts.is_empty());
    }
}
