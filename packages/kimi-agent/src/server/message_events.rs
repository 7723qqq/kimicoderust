use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::callbacks::HostCallbacks;
use crate::rpc::types::{
    AskQuestionRequest, AskQuestionResponse, CheckpointRequest, ListToolsResponse, LlmChatRequest,
    LlmChatResponse, PermissionCheckRequest, PermissionDecision, StateReadRequest,
    StateReadResponse, StateWriteRequest, StateWriteResponse, ToolExecuteRequest,
    ToolExecuteResponse,
};
use crate::server::hub::EventHub;
use crate::turn_loop::types::{GoalContext, LLMMessage};

struct AssistantMessage {
    id: String,

    content_index: usize,
}

struct MessageState {
    step: u32,
    current: Option<AssistantMessage>,

    prompt_announced: bool,
}

pub struct MessageCallbacks {
    pub inner: Arc<dyn HostCallbacks>,
    pub session_id: String,
    pub hub: Arc<EventHub>,
    pub turn_number: u32,
    pub prompt: String,

    pub blocks: Vec<crate::rpc::types::ContentPart>,

    pub origin: Option<Value>,
    state: Mutex<MessageState>,

    step_tracker: Option<Box<dyn Fn(u32) + Send + Sync>>,
}

impl MessageCallbacks {
    pub fn new(
        inner: Arc<dyn HostCallbacks>,
        session_id: &str,
        hub: Arc<EventHub>,
        turn_number: u32,
        prompt: &str,
    ) -> Self {
        Self::with_step_tracker(inner, session_id, hub, turn_number, prompt, &[], None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_step_tracker(
        inner: Arc<dyn HostCallbacks>,
        session_id: &str,
        hub: Arc<EventHub>,
        turn_number: u32,
        prompt: &str,
        blocks: &[crate::rpc::types::ContentPart],
        origin: Option<Value>,
        step_tracker: Option<Box<dyn Fn(u32) + Send + Sync>>,
    ) -> Self {
        Self {
            inner,
            session_id: session_id.to_string(),
            hub,
            turn_number,
            prompt: prompt.to_string(),
            blocks: blocks.to_vec(),
            origin,
            state: Mutex::new(MessageState {
                step: 0,
                current: None,
                prompt_announced: false,
            }),
            step_tracker,
        }
    }

    pub fn announce_prompt(&self) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.prompt_announced {
                return;
            }
            state.prompt_announced = true;
        }
        let mut content = vec![json!({ "type": "text", "text": self.prompt })];
        content.extend(self.blocks.iter().filter_map(media_content_part));
        let mut message = json!({
            "id": format!("msg-u{}", self.turn_number),
            "session_id": self.session_id,
            "role": "user",
            "content": content,
            "created_at": chrono::Utc::now().to_rfc3339(),
        });

        if let Some(origin) = &self.origin {
            message["origin"] = origin.clone();
        }
        self.hub
            .bus_for(&self.session_id)
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.message.created",
                "message": message,
            })));
    }

    fn on_delta(&self, part: &Value) {
        let Some(part_kind) = part.get("type").and_then(|v| v.as_str()) else {
            return;
        };

        if crate::events::is_hidden_delta_part(part) {
            return;
        }
        let Some(chunk) = (match part_kind {
            "text" => part.get("text").and_then(|v| v.as_str()),
            "think" => part.get("think").and_then(|v| v.as_str()),
            _ => None,
        }) else {
            return;
        };

        let event_name = if part_kind == "think" {
            "event.thinking.delta"
        } else {
            "event.assistant.delta"
        };
        let (message_id, index) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.current.is_none() {
                let id = format!("msg-a{}-{}", self.turn_number, state.step);
                state.current = Some(AssistantMessage {
                    id: id.clone(),
                    content_index: 0,
                });
                self.hub
                    .bus_for(&self.session_id)
                    .publish(&crate::events::EngineEvent::Custom(json!({
                        "type": "event.message.created",
                        "message": {
                            "id": id,
                            "session_id": self.session_id,
                            "role": "assistant",
                            "content": [],
                            "created_at": chrono::Utc::now().to_rfc3339(),
                        },
                    })));
            }
            let current = state.current.as_mut().unwrap();
            let index = current.content_index;
            current.content_index += 1;
            (current.id.clone(), index)
        };
        self.hub
            .bus_for(&self.session_id)
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": event_name,
                "turn_id": self.turn_number,
                "message_id": message_id,
                "content_index": index,
                "delta": chunk,
            })));
    }

    fn on_step_end(&self, event: &Value) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(current) = state.current.take() else {
            return;
        };
        let mut content: Vec<Value> = Vec::new();
        if let Some(text) = event
            .get("content")
            .and_then(|v| v.as_str())
            .filter(|text| !text.is_empty())
        {
            content.push(json!({ "type": "text", "text": text }));
        }
        if let Some(tool_calls) = event.get("tool_calls").and_then(|v| v.as_array()) {
            for call in tool_calls {
                content.push(json!({
                    "type": "tool_use",
                    "tool_call_id": call.get("id").cloned().unwrap_or(Value::Null),
                    "tool_name": call.get("name").cloned().unwrap_or(Value::Null),
                    "input": call.get("arguments").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        self.hub
            .bus_for(&self.session_id)
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.message.updated",
                "message_id": current.id,
                "content": content,
                "status": "completed",
            })));
    }
}

