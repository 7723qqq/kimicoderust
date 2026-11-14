use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::profile::init::DEFAULT_INIT_PROMPT;
use super::sessionInit::ISessionInitService;

pub const INIT_PROFILE_NAME: &str = "coder";
pub const INIT_PARENT_TOOL_CALL_ID: &str = "generate-agents-md";
pub const INIT_DESCRIPTION: &str = "Initialize AGENTS.md";
pub const INIT_LABELS: &[(&str, &str)] = &[("sessionInit", "agents-md")];

pub type SessionInitRunner = Arc<
    dyn Fn(
            &'static str,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

pub struct SessionInitService {
    runner: SessionInitRunner,
    next_run_id: AtomicU64,
    init_run: Mutex<Option<(u64, CancellationToken)>>,
}

impl SessionInitService {
    pub fn new(runner: SessionInitRunner) -> Self {
        Self {
            runner,
            next_run_id: AtomicU64::new(0),
            init_run: Mutex::new(None),
        }
    }

    pub fn cancel_init(&self) {
        if let Some((_, token)) = self.init_run.lock().unwrap().as_ref() {
            token.cancel();
        }
    }

    pub async fn generate_agents_md(&self) -> Result<(), String> {
        let run_id = self.next_run_id.fetch_add(1, Ordering::SeqCst) + 1;
        let token = CancellationToken::new();
        *self.init_run.lock().unwrap() = Some((run_id, token.clone()));
        let result = (self.runner)(DEFAULT_INIT_PROMPT, token).await;
        let mut init_run = self.init_run.lock().unwrap();
        if init_run.as_ref().is_some_and(|(id, _)| *id == run_id) {
            *init_run = None;
        }
        result
    }
}

#[async_trait]
impl ISessionInitService for SessionInitService {
    async fn generate_agents_md(&self) -> Result<(), String> {
        Self::generate_agents_md(self).await
    }

    fn cancel_init(&self) {
        Self::cancel_init(self);
    }
}
