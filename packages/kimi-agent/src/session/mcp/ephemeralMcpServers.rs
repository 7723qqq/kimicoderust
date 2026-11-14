use std::collections::HashMap;

use crate::_base::di::scope::ScopeSeed;
use crate::mcp_core::config_schema::McpServerConfig;

pub type ISessionEphemeralMcpServers = HashMap<String, McpServerConfig>;

pub fn session_ephemeral_mcp_servers_seed(
    servers: ISessionEphemeralMcpServers,
) -> Vec<ScopeSeed> {
    vec![ScopeSeed::new::<ISessionEphemeralMcpServers>(servers)]
}
