//! Tower inbox wake mapping, batching, rendering, and delegated replay.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::session::wake::{
    TowerWakeDriver, WakeBuffer, WakeGateDigest, WakeItem, WakePayload, WakeReplayHandler,
};
use crate::tools::tower::status_reader::{
    TowerStatusGate, TowerWakeGateDigest, read_tower_wake_status_gates,
};
use crate::tools::tower::store::TowerStore;
use crate::tools::tower::types::TowerInboxDelivery;
use crate::turn_loop::types::LLMMessage;

pub const TOWER_INBOX_WAKE_VARIANT: &str = "tower_inbox";
const WAKE_SUBJECT_PREVIEW_MAX: usize = 120;
const WAKE_DIGEST_LIMIT: usize = 4;

/// Map the inbox publication metadata into the generic wake reference. Legacy
/// records without a message id fall back to their stable file identity; an
/// event with neither lets `WakeBuffer` allocate its legacy sequence key.
pub fn tower_wake_item(delivery: &TowerInboxDelivery) -> WakeItem {
    let key = if delivery.message_id.is_empty() {
        (!delivery.file.is_empty()).then(|| format!("file:{}", delivery.file))
    } else {
        Some(format!("id:{}", delivery.message_id))
    }
    .unwrap_or_default();
    WakeItem {
        key,
        source: delivery.from.clone(),
        subject: delivery.subject.clone(),
        message_id: (!delivery.message_id.is_empty()).then(|| delivery.message_id.clone()),
        file: (!delivery.file.is_empty()).then(|| delivery.file.clone()),
        sent_at: (!delivery.sent_at.is_empty()).then(|| delivery.sent_at.clone()),
        mission_id: delivery.mission_id.clone(),
    }
}

struct WakeChannel {
    buffer: WakeBuffer,
    scheduled: bool,
    replay_failed: bool,
    active: bool,
}

type SharedWakeChannel = Arc<tokio::sync::Mutex<WakeChannel>>;

impl WakeChannel {
    fn new(variant: &str) -> Self {
        Self {
            buffer: WakeBuffer::new(variant),
            scheduled: false,
            replay_failed: false,
            active: true,
        }
    }
}

/// Shared owner of the driver and all per-session wake buffers. A server-built
/// toolset is short-lived, so this runtime — not the toolset itself — owns the
/// batching and replay state.
pub struct TowerWakeRuntime {
    driver: Arc<dyn TowerWakeDriver>,
    channels: Arc<Mutex<HashMap<(String, String), SharedWakeChannel>>>,
}

impl TowerWakeRuntime {
    #[must_use]
    pub fn new(driver: Arc<dyn TowerWakeDriver>) -> Arc<Self> {
        Arc::new(Self {
            driver,
            channels: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn channel_for_enqueue(&self, session_id: &str) -> SharedWakeChannel {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry((session_id.to_string(), TOWER_INBOX_WAKE_VARIANT.to_string()))
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(WakeChannel::new(
                    TOWER_INBOX_WAKE_VARIANT,
                )))
            })
            .clone()
    }

    /// Queue one inbox reference and schedule a single flush for its channel.
    /// The actual gate read and rendering happen after the producer yields, so
    /// a burst published in one task coalesces into one payload.
    pub async fn enqueue(
        self: &Arc<Self>,
        repo_root: &Path,
        task_runner: Option<&Arc<crate::storage::TaskRunner>>,
        session_id: &str,
        delivery: &TowerInboxDelivery,
    ) {
        let channel = self.channel_for_enqueue(session_id);
        let mut guard = channel.lock().await;
        if !guard.active
            || !guard.buffer.push(tower_wake_item(delivery))
            || guard.scheduled
            || guard.replay_failed
        {
            return;
        }
        guard.scheduled = true;
        drop(guard);

        let runtime = self.clone();
        let repo_root = repo_root.to_path_buf();
        let task_runner = task_runner.cloned();
        let session_id = session_id.to_string();
        tokio::spawn(async move {
            runtime
                .flush_initial(channel, &repo_root, task_runner, &session_id)
                .await;
        });
    }

