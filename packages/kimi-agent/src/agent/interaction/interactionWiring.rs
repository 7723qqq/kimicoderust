use std::sync::Arc;

use serde_json::Value;

use crate::human::interaction::facade::interactions;
use crate::human::interaction::interaction::{
    INTERACTION_TAG_AGENT_ID, INTERACTION_TAG_SESSION_ID, INTERACTION_TAG_TOOL_CALL_ID,
    InteractionCancellation, InteractionCancellationReason, InteractionQuery, InteractionTagValue,
    InteractionTags,
};
use crate::human::interaction::machine::InteractionEmitted;
use crate::state::eventDispatcher::IEventDispatcher;

use super::interaction_ops::{
    InteractionRequestEvent, InteractionRequestEventPayload, InteractionResolvedEvent,
    InteractionResolvedEventPayload, register_interaction_event_classes,
};

pub fn attach_interaction_agent(
    agent_id: &str,
    session_id: &str,
    dispatcher: Arc<dyn IEventDispatcher>,
) {
    let _ = register_interaction_event_classes();
    let agent_id = agent_id.to_string();
    interactions().attach_agent(
        &agent_id,
        session_id,
        Arc::new({
            let agent_id = agent_id.clone();
            move |emitted| dispatch_interaction_event(&agent_id, emitted, dispatcher.clone())
        }),
    );
}

pub fn detach_interaction_agent(agent_id: &str, session_id: &str) {
    let interactions = interactions();
    interactions.detach_agent(agent_id, session_id);
    let pending = interactions.find_all(InteractionQuery {
        resolved: Some(false),
        tags: Some(tags_with_agent(agent_id, session_id)),
        ..Default::default()
    });
    for interaction in pending {
        interactions.respond(
            &interaction.id,
            serde_json::to_value(InteractionCancellation {
                cancelled: true,
                reason: InteractionCancellationReason::AgentClosed,
            })
            .unwrap(),
        );
    }
}

pub fn cancel_interactions_for_turn(agent_id: &str, session_id: &str, turn_id: u64) {
    let interactions = interactions();
    let mut tags = tags_with_agent(agent_id, session_id);
    tags.insert(
        "turnId".to_string(),
        InteractionTagValue::Number(turn_id as f64),
    );
    for interaction in interactions.find_all(InteractionQuery {
        resolved: Some(false),
        tags: Some(tags),
        ..Default::default()
    }) {
        interactions.respond(
            &interaction.id,
            serde_json::to_value(InteractionCancellation {
                cancelled: true,
                reason: InteractionCancellationReason::TurnEnded,
            })
            .unwrap(),
        );
    }
}

fn dispatch_interaction_event(
    agent_id: &str,
    emitted: InteractionEmitted,
    dispatcher: Arc<dyn IEventDispatcher>,
) {
    let event = match emitted {
        InteractionEmitted::Requested { record } => {
            let tool_call_id = read_string_tag(&record.interaction.tags, INTERACTION_TAG_TOOL_CALL_ID)
                .or_else(|| read_payload_tool_call_id(&record.interaction.payload));
            InteractionRequestEvent::new(InteractionRequestEventPayload {
                agent_id: agent_id.to_string(),
                id: record.interaction.id,
                kind: record.interaction.kind,
                tool_call_id,
                request: record.interaction.payload,
            })
            .into()
        }
        InteractionEmitted::Resolved { id, response, .. } => {
            InteractionResolvedEvent::new(InteractionResolvedEventPayload {
                agent_id: agent_id.to_string(),
                id,
                response,
            })
            .into()
        }
    };
    tokio::spawn(async move {
        let _ = dispatcher.dispatch(event).await;
    });
}

fn tags_with_agent(agent_id: &str, session_id: &str) -> InteractionTags {
    InteractionTags::from([
        (
            INTERACTION_TAG_AGENT_ID.to_string(),
            InteractionTagValue::String(agent_id.to_string()),
        ),
        (
            INTERACTION_TAG_SESSION_ID.to_string(),
            InteractionTagValue::String(session_id.to_string()),
        ),
    ])
}

fn read_string_tag(tags: &InteractionTags, key: &str) -> Option<String> {
    match tags.get(key) {
        Some(InteractionTagValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn read_payload_tool_call_id(payload: &Value) -> Option<String> {
    payload
        .get("toolCallId")
        .and_then(Value::as_str)
        .map(str::to_string)
}
