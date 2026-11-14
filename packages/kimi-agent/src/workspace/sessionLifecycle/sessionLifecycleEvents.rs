use std::ops::Deref;

use serde_json::Value;

use crate::app::event::event2::{Event2, Event2Class};

macro_rules! session_event {
    ($name:ident, $payload:ident, $type_const:ident, $type:literal, $( $field:ident : $ty:ty ),* $(,)?) => {
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        pub struct $payload {
            $( pub $field: $ty, )*
        }

        pub const $type_const: &str = $type;

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type_const;

            pub fn new(payload: $payload) -> Self {
                let class = Event2Class {
                    type_: $type_const.to_string(),
                    durable: true,
                    observable: true,
                    agent_domain: false,
                    schema: None,
                };
                let event = Event2::new(
                    &class,
                    serde_json::to_value(&payload).unwrap(),
                    None,
                );
                Self { event, payload }
            }
        }

        impl Deref for $name {
            type Target = Event2;

            fn deref(&self) -> &Self::Target {
                &self.event
            }
        }
    };
}

session_event!(SessionArchived, SessionArchivedPayload, SESSION_ARCHIVED_TYPE, "event.session.archived",
    session_id: String, workspace_id: String);
session_event!(SessionRestored, SessionRestoredPayload, SESSION_RESTORED_TYPE, "event.session.restored",
    session_id: String, workspace_id: String);
session_event!(SessionDeleted, SessionDeletedPayload, SESSION_DELETED_TYPE, "event.session.deleted",
    session_id: String, workspace_id: String);
session_event!(SessionCreated, SessionCreatedPayload, SESSION_CREATED_TYPE, "event.session.created",
    agent_id: String, session_id: String, session: Value);
