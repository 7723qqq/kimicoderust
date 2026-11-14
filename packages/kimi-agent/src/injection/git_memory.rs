use std::sync::Arc;

use crate::injection::{InjectionContext, InjectionProvider};
use crate::tools::git_memory::GitMemoryService;
use crate::turn_loop::types::LLMMessage;

pub fn git_memory_signal_provider(service: Arc<GitMemoryService>) -> InjectionProvider {
    Box::new(move |ctx: &InjectionContext| {
        if !ctx.is_new_turn {
            return None;
        }
        let text = last_user_text(ctx.history)?;
        let block = service.signal_block(&text);
        (!block.is_empty()).then_some(block)
    })
}

fn last_user_text(history: &[LLMMessage]) -> Option<String> {
    for message in history.iter().rev() {
        if message.role != "user" {
            continue;
        }
        for block in &message.blocks {
            if let crate::rpc::types::ContentBlock::Text { text, .. } = block
                && !text.is_empty()
            {
                return Some(text.clone());
            }
        }
        if !message.content.is_empty() {
            return Some(message.content.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::StepAccess;
    use crate::tools::memory_repo::MemoryRepo;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn service(workspace: &Path, memory: &Path) -> Arc<GitMemoryService> {
        Arc::new(GitMemoryService::with_enabled(
            workspace.to_path_buf(),
            memory.to_path_buf(),
            true,
        ))
    }

    fn history(text: &str) -> Vec<LLMMessage> {
        vec![LLMMessage::new("user", text)]
    }

    #[test]
    fn a_new_turn_injects_the_commits_for_a_mentioned_path() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/a.ts"), "a\n").unwrap();
        assert!(MemoryRepo::new(workspace.path().to_path_buf()).commit("add a"));

        let mut provider = git_memory_signal_provider(service(workspace.path(), memory.path()));
        let access = StepAccess::default();
        let injected = Vec::new();
        let history = history("please fix src/a.ts");
        let ctx = InjectionContext {
            is_new_turn: true,
            rearmed: false,
            injected: &injected,
            access: &access,
            history: &history,
        };
        let block = provider(&ctx).expect("a quoted path injects the signal block");
        assert!(
            block.contains("<repo-memory signal=\"src/a.ts\">"),
            "{block}"
        );
        assert!(block.contains("add a"), "{block}");
        let again = provider(&ctx).expect("a rearmed pass still renders the seen-commit line");
        assert!(again.contains("see commit"), "{again}");
        assert!(!again.contains("add a"), "{again}");
    }

    #[test]
    fn a_later_step_or_a_path_less_message_injects_nothing() {
        if !git_available() {
            return;
        }
        let workspace = tempfile::tempdir().unwrap();
        let memory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/a.ts"), "a\n").unwrap();
        assert!(MemoryRepo::new(workspace.path().to_path_buf()).commit("add a"));

        let mut provider = git_memory_signal_provider(service(workspace.path(), memory.path()));
        let access = StepAccess::default();
        let injected = Vec::new();
        let history = vec![LLMMessage::new("user", "please fix src/a.ts")];
        let ctx = InjectionContext {
            is_new_turn: false,
            rearmed: false,
            injected: &injected,
            access: &access,
            history: &history,
        };
        assert!(provider(&ctx).is_none());

        let pathless = vec![LLMMessage::new("user", "no file mentioned here")];
        let ctx = InjectionContext {
            is_new_turn: true,
            rearmed: false,
            injected: &injected,
            access: &access,
            history: &pathless,
        };
        assert!(provider(&ctx).is_none());
    }
}
