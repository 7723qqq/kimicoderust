use async_trait::async_trait;

pub const SESSION_INIT_SERVICE_ID: &str = "sessionInitService";

#[async_trait]
pub trait ISessionInitService: Send + Sync {
    async fn generate_agents_md(&self) -> Result<(), String>;
    fn cancel_init(&self);
}