    async fn flush_initial(
        &self,
        channel: SharedWakeChannel,
        repo_root: &Path,
        task_runner: Option<Arc<crate::storage::TaskRunner>>,
        session_id: &str,
    ) {
        let mut guard = channel.lock().await;
        if !guard.active || guard.replay_failed || guard.buffer.items().is_empty() {
            guard.scheduled = false;
            return;
        }
        let active_agent_ids = super::active_tower_agent_ids(task_runner.as_deref());
        let store = TowerStore::new(repo_root);
        let rendered = render_wake(
            &store,
            guard.buffer.items(),
            guard.buffer.omitted(),
            active_agent_ids.as_ref(),
            false,
        )
        .await;
        let Ok((message, gate_digests)) = rendered else {
            guard.scheduled = false;
            return;
        };
        let Some(payload) = guard.buffer.take(message, gate_digests) else {
            guard.scheduled = false;
            return;
        };
        if self.driver.enqueue(session_id, payload.clone()).is_err() {
            // An initial enqueue has no engine-retained payload. Keep the batch
            // queued so a later publication can retry it without losing work.
            guard.buffer.requeue(payload);
        }
        guard.scheduled = false;
    }

    /// Build the session/server-registered replay owner for one session. The
    /// captured session id is required by the standalone driver; the native
    /// driver deliberately ignores it because it is already session-bound.
    #[must_use]
    pub fn replay_handler(
        self: &Arc<Self>,
        session_id: &str,
        repo_root: &Path,
        task_runner: Option<&Arc<crate::storage::TaskRunner>>,
    ) -> WakeReplayHandler {
        let runtime = self.clone();
        let session_id = session_id.to_string();
        let repo_root = repo_root.to_path_buf();
        let task_runner = task_runner.cloned();
        Arc::new(move |payload| {
            let runtime = runtime.clone();
            let session_id = session_id.clone();
            let repo_root = repo_root.clone();
            let task_runner = task_runner.clone();
            Box::pin(async move {
                runtime
                    .replay(&session_id, &repo_root, task_runner, payload)
                    .await
            })
        })
    }

    async fn replay(
        &self,
        session_id: &str,
        repo_root: &Path,
        task_runner: Option<Arc<crate::storage::TaskRunner>>,
        payload: WakePayload,
    ) -> Result<(), String> {
        let channel = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(session_id.to_string(), TOWER_INBOX_WAKE_VARIANT.to_string()))
            .cloned();
        let Some(channel) = channel else {
            return Ok(());
        };
        let mut guard = channel.lock().await;
        if !guard.active {
            return Ok(());
        }
        guard.replay_failed = false;
        guard.buffer.requeue(payload);

        let result = async {
            let active_agent_ids = super::active_tower_agent_ids(task_runner.as_deref());
            let store = TowerStore::new(repo_root);
            // Delegated replay is stricter than the initial notification: a
            // failed reread leaves the engine's original payload pending.
            let (message, gate_digests) = render_wake(
                &store,
                guard.buffer.items(),
                guard.buffer.omitted(),
                active_agent_ids.as_ref(),
                true,
            )
            .await?;
            let Some(payload) = guard.buffer.take(message, gate_digests) else {
                return Ok(());
            };
            if let Err(error) = self.driver.enqueue(session_id, payload.clone()) {
                // Do not retry here. The engine retains the original payload;
                // requeue only restores this buffer for its next delegation.
                guard.buffer.requeue(payload);
                return Err(error);
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            // A scheduled initial flush must not become an accidental second
            // attempt after a delegated replay failed.
            guard.replay_failed = true;
        }
        result
    }

