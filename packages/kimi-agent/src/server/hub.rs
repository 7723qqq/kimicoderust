use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use tokio::sync::{mpsc, watch};

use crate::events::{EngineEvent, EventBus};

pub const STATE_OPEN: u8 = 0;
pub const STATE_DETACHED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubClosed {
    Detached,
}

#[derive(Debug)]
pub struct SequencedEvent {
    pub session_id: Arc<str>,

    pub epoch: Arc<str>,

    pub seq: u64,
    pub event: EngineEvent,

    envelope: OnceLock<Arc<[u8]>>,
}

impl SequencedEvent {
    pub fn new(session_id: Arc<str>, epoch: Arc<str>, seq: u64, event: EngineEvent) -> Self {
        Self {
            session_id,
            epoch,
            seq,
            event,
            envelope: OnceLock::new(),
        }
    }

    pub fn envelope(&self) -> Arc<[u8]> {
        self.envelope
            .get_or_init(|| {
                match crate::server::ws_protocol::encode_envelope(
                    &self.session_id,
                    &self.epoch,
                    self.seq,
                    &self.event,
                ) {
                    Ok(bytes) => Arc::from(bytes.into_boxed_slice()),
                    Err(_) => Arc::from(Vec::<u8>::new()),
                }
            })
            .clone()
    }
}

struct Lane {
    bus: Arc<EventBus>,
    session_id: Arc<str>,
    epoch: Arc<str>,
    next_seq: AtomicU64,

    history: Mutex<VecDeque<Arc<SequencedEvent>>>,

    order: Mutex<()>,
}

struct Slot {
    id: u64,
    sender: mpsc::UnboundedSender<Arc<SequencedEvent>>,
    state: watch::Sender<u8>,
    remote_address: Option<String>,
    user_agent: Option<String>,
    connected_at: u64,
    client_hello: Arc<AtomicBool>,
    subscriptions: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: u64,
    pub remote_address: Option<String>,
    pub user_agent: Option<String>,
    pub connected_at: u64,
    pub has_client_hello: bool,
    pub subscriptions: Vec<String>,
}

type Slots = Arc<RwLock<Vec<Arc<Slot>>>>;

pub type EventPersister = Arc<dyn Fn(&SequencedEvent) + Send + Sync>;

#[derive(Clone, Default)]
pub struct PersistedHeads(Arc<Mutex<HashMap<String, u64>>>);

impl PersistedHeads {
    pub fn note(&self, session_id: &str, seq: u64) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), seq);
    }

    pub fn get(&self, session_id: &str) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .copied()
    }
}

pub struct EventHub {
    lanes: RwLock<HashMap<String, Arc<Lane>>>,
    slots: Slots,
    next_slot_id: AtomicU64,
    persister: Arc<RwLock<Option<EventPersister>>>,
    persisted_heads: PersistedHeads,
}

impl Default for EventHub {
    fn default() -> Self {
        Self::new()
    }
}

impl EventHub {
    pub fn new() -> Self {
        Self {
            lanes: RwLock::new(HashMap::new()),
            slots: Arc::new(RwLock::new(Vec::new())),
            next_slot_id: AtomicU64::new(1),
            persister: Arc::new(RwLock::new(None)),
            persisted_heads: PersistedHeads::default(),
        }
    }

    pub fn persisted_heads(&self) -> PersistedHeads {
        self.persisted_heads.clone()
    }

    pub fn persisted_head(&self, session_id: &str) -> Option<u64> {
        self.persisted_heads.get(session_id)
    }

