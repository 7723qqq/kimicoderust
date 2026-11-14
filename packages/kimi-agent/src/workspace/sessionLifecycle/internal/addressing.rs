use crate::_base::utils::paths::join;

pub fn workspace_persistence_scope(sessions_scope: &str, workspace_id: &str) -> String {
    join(sessions_scope, &[workspace_id])
}

pub fn session_scope_of(handler_scope: &str, session_id: &str) -> String {
    format!("{handler_scope}/{session_id}")
}

pub fn session_dir_of(home_dir: &str, handler_scope: &str, session_id: &str) -> String {
    join(home_dir, &[&session_scope_of(handler_scope, session_id)])
}

pub fn agent_scope_of(session_scope: &str, agent_id: &str) -> String {
    format!("{session_scope}/agents/{agent_id}")
}

pub fn legacy_session_meta_scope_of(session_scope: &str) -> String {
    format!("{session_scope}/session-meta")
}