    /// Clear both the producer state and every wake already admitted by the
    /// engine/server. No turn or wake id is retained across replay.
    pub async fn clear(&self, session_id: &str, variant: &str) -> bool {
        let key = (session_id.to_string(), variant.to_string());
        let channel = self
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(channel) = channel {
            let mut guard = channel.lock().await;
            guard.active = false;
            guard.scheduled = false;
            guard.buffer.clear();
        }
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
        self.driver.drop_wakes(session_id, variant)
    }
}

fn mission_ids(items: &[WakeItem]) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter_map(|item| item.mission_id.clone())
        .filter(|mission_id| seen.insert(mission_id.clone()))
        .collect()
}

fn unavailable_gate(mission_id: &str, status: &str) -> TowerStatusGate {
    TowerStatusGate {
        mission_id: mission_id.to_string(),
        branch: String::new(),
        status: status.to_string(),
        ready: false,
        noop: false,
        reasons: Vec::new(),
        blocks: Vec::new(),
        selected_review: None,
        binding: None,
    }
}

async fn render_wake(
    store: &TowerStore,
    items: &[WakeItem],
    omitted: usize,
    active_agent_ids: Option<&HashSet<String>>,
    require_state: bool,
) -> Result<(LLMMessage, Vec<WakeGateDigest>), String> {
    let state = match store.load().await {
        Ok(state) => Some(state),
        Err(_) if !require_state => None,
        Err(error) => return Err(error),
    };
    let recovered_at = match state.as_ref() {
        Some(_) => match store.recovered_at().await {
            Ok(recovered_at) => recovered_at,
            Err(_) if !require_state => None,
            Err(error) => return Err(error),
        },
        None => None,
    };
    let mission_ids = mission_ids(items);
    let selected = &mission_ids[..mission_ids.len().min(WAKE_DIGEST_LIMIT)];
    let gates = match state.as_ref() {
        Some(state) => read_tower_wake_status_gates(store, state, selected, active_agent_ids).await,
        None => selected
            .iter()
            .map(|mission_id| unavailable_gate(mission_id, "unavailable"))
            .collect(),
    };
    let gate_digests = gates
        .into_iter()
        .map(|gate| {
            let digest = TowerWakeGateDigest::from(gate);
            WakeGateDigest {
                mission_id: digest.mission_id,
                status: digest.status,
                reasons: digest.reasons,
                binding: digest.binding,
            }
        })
        .collect::<Vec<_>>();
    let message = render_wake_message(
        items,
        omitted,
        recovered_at.as_deref(),
        &gate_digests,
        mission_ids.len(),
    );
    Ok((message, gate_digests))
}

