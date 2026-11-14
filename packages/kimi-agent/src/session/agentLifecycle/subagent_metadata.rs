use std::collections::HashMap;

use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};

use crate::i18n::{LocalizedText, i18n_params};
use crate::session::session_metadata::session_metadata::{AgentMeta, AgentType};
use crate::wire::record::WireAsyncError;

use super::errors::SUBAGENT_DEPTH_EXCEEDED;

pub const MAX_SUBAGENT_DEPTH: i64 = 8;
pub const SUBAGENT_DEPTH_LABEL: &str = "subagentDepth";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentLabelOptions {
    pub swarm_item: Option<String>,
    pub depth: Option<i64>,
}

pub fn subagent_labels(
    parent_agent_id: &str,
    options: SubagentLabelOptions,
) -> HashMap<String, String> {
    let mut labels = HashMap::from([(
        "parentAgentId".to_string(),
        parent_agent_id.to_string(),
    )]);
    if let Some(swarm_item) = options.swarm_item {
        labels.insert("swarmItem".to_string(), swarm_item.to_string());
    }
    if let Some(depth) = options.depth {
        labels.insert(SUBAGENT_DEPTH_LABEL.to_string(), depth.to_string());
    }
    labels
}

pub fn with_subagent_profile(
    labels: Option<HashMap<String, String>>,
    profile_name: Option<String>,
) -> Option<HashMap<String, String>> {
    let Some(profile_name) = profile_name.filter(|profile_name| !profile_name.is_empty()) else {
        return labels;
    };
    let mut labels = labels.unwrap_or_default();
    labels.insert("profileName".to_string(), profile_name);
    Some(labels)
}

pub fn labels_from_agent_meta(meta: &AgentMeta) -> Option<HashMap<String, String>> {
    let mut labels = meta.labels.clone().unwrap_or_default();
    if let Some(parent_agent_id) = subagent_parent_agent_id(Some(meta)) {
        labels.insert("parentAgentId".to_string(), parent_agent_id);
    }
    if let Some(swarm_item) = subagent_swarm_item(Some(meta)) {
        labels.insert("swarmItem".to_string(), swarm_item);
    }
    (!labels.is_empty()).then_some(labels)
}

pub fn is_subagent_meta(meta: Option<&AgentMeta>) -> bool {
    let Some(meta) = meta else {
        return false;
    };
    subagent_parent_agent_id(Some(meta)).is_some() || meta.type_ == Some(AgentType::Sub)
}

pub fn subagent_parent_agent_id(meta: Option<&AgentMeta>) -> Option<String> {
    let meta = meta?;
    first_non_empty(&[
        meta.labels
            .as_ref()
            .and_then(|labels| labels.get("parentAgentId").cloned()),
        meta.parent_agent_id.clone().flatten(),
    ])
}

pub fn subagent_swarm_item(meta: Option<&AgentMeta>) -> Option<String> {
    let meta = meta?;
    first_non_empty(&[
        meta.labels
            .as_ref()
            .and_then(|labels| labels.get("swarmItem").cloned()),
        meta.swarm_item.clone(),
    ])
}

pub fn subagent_depth_of(meta: Option<&AgentMeta>) -> i64 {
    let Some(raw) = meta
        .and_then(|meta| meta.labels.as_ref())
        .and_then(|labels| labels.get(SUBAGENT_DEPTH_LABEL))
    else {
        return 0;
    };
    parse_js_integer(raw).filter(|depth| *depth >= 0).unwrap_or(0)
}

pub fn assert_subagent_depth_allowed(meta: Option<&AgentMeta>) -> Result<i64, WireAsyncError> {
    let caller_depth = subagent_depth_of(meta);
    if caller_depth >= MAX_SUBAGENT_DEPTH {
        let mut details = Map::new();
        details.insert("depth".to_string(), Value::from(caller_depth));
        details.insert("maxDepth".to_string(), Value::from(MAX_SUBAGENT_DEPTH));
        return Err(Box::new(Error2::with_options(
            SUBAGENT_DEPTH_EXCEEDED,
            LocalizedText::with_params(
                "v2Errors.subagentDepthExceeded",
                i18n_params!["maxDepth" => MAX_SUBAGENT_DEPTH],
            )
            .render(),
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    }
    Ok(caller_depth + 1)
}

pub fn subagent_profile_name(meta: Option<&AgentMeta>) -> Option<String> {
    first_non_empty(&[meta
        .and_then(|meta| meta.labels.as_ref())
        .and_then(|labels| labels.get("profileName").cloned())])
}

fn first_non_empty(values: &[Option<String>]) -> Option<String> {
    values.iter().flatten().find(|value| !value.is_empty()).cloned()
}

fn parse_js_integer(raw: &str) -> Option<i64> {
    let raw = raw.trim_start();
    let sign_len = usize::from(raw.starts_with('+') || raw.starts_with('-'));
    let digits = &raw[sign_len..];
    let digit_count = digits
        .char_indices()
        .take_while(|(_, character)| character.is_ascii_digit())
        .last()
        .map(|(index, character)| index + character.len_utf8())
        .unwrap_or(0);
    if digit_count == 0 {
        return None;
    }
    let parsed = raw[..sign_len + digit_count].parse::<i128>().ok()?;
    if parsed > i64::MAX as i128 || parsed < i64::MIN as i128 {
        return None;
    }
    let parsed = parsed as i64;
    (parsed.unsigned_abs() <= 9_007_199_254_740_991).then_some(parsed)
}