impl HostCallbacks for MessageCallbacks {
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<AskQuestionResponse, String>> {
        self.inner.ask_question(request)
    }

    fn cancel_question(&self, question_id: &str, reason: &str) {
        self.inner.cancel_question(question_id, reason);
    }

    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<StateReadResponse, String>> {
        self.inner.state_read(request)
    }

    fn state_write(
        &self,
        request: StateWriteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<StateWriteResponse, String>> {
        self.inner.state_write(request)
    }

    fn checkpoint(
        &self,
        request: CheckpointRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint(request)
    }

    fn checkpoint_state(&self) -> crate::rpc::types::BoxFuture<'static, Result<(), String>> {
        self.inner.checkpoint_state()
    }

    fn list_tools(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }

    fn goal(&self) -> crate::rpc::types::BoxFuture<'static, Result<Option<GoalContext>, String>> {
        self.inner.goal()
    }

    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.inner.parse_bash(request)
    }

    fn auth_token(
        &self,
        provider: String,
        force: bool,
    ) -> crate::rpc::types::BoxFuture<'static, Result<String, String>> {
        self.inner.auth_token(provider, force)
    }

    fn drain_steers(
        &self,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Vec<LLMMessage>, String>> {
        self.inner.drain_steers()
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.inner.set_turn_goal(turn_id, goal_id);
    }

    fn emit_event(&self, event: Value) {
        match event.get("type").and_then(|v| v.as_str()) {
            Some("llm.step.begin") => {
                let step = {
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    state.step += 1;
                    state.step
                };
                if let Some(tracker) = &self.step_tracker {
                    tracker(step);
                }
            }
            Some("llm.delta") => {
                if let Some(part) = event.get("part") {
                    self.on_delta(part);
                }
            }
            Some("llm.step.end") => self.on_step_end(&event),
            _ => {}
        }
        self.inner.emit_event(event);
    }

    fn turn_event(&self, event: crate::turn_events::TurnEvent) {
        self.inner.turn_event(event);
    }

    fn telemetry(&self, event: Value) {
        self.inner.telemetry(event);
    }
}

fn media_content_part(block: &crate::rpc::types::ContentPart) -> Option<Value> {
    use crate::rpc::types::ContentPart;
    let (part_type, source) = match block {
        ContentPart::ImageUrl { image_url } => (
            "image",
            image_source(&image_url.url, image_url.id.as_deref()),
        ),
        ContentPart::VideoUrl { video_url } => (
            "video",
            url_source(&video_url.url, video_url.id.as_deref()),
        ),
        ContentPart::AudioUrl { audio_url } => (
            "audio",
            url_source(&audio_url.url, audio_url.id.as_deref()),
        ),
        ContentPart::Text { .. }
        | ContentPart::Think { .. }
        | ContentPart::DocumentUrl { .. } => return None,
    };
    Some(json!({ "type": part_type, "source": source }))
}

