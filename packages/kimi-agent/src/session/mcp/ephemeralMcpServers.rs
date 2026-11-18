use std::collections::HashMap;
use std::sync::Arc;

use crate::_base::di::instantiation::create_decorator;
use crate::_base::di::scope::{ScopeSeed, seed_instance};
use crate::mcp_core::config_schema::McpServerConfig;

pub type ISessionEphemeralMcpServers = HashMap<String, McpServerConfig>;

pub fn session_ephemeral_mcp_servers_seed(
    servers: ISessionEphemeralMcpServers,
) -> ScopeSeed {
    let id = create_decorator::<ISessionEphemeralMcpServers>("sessionEphemeralMcpServers");
    seed_instance(&id, Arc::new(servers))
}