fn render_wake_message(
    items: &[WakeItem],
    omitted: usize,
    recovered_at: Option<&str>,
    gate_digests: &[WakeGateDigest],
    mission_count: usize,
) -> LLMMessage {
    let count = items.len() + omitted;
    let mut lines = vec![format!(
        "{} new tower inbox message{} — {} reference(s), omitted={}. Read and route with TowerInbox; this wake does not acknowledge them.",
        count,
        if count == 1 { "" } else { "s" },
        items.len(),
        omitted
    )];
    for item in items {
        let subject = if item.subject.chars().count() > WAKE_SUBJECT_PREVIEW_MAX {
            format!(
                "{}…",
                item.subject
                    .chars()
                    .take(WAKE_SUBJECT_PREVIEW_MAX)
                    .collect::<String>()
            )
        } else {
            item.subject.clone()
        };
        let mut references = Vec::new();
        if let Some(message_id) = item.message_id.as_deref() {
            references.push(format!("messageId={message_id}"));
        }
        if let Some(file) = item.file.as_deref() {
            references.push(format!("file={file}"));
        }
        if let Some(sent_at) = item.sent_at.as_deref() {
            references.push(format!("sentAt={sent_at}"));
        }
        if let Some(mission_id) = item.mission_id.as_deref() {
            references.push(format!("mission={mission_id}"));
        }
        lines.push(format!(
            "- {}: \"{}\"{}",
            item.source,
            subject,
            if references.is_empty() {
                String::new()
            } else {
                format!(" ({})", references.join(", "))
            }
        ));
    }
    if let Some(recovered_at) = recovered_at {
        lines.push(format!(
            "Tower state was recovered after a loss at {recovered_at} — missions, roster, and protocol history before that point are gone. Report the loss to the user instead of continuing silently."
        ));
    }
    if mission_count > 0 {
        lines.push("Gate digest:".into());
        for digest in gate_digests {
            let mut line = format!("- {}: gate={}", digest.mission_id, digest.status);
            match digest.status.as_str() {
                "READY" => {
                    line.push_str(" reasons=none");
                    if let Some(binding) = digest.binding.as_deref() {
                        line.push_str(&format!(" binding={binding}"));
                    }
                }
                "BLOCKED" => {
                    line.push_str(&format!(" reasons={}", digest.reasons.join(",")));
                }
                _ => {}
            }
            lines.push(line);
        }
        if mission_count > gate_digests.len() {
            lines.push(format!(
                "- digest-omitted={}",
                mission_count - gate_digests.len()
            ));
        }
    }
    LLMMessage {
        role: "user".into(),
        content: lines.join("\n"),
        ..LLMMessage::default()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{TOWER_INBOX_WAKE_VARIANT, TowerWakeRuntime, tower_wake_item};
    use crate::session::wake::{TowerWakeDriver, WakeGateDigest, WakePayload};
    use crate::tools::tower::paths::STATE_FILE;
    use crate::tools::tower::store::TowerStore;
    use crate::tools::tower::types::{
        TowerInboxDelivery, TowerMission, TowerMissionStatus, TowerState,
    };
    use crate::turn_loop::types::LLMMessage;

    #[derive(Default)]
    struct RecordingDriver {
        payloads: Mutex<Vec<(String, WakePayload)>>,
        drops: Mutex<Vec<(String, String)>>,
        fail_next_enqueue: AtomicBool,
    }

    impl TowerWakeDriver for RecordingDriver {
        fn enqueue(&self, session_id: &str, payload: WakePayload) -> Result<(), String> {
            self.payloads
                .lock()
                .unwrap()
                .push((session_id.to_string(), payload));
            if self.fail_next_enqueue.swap(false, Ordering::SeqCst) {
                return Err("injected enqueue failure".into());
            }
            Ok(())
        }

        fn drop_wakes(&self, session_id: &str, variant: &str) -> bool {
            self.drops
                .lock()
                .unwrap()
                .push((session_id.to_string(), variant.to_string()));
            true
        }
    }

    fn delivery(id: &str, subject: &str, mission_id: Option<&str>) -> TowerInboxDelivery {
        TowerInboxDelivery {
            from: "worker-a".into(),
            to: "tower".into(),
            subject: subject.into(),
            message_id: id.into(),
            file: format!(".tower/comms/inbox/{id}.md"),
            sent_at: "2026-11-04T00:00:00Z".into(),
            mission_id: mission_id.map(str::to_string),
        }
    }

    fn message(text: &str) -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: text.into(),
            ..LLMMessage::default()
        }
    }

    fn rich_payload(delivery: &TowerInboxDelivery) -> WakePayload {
        let mut payload = WakePayload::new(TOWER_INBOX_WAKE_VARIANT, message("stale wake"));
        payload.items.push(tower_wake_item(delivery));
        payload.gate_digests.push(WakeGateDigest {
            mission_id: delivery.mission_id.clone().unwrap_or_default(),
            status: "READY".into(),
            reasons: Vec::new(),
            binding: Some("stale-binding".into()),
        });
        payload
    }

    async fn save_state(root: &Path, missions: Vec<TowerMission>) {
        let store = TowerStore::new(root);
        tokio::fs::create_dir_all(store.abs(STATE_FILE).parent().unwrap())
            .await
            .unwrap();
        store
            .save(&TowerState {
                version: 1,
                base: "main".into(),
                mode: "branch".into(),
                created_at: "2026-11-04T00:00:00Z".into(),
                session_id: Some("session-main".into()),
                roster: Default::default(),
                missions,
            })
            .await
            .unwrap();
    }

    fn mission(id: &str, status: TowerMissionStatus) -> TowerMission {
        TowerMission {
            id: id.into(),
            title: format!("Mission {id}"),
            slug: id.to_ascii_lowercase(),
            kind: Default::default(),
            scope: vec!["src/**".into()],
            branch: format!("feat/{id}"),
            worktree: format!("wt-{id}"),
            spawn_base: None,
            deps: Vec::new(),
            status,
            owner: None,
            context: None,
            tasks: Vec::new(),
            notes: Vec::new(),
            blockers: Vec::new(),
        }
    }

    async fn wait_for_payloads(driver: &RecordingDriver, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if driver.payloads.lock().unwrap().len() >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for wake payloads");
    }

    async fn prime_channel(
        runtime: &Arc<TowerWakeRuntime>,
        driver: &RecordingDriver,
        root: &Path,
        delivery: &TowerInboxDelivery,
    ) {
        runtime.enqueue(root, None, "session-main", delivery).await;
        wait_for_payloads(driver, 1).await;
        driver.payloads.lock().unwrap().clear();
    }

    #[test]
    fn inbox_delivery_maps_message_id_then_file_to_the_dedup_key() {
        let item = tower_wake_item(&delivery("msg-1", "ready", Some("M1")));
        assert_eq!(item.key, "id:msg-1");
        assert_eq!(item.source, "worker-a");
        assert_eq!(item.subject, "ready");
        assert_eq!(item.message_id.as_deref(), Some("msg-1"));
        assert_eq!(item.file.as_deref(), Some(".tower/comms/inbox/msg-1.md"));
        assert_eq!(item.sent_at.as_deref(), Some("2026-11-04T00:00:00Z"));
        assert_eq!(item.mission_id.as_deref(), Some("M1"));

        let legacy = tower_wake_item(&delivery("", "legacy", None));
        assert_eq!(legacy.key, "file:.tower/comms/inbox/.md");
        assert_eq!(legacy.message_id, None);
    }

    #[tokio::test]
    async fn initial_wake_coalesces_and_renders_bounded_gate_digests() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Arc::new(RecordingDriver::default());
        let runtime = TowerWakeRuntime::new(driver.clone());
        let long_subject = "x".repeat(121);
        for index in 0..5 {
            let subject = if index == 0 {
                long_subject.clone()
            } else {
                format!("subject-{index}")
            };
            runtime
                .enqueue(
                    dir.path(),
                    None,
                    "session-main",
                    &delivery(
                        &format!("msg-{index}"),
                        &subject,
                        Some(&format!("M{index}")),
                    ),
                )
                .await;
        }

        wait_for_payloads(&driver, 1).await;
        let payloads = driver.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1, "one burst produces one wake");
        let (session_id, payload) = &payloads[0];
        assert_eq!(session_id, "session-main");
        assert_eq!(payload.variant, TOWER_INBOX_WAKE_VARIANT);
        assert_eq!(payload.items.len(), 5);
        assert_eq!(payload.gate_digests.len(), 4);
        assert!(
            payload
                .message
                .content
                .contains("5 new tower inbox messages")
        );
        assert!(payload.message.content.contains("omitted=0"));
        assert!(payload.message.content.contains("messageId=msg-0"));
        assert!(payload.message.content.contains("Gate digest:"));
        assert!(payload.message.content.contains("M0: gate=unavailable"));
        assert!(payload.message.content.contains("digest-omitted=1"));
        assert!(
            payload
                .message
                .content
                .contains(&format!("{}…", "x".repeat(120)))
        );
        assert!(!payload.message.content.contains(&long_subject));
    }

    #[tokio::test]
    async fn replay_rereads_gate_and_renders_before_one_reenqueue() {
        let dir = tempfile::tempdir().unwrap();
        save_state(dir.path(), vec![mission("M1", TowerMissionStatus::Merged)]).await;
        let driver = Arc::new(RecordingDriver::default());
        let runtime = TowerWakeRuntime::new(driver.clone());
        let original = rich_payload(&delivery("msg-1", "fresh subject", Some("M1")));
        prime_channel(
            &runtime,
            &driver,
            dir.path(),
            &delivery("msg-1", "fresh subject", Some("M1")),
        )
        .await;
        let handler = runtime.replay_handler("session-main", dir.path(), None);

        handler(original).await.unwrap();

        let payloads = driver.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1);
        let payload = &payloads[0].1;
        assert!(!payload.message.content.contains("stale wake"));
        assert!(payload.message.content.contains("fresh subject"));
        assert!(payload.message.content.contains("M1: gate=merged"));
        assert_eq!(payload.gate_digests.len(), 1);
        assert_eq!(payload.gate_digests[0].status, "merged");
        assert_eq!(payload.gate_digests[0].binding, None);
    }

    #[tokio::test]
    async fn replay_gate_failure_returns_err_without_reenqueueing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let driver = Arc::new(RecordingDriver::default());
        let runtime = TowerWakeRuntime::new(driver.clone());
        let original = rich_payload(&delivery("msg-1", "subject", Some("M1")));
        prime_channel(
            &runtime,
            &driver,
            &missing,
            &delivery("msg-1", "subject", Some("M1")),
        )
        .await;
        let handler = runtime.replay_handler("session-main", &missing, None);

        assert!(handler(original.clone()).await.is_err());
        assert!(driver.payloads.lock().unwrap().is_empty());

        save_state(&missing, Vec::new()).await;
        handler(original).await.unwrap();
        let payloads = driver.payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1, "only the explicit retry enqueues");
        assert_eq!(
            payloads[0].1.items.len(),
            1,
            "requeue deduplicates the retained item"
        );
    }

    #[tokio::test]
    async fn replay_enqueue_failure_does_not_schedule_a_second_attempt() {
        let dir = tempfile::tempdir().unwrap();
        save_state(dir.path(), Vec::new()).await;
        let driver = Arc::new(RecordingDriver::default());
        let runtime = TowerWakeRuntime::new(driver.clone());
        let original = rich_payload(&delivery("msg-1", "subject", None));
        prime_channel(
            &runtime,
            &driver,
            dir.path(),
            &delivery("msg-1", "subject", None),
        )
        .await;
        driver.fail_next_enqueue.store(true, Ordering::SeqCst);
        let handler = runtime.replay_handler("session-main", dir.path(), None);

        assert!(handler(original.clone()).await.is_err());
        assert_eq!(driver.payloads.lock().unwrap().len(), 1);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(driver.payloads.lock().unwrap().len(), 1);

        handler(original).await.unwrap();
        assert_eq!(driver.payloads.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn teardown_clears_the_channel_and_drops_by_session_and_variant() {
        let dir = tempfile::tempdir().unwrap();
        save_state(dir.path(), Vec::new()).await;
        let driver = Arc::new(RecordingDriver::default());
        let runtime = TowerWakeRuntime::new(driver.clone());
        let handler = runtime.replay_handler("session-main", dir.path(), None);

        assert!(
            runtime
                .clear("session-main", TOWER_INBOX_WAKE_VARIANT)
                .await
        );
        assert_eq!(
            driver.drops.lock().unwrap().as_slice(),
            &[(
                "session-main".to_string(),
                TOWER_INBOX_WAKE_VARIANT.to_string()
            )]
        );

        handler(rich_payload(&delivery("stale", "stale", None)))
            .await
            .unwrap();
        assert!(
            driver.payloads.lock().unwrap().is_empty(),
            "a replay racing teardown must not resurrect the channel"
        );
    }
}
