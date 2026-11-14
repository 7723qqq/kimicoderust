use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTitleSource {
    UserPrompts,
    FirstTurn,
    Digest,
}

impl SessionTitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserPrompts => "user_prompts",
            Self::FirstTurn => "first_turn",
            Self::Digest => "digest",
        }
    }
}

#[async_trait::async_trait]
pub trait ISessionTitleService: Send + Sync {
    async fn generate_title(
        &self,
        force: Option<bool>,
        source: Option<SessionTitleSource>,
    ) -> Result<Option<String>, WireAsyncError>;
}