fn url_source(url: &str, id: Option<&str>) -> Value {
    if let Some(file_id) = crate::llm::media_resolver::parse_daemon_file_url(url) {
        return json!({ "kind": "session_media", "file_id": file_id });
    }
    let mut source = json!({ "kind": "url", "url": url });
    if let Some(id) = id {
        source["id"] = json!(id);
    }
    source
}

fn image_source(url: &str, id: Option<&str>) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
    {
        return json!({ "kind": "base64", "media_type": media_type, "data": data });
    }
    url_source(url, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn inner() -> Arc<dyn HostCallbacks> {
        struct Drop;
        impl HostCallbacks for Drop {
            fn llm_chat(
                &self,
                _: LlmChatRequest,
            ) -> crate::rpc::types::BoxFuture<'static, Result<LlmChatResponse, String>>
            {
                Box::pin(async { Err("unused".into()) })
            }
            fn execute_tool(
                &self,
                _: ToolExecuteRequest,
            ) -> crate::rpc::types::BoxFuture<'static, Result<ToolExecuteResponse, String>>
            {
                Box::pin(async { Err("unused".into()) })
            }
            fn check_permission(
                &self,
                _: PermissionCheckRequest,
            ) -> crate::rpc::types::BoxFuture<'static, Result<PermissionDecision, String>>
            {
                Box::pin(async { Err("unused".into()) })
            }
        }
        Arc::new(Drop)
    }

    async fn next_event(sub: &mut crate::server::hub::WsSubscription) -> Value {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("event within 5s")
            .expect("hub open");
        serde_json::to_value(&event.event).unwrap()
    }

    #[tokio::test]
    async fn the_prompt_announcement_carries_the_turn_origin() {
        let hub = Arc::new(EventHub::new());
        let mut sub = hub.attach();
        let origin = json!({
            "kind": "skill_activation",
            "trigger": "user-slash",
            "skillName": "review",
            "skillArgs": "src/main.rs",
        });
        let callbacks = MessageCallbacks::with_step_tracker(
            inner(),
            "sess-origin",
            hub.clone(),
            2,
            "/review src/main.rs",
            &[],
            Some(origin.clone()),
            None,
        );

        callbacks.announce_prompt();
        let user = next_event(&mut sub).await;
        assert_eq!(user["message"]["origin"], origin);
    }

    #[tokio::test]
    async fn the_prompt_announcement_carries_its_media_blocks() {
        let hub = Arc::new(EventHub::new());
        let mut sub = hub.attach();
        let blocks = vec![
            crate::llm::media_resolver::file_media_part(
                "f_att1".into(),
                crate::rpc::types::MediaKind::Image,
            ),
            crate::llm::media_resolver::media_url_part(
                crate::rpc::types::MediaKind::Image,
                "https://example.test/remote.png".into(),
                Some("ms-9".into()),
                None,
            ),
            crate::llm::media_resolver::media_url_part(
                crate::rpc::types::MediaKind::Image,
                "https://example.test/idless.png".into(),
                None,
                None,
            ),
        ];
        let callbacks = MessageCallbacks::with_step_tracker(
            inner(),
            "sess-media",
            hub.clone(),
            2,
            "look at this",
            &blocks,
            None,
            None,
        );

        callbacks.announce_prompt();
        let user = next_event(&mut sub).await;
        assert_eq!(user["message"]["id"], "msg-u2");
        assert!(
            user["message"].get("origin").is_none(),
            "a turn without an origin announces none"
        );
        assert_eq!(
            user["message"]["content"],
            json!([
                { "type": "text", "text": "look at this" },
                { "type": "image", "source": { "kind": "session_media", "file_id": "f_att1" } },
                { "type": "image", "source": { "kind": "url", "url": "https://example.test/remote.png", "id": "ms-9" } },
                { "type": "image", "source": { "kind": "url", "url": "https://example.test/idless.png" } },
            ]),
        );
    }

    #[tokio::test]
    async fn the_turn_publishes_user_and_step_messages() {
        let hub = Arc::new(EventHub::new());
        let mut sub = hub.attach();
        let callbacks = MessageCallbacks::new(inner(), "sess-msg", hub.clone(), 4, "draw a cat");

        callbacks.announce_prompt();
        let user = next_event(&mut sub).await;
        assert_eq!(user["type"], "event.message.created");
        assert_eq!(user["message"]["id"], "msg-u4");
        assert_eq!(user["message"]["role"], "user");
        assert_eq!(user["message"]["content"][0]["text"], "draw a cat");

        callbacks.announce_prompt();

        callbacks.emit_event(json!({ "type": "llm.step.begin", "model": "m" }));
        callbacks.emit_event(json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": "he" },
        }));
        let created = next_event(&mut sub).await;
        assert_eq!(created["type"], "event.message.created");
        assert_eq!(created["message"]["id"], "msg-a4-1");
        assert_eq!(created["message"]["role"], "assistant");
        assert_eq!(created["message"]["content"], json!([]));

        let delta1 = next_event(&mut sub).await;
        assert_eq!(delta1["type"], "event.assistant.delta");
        assert_eq!(delta1["content_index"], 0);
        assert_eq!(delta1["delta"], "he");

        callbacks.emit_event(json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": "llo" },
        }));
        let delta = next_event(&mut sub).await;
        assert_eq!(delta["type"], "event.assistant.delta");
        assert_eq!(delta["message_id"], "msg-a4-1");
        assert_eq!(delta["content_index"], 1);
        assert_eq!(delta["delta"], "llo");

        callbacks.emit_event(json!({
            "type": "llm.step.end",
            "content": "hello",
            "tool_calls": [
                { "id": "c1", "name": "Read", "arguments": { "path": "a" } }
            ],
        }));
        let updated = next_event(&mut sub).await;
        assert_eq!(updated["type"], "event.message.updated");
        assert_eq!(updated["message_id"], "msg-a4-1");
        assert_eq!(updated["status"], "completed");
        assert_eq!(updated["content"][0]["type"], "text");
        assert_eq!(updated["content"][0]["text"], "hello");
        assert_eq!(updated["content"][1]["type"], "tool_use");
        assert_eq!(updated["content"][1]["tool_call_id"], "c1");
        assert_eq!(updated["content"][1]["input"]["path"], "a");

        callbacks.emit_event(json!({ "type": "llm.step.begin", "model": "m" }));
        callbacks.emit_event(json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": "next" },
        }));
        let created2 = next_event(&mut sub).await;
        assert_eq!(created2["message"]["id"], "msg-a4-2");
    }

    #[tokio::test]
    async fn thinking_deltas_stream_and_empty_step_ends_are_ignored() {
        let hub = Arc::new(EventHub::new());
        let mut sub = hub.attach();
        let callbacks = MessageCallbacks::new(inner(), "sess-think", hub.clone(), 1, "hi");

        callbacks.announce_prompt();
        let _user = next_event(&mut sub).await;

        callbacks.emit_event(json!({ "type": "llm.step.begin", "model": "m" }));
        callbacks.emit_event(json!({ "type": "llm.step.end", "content": "", "tool_calls": [] }));

        callbacks.emit_event(json!({
            "type": "llm.delta",
            "part": { "type": "think", "think": "reasoning..." },
        }));
        let created = next_event(&mut sub).await;
        assert_eq!(created["message"]["id"], "msg-a1-1");
        let delta = next_event(&mut sub).await;
        assert_eq!(delta["type"], "event.thinking.delta");
        assert_eq!(delta["delta"], "reasoning...");

        assert_eq!(delta["turn_id"], 1, "the delta must name its turn");
    }

    #[tokio::test]
    async fn text_deltas_ride_the_assistant_event() {
        let hub = Arc::new(EventHub::new());
        let mut sub = hub.attach();
        let callbacks = MessageCallbacks::new(inner(), "sess-text", hub.clone(), 2, "hi");
        callbacks.announce_prompt();
        let _user = next_event(&mut sub).await;

        callbacks.emit_event(json!({ "type": "llm.step.begin", "model": "m" }));
        callbacks.emit_event(json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": "answer" },
        }));
        let _created = next_event(&mut sub).await;
        let delta = next_event(&mut sub).await;
        assert_eq!(delta["type"], "event.assistant.delta");
        assert_eq!(delta["delta"], "answer");
        assert_eq!(delta["turn_id"], 2);
    }
}
