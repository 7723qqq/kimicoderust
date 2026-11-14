use crate::swarm::agent_run_batch::{AgentRunResult, AgentRunTask};

pub const SESSION_SWARM_SERVICE_ID: &str = "sessionSwarmService";

pub type SessionSwarmTask<T> = AgentRunTask<T>;
pub type SessionSwarmRunResult<T> = AgentRunResult<T>;

pub struct SessionSwarmRunArgs<T> {
    pub caller_agent_id: String,
    pub tasks: Vec<SessionSwarmTask<T>>,
}