    pub fn forget_persisted_head(&self, session_id: &str) {
        self.persisted_heads
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    pub fn set_persister(&self, persister: EventPersister) {
        *self
            .persister
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(persister);
    }

    pub fn bus_for(&self, session_id: &str) -> Arc<EventBus> {
        let existing = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|lane| lane.bus.clone());
        if let Some(bus) = existing {
            return bus;
        }
        let mut lanes = self
            .lanes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(lane) = lanes.get(session_id) {
            return lane.bus.clone();
        }

        let lane = Arc::new(Lane {
            bus: Arc::new(EventBus::new()),
            session_id: Arc::from(session_id),
            epoch: Arc::from(new_epoch().as_str()),
            next_seq: AtomicU64::new(0),
            history: Mutex::new(VecDeque::new()),
            order: Mutex::new(()),
        });
        let forward = Arc::clone(&lane);
        let slots = Arc::clone(&self.slots);
        let persister = Arc::clone(&self.persister);
        lane.bus.subscribe(move |event| {
            let _ordered = forward
                .order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let seq = forward.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let sequenced = Arc::new(SequencedEvent::new(
                Arc::clone(&forward.session_id),
                Arc::clone(&forward.epoch),
                seq,
                event.clone(),
            ));
            let mut history = forward
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            history.push_back(Arc::clone(&sequenced));
            drop(history);
            if let Some(ref p) = *persister
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                p(&sequenced);
            }
            deliver(&slots, sequenced);
        });
        lanes.insert(session_id.to_string(), lane.clone());
        lane.bus.clone()
    }

    pub fn ensure_lane_with_initial_seq(
        &self,
        session_id: &str,
        initial_seq: u64,
    ) -> (u64, Arc<str>) {
        let _bus = self.bus_for(session_id);
        let lanes = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lane = lanes.get(session_id).expect("lane was just ensured");
        let _ = lane.next_seq.fetch_max(initial_seq, Ordering::Relaxed);
        (
            lane.next_seq.load(Ordering::Relaxed),
            Arc::clone(&lane.epoch),
        )
    }

    pub fn lane_session_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    pub fn lane_exists(&self, session_id: &str) -> bool {
        self.lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(session_id)
    }

    pub fn evict_lane(&self, session_id: &str) {
        self.lanes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(session_id);
    }

    pub fn lane_count(&self) -> usize {
        self.lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn ensure_lane_cursor(&self, session_id: &str) -> (u64, Arc<str>) {
        let _bus = self.bus_for(session_id);
        let lanes = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lane = lanes.get(session_id).expect("lane was just ensured");
        (
            lane.next_seq.load(Ordering::Relaxed),
            Arc::clone(&lane.epoch),
        )
    }

    pub fn session_cursor(&self, session_id: &str) -> Option<(u64, Arc<str>)> {
        let lanes = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lanes.get(session_id).map(|lane| {
            (
                lane.next_seq.load(Ordering::Relaxed),
                Arc::clone(&lane.epoch),
            )
        })
    }

    pub fn replay_for(&self, session_id: &str, since_seq: u64) -> Vec<Arc<SequencedEvent>> {
        let lanes = self
            .lanes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(lane) = lanes.get(session_id) else {
            return Vec::new();
        };
        let _order = lane
            .order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let history = lane
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        history
            .iter()
            .filter(|e| e.seq > since_seq)
            .cloned()
            .collect()
    }

    pub fn attach(&self) -> WsSubscription {
        self.attach_from(None, None)
    }

    pub fn attach_from(
        &self,
        remote_address: Option<String>,
        user_agent: Option<String>,
    ) -> WsSubscription {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (state_sender, state) = watch::channel(STATE_OPEN);
        let id = self.next_slot_id.fetch_add(1, Ordering::Relaxed);
        let client_hello = Arc::new(AtomicBool::new(false));
        let subscriptions = Arc::new(Mutex::new(HashSet::new()));

        let slot = Arc::new(Slot {
            id,
            sender,
            state: state_sender,
            remote_address,
            user_agent,
            connected_at: chrono::Utc::now().timestamp_millis() as u64,
            client_hello: Arc::clone(&client_hello),
            subscriptions: Arc::clone(&subscriptions),
        });
        self.slots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(slot);

        let lanes: Vec<Arc<Lane>> = {
            let lanes = self
                .lanes
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut lanes: Vec<Arc<Lane>> = lanes.values().cloned().collect();
            lanes.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            lanes
        };
        let mut replay = VecDeque::new();
        let _lane_locks: Vec<_> = lanes
            .iter()
            .map(|lane| {
                lane.order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            })
            .collect();
        for lane in &lanes {
            replay.extend(
                lane.history
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .cloned(),
            );
        }

        WsSubscription {
            slots: Arc::clone(&self.slots),
            id,
            receiver,
            state,
            replay,
            delivered: HashMap::new(),
            client_hello,
            subscriptions,
        }
    }

    pub fn connections(&self) -> Vec<ConnectionInfo> {
        self.slots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|slot| ConnectionInfo {
                id: slot.id,
                remote_address: slot.remote_address.clone(),
                user_agent: slot.user_agent.clone(),
                connected_at: slot.connected_at,
                has_client_hello: slot.client_hello.load(Ordering::Relaxed),
                subscriptions: slot
                    .subscriptions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned()
                    .collect(),
            })
            .collect()
    }

    pub fn subscriber_count(&self) -> usize {
        self.slots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

fn new_epoch() -> String {
    format!("epoch-{:016x}", fastrand::u64(..))
}

fn deliver(slots: &Slots, event: Arc<SequencedEvent>) {
    for slot in slots
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
    {
        if slot.sender.send(Arc::clone(&event)).is_err() {
            let _ = slot.state.send(STATE_DETACHED);
        }
    }
}

pub struct WsSubscription {
    slots: Slots,
    id: u64,
    receiver: mpsc::UnboundedReceiver<Arc<SequencedEvent>>,
    state: watch::Receiver<u8>,

    replay: VecDeque<Arc<SequencedEvent>>,

    delivered: HashMap<(Arc<str>, Arc<str>), u64>,

    client_hello: Arc<AtomicBool>,

    subscriptions: Arc<Mutex<HashSet<String>>>,
}

impl WsSubscription {
    pub fn mark_client_hello(&self) {
        self.client_hello.store(true, Ordering::Relaxed);
    }

    pub fn set_subscriptions(&self, ids: &HashSet<String>) {
        let mut set = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        *set = ids.clone();
    }

    pub async fn recv(&mut self) -> Result<Arc<SequencedEvent>, HubClosed> {
        loop {
            while let Some(event) = self.replay.pop_front() {
                if mark_delivered(&mut self.delivered, &event) {
                    return Ok(event);
                }
            }
            while let Ok(event) = self.receiver.try_recv() {
                if mark_delivered(&mut self.delivered, &event) {
                    return Ok(event);
                }
            }
            if *self.state.borrow() != STATE_OPEN {
                return Err(HubClosed::Detached);
            }

            tokio::select! {
                biased;
                next = self.receiver.recv() => {
                    match next {
                        Some(event) => {
                            if mark_delivered(&mut self.delivered, &event) {
                                return Ok(event);
                            }

                        }


                        None => {
                            let _ = self.state.borrow_and_update();
                            return Err(HubClosed::Detached);
                        }
                    }
                }
                changed = self.state.changed() => {
                    if changed.is_err() {
                        return Err(HubClosed::Detached);
                    }
                }
            }
        }
    }

    pub fn state_rx(&self) -> watch::Receiver<u8> {
        self.state.clone()
    }

    pub fn closed(&self) -> Option<HubClosed> {
        let state = *self.state.borrow();
        if state == STATE_OPEN {
            None
        } else {
            Some(HubClosed::Detached)
        }
    }
}

fn mark_delivered(
    delivered: &mut HashMap<(Arc<str>, Arc<str>), u64>,
    event: &SequencedEvent,
) -> bool {
    let key = (Arc::clone(&event.session_id), Arc::clone(&event.epoch));
    let high = delivered.entry(key).or_insert(0);
    if event.seq <= *high {
        return false;
    }
    *high = event.seq;
    true
}

impl Drop for WsSubscription {
    fn drop(&mut self) {
        let mut slots = self
            .slots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(pos) = slots.iter().position(|slot| slot.id == self.id) {
            slots.swap_remove(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step_event(step: u32) -> EngineEvent {
        EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t1".into(),
            step,
        }
    }

    #[tokio::test]
    async fn a_new_subscriber_receives_events_published_after_it_attaches() {
        let hub = EventHub::new();
        let mut sub = hub.attach();

        hub.bus_for("sess-1").publish(&step_event(1));
        let first = sub.recv().await.unwrap();
        assert_eq!(first.event.event_type(), "llm.step.begin");
        assert_eq!(first.seq, 1);

        drop(sub);
        assert_eq!(hub.subscriber_count(), 0, "dropping leaked a slot");
    }

    #[tokio::test]
    async fn evict_lane_drops_the_lane_and_restarts_its_numbering() {
        let hub = EventHub::new();
        let (_, epoch_before) = hub.ensure_lane_cursor("sess-1");
        hub.bus_for("sess-1").publish(&step_event(1));
        assert!(hub.lane_exists("sess-1"));

        hub.evict_lane("sess-1");
        assert!(!hub.lane_exists("sess-1"));

        let mut sub = hub.attach();
        let (_, epoch_after) = hub.ensure_lane_cursor("sess-1");
        assert_ne!(epoch_before, epoch_after);
        hub.bus_for("sess-1").publish(&step_event(7));
        let first = sub.recv().await.unwrap();
        assert_eq!(first.seq, 1, "a fresh lane restarts seq numbering");
        assert_eq!(first.epoch.as_ref(), epoch_after.as_ref());
    }

    #[tokio::test]
    async fn connections_report_metadata_and_subscriptions() {
        let hub = EventHub::new();
        let sub = hub.attach_from(Some("10.0.0.5:1234".into()), Some("test-agent".into()));
        let conns = hub.connections();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].remote_address.as_deref(), Some("10.0.0.5:1234"));
        assert_eq!(conns[0].user_agent.as_deref(), Some("test-agent"));
        assert!(!conns[0].has_client_hello);
        assert!(conns[0].subscriptions.is_empty());

        sub.mark_client_hello();
        sub.set_subscriptions(&HashSet::from(["sess-1".to_string()]));
        let conns = hub.connections();
        assert!(conns[0].has_client_hello);
        assert_eq!(conns[0].subscriptions, vec!["sess-1".to_string()]);
    }

    #[tokio::test]
    async fn each_subscriber_sees_the_same_events_with_the_same_numbers() {
        let hub = EventHub::new();
        let mut first = hub.attach();
        let mut second = hub.attach();
        assert_eq!(hub.subscriber_count(), 2);

        let bus = hub.bus_for("sess-1");
        for step in 1..=3 {
            bus.publish(&step_event(step));
        }
        for expected in 1..=3_u64 {
            let a = first.recv().await.unwrap();
            let b = second.recv().await.unwrap();
            assert_eq!(a.seq, expected, "seq must not depend on the reader");
            assert_eq!(b.seq, expected);
            assert_eq!(a.epoch, b.epoch);
            let EngineEvent::LlmStepBegin { step, .. } = &a.event else {
                panic!("wrong variant on first subscriber");
            };
            assert_eq!(u64::from(*step), expected);
        }
    }

    #[tokio::test]
    async fn lanes_number_independently_and_keep_their_own_epoch() {
        let hub = EventHub::new();
        let one = hub.bus_for("sess-1");
        let two = hub.bus_for("sess-2");

        one.publish(&step_event(1));
        one.publish(&step_event(2));
        two.publish(&step_event(1));

        let mut sub = hub.attach();
        let a = sub.recv().await.unwrap();
        let b = sub.recv().await.unwrap();
        let c = sub.recv().await.unwrap();

        assert_eq!((&*a.session_id, a.seq), ("sess-1", 1));
        assert_eq!((&*b.session_id, b.seq), ("sess-1", 2));
        assert_eq!((&*c.session_id, c.seq), ("sess-2", 1));
        assert_ne!(
            a.epoch, c.epoch,
            "a second session's stream is not a continuation of the first"
        );
        assert_eq!(hub.lane_count(), 2);
        assert_eq!(hub.lane_session_ids(), vec!["sess-1", "sess-2"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_publishers_on_one_lane_stay_consecutive_and_in_order() {
        let hub = EventHub::new();
        let bus = hub.bus_for("sess-1");
        let mut sub = hub.attach();

        let publishers: Vec<_> = (0..4_u32)
            .map(|worker| {
                let bus = bus.clone();
                tokio::spawn(async move {
                    for step in 0..25 {
                        bus.publish(&step_event(worker * 100 + step));
                    }
                })
            })
            .collect();
        for publisher in publishers {
            publisher.await.unwrap();
        }

        let mut seen = Vec::new();
        for _ in 0..100 {
            seen.push(sub.recv().await.unwrap().seq);
        }
        assert_eq!(
            seen,
            (1..=100_u64).collect::<Vec<u64>>(),
            "a lane must number its stream consecutively, in delivery order"
        );
    }

    #[tokio::test]
    async fn asking_twice_for_a_lane_returns_the_same_numbering() {
        let hub = EventHub::new();
        let first = hub.bus_for("sess-1");
        let second = hub.bus_for("sess-1");
        assert!(Arc::ptr_eq(&first, &second), "a lane must not fork");

        first.publish(&step_event(1));
        second.publish(&step_event(2));
        let mut sub = hub.attach();
        assert_eq!(sub.recv().await.unwrap().seq, 1);
        assert_eq!(sub.recv().await.unwrap().seq, 2);
    }

    #[tokio::test]
    async fn publishing_does_not_run_a_handler_that_can_reenter_the_hub() {
        let hub = EventHub::new();
        let bus = hub.bus_for("sess-1");
        let sub = hub.attach();
        assert_eq!(hub.subscriber_count(), 1);

        bus.publish(&step_event(1));
        drop(sub);
        bus.publish(&step_event(2));
        assert_eq!(hub.subscriber_count(), 0);
    }

    #[tokio::test]
    async fn replayed_history_then_live_events_stay_contiguous_without_duplicates() {
        let hub = EventHub::new();
        let bus = hub.bus_for("sess-1");
        bus.publish(&step_event(1));
        bus.publish(&step_event(2));

        let mut sub = hub.attach();

        bus.publish(&step_event(3));
        bus.publish(&step_event(4));

        let mut seqs = Vec::new();
        for _ in 0..4 {
            seqs.push(sub.recv().await.unwrap().seq);
        }
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    #[test]
    fn mark_delivered_filters_the_replay_live_overlap_by_high_water_seq() {
        let mut delivered = HashMap::new();
        let event = |seq: u64| {
            SequencedEvent::new(
                Arc::from("sess-1"),
                Arc::from("epoch-a"),
                seq,
                step_event(seq as u32),
            )
        };
        assert!(mark_delivered(&mut delivered, &event(1)));
        assert!(mark_delivered(&mut delivered, &event(2)));

        assert!(!mark_delivered(&mut delivered, &event(1)));
        assert!(!mark_delivered(&mut delivered, &event(2)));
        assert!(mark_delivered(&mut delivered, &event(3)));

        let other =
            SequencedEvent::new(Arc::from("sess-2"), Arc::from("epoch-b"), 1, step_event(1));
        assert!(mark_delivered(&mut delivered, &other));
    }

    #[tokio::test]
    async fn lane_history_is_not_capped() {
        let hub = EventHub::new();
        let bus = hub.bus_for("sess-1");
        const TOTAL: u64 = 1_000;
        for step in 0..TOTAL {
            bus.publish(&step_event(step as u32));
        }
        let mut sub = hub.attach();
        let mut count = 0;
        let mut last_seq = 0;
        while count < TOTAL {
            let event = sub.recv().await.unwrap();
            count += 1;
            last_seq = event.seq;
        }
        assert_eq!(count, TOTAL);
        assert_eq!(last_seq, TOTAL, "numbering must survive the whole buffer");
    }
}
