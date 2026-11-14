use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::types::EngineEvent;

pub type EventHandler = Arc<dyn Fn(&EngineEvent) + Send + Sync + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Subscription(u64);

struct SubscriberEntry {
    id: Subscription,
    handler: EventHandler,
    filter: Option<String>,
    agent: Option<String>,
}

#[derive(Default)]
pub struct EventBus {
    next_id: AtomicU64,
    subscribers: RwLock<Vec<Arc<SubscriberEntry>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            subscribers: RwLock::new(Vec::new()),
        }
    }

    pub fn subscribe<F>(&self, handler: F) -> Subscription
    where
        F: Fn(&EngineEvent) + Send + Sync + 'static,
    {
        self.subscribe_inner(None, None, handler)
    }

    pub fn subscribe_filtered<F>(&self, event_type: impl Into<String>, handler: F) -> Subscription
    where
        F: Fn(&EngineEvent) + Send + Sync + 'static,
    {
        self.subscribe_inner(Some(event_type.into()), None, handler)
    }

    /// v2 `EventBus.subscribeAgent`: a handler that only receives another
    /// agent's events. The per-agent channel is what `listenerCounts().perAgent`
    /// counts.
    pub fn subscribe_for_agent<F>(
        &self,
        agent_id: impl Into<String>,
        handler: F,
    ) -> Subscription
    where
        F: Fn(&EngineEvent) + Send + Sync + 'static,
    {
        self.subscribe_inner(None, Some(agent_id.into()), handler)
    }

    /// v2 `EventBus.subscribeAgent(agent, type, handler)`.
    pub fn subscribe_filtered_for_agent<F>(
        &self,
        agent_id: impl Into<String>,
        event_type: impl Into<String>,
        handler: F,
    ) -> Subscription
    where
        F: Fn(&EngineEvent) + Send + Sync + 'static,
    {
        self.subscribe_inner(Some(event_type.into()), Some(agent_id.into()), handler)
    }

    fn subscribe_inner<F>(
        &self,
        filter: Option<String>,
        agent: Option<String>,
        handler: F,
    ) -> Subscription
    where
        F: Fn(&EngineEvent) + Send + Sync + 'static,
    {
        let id = Subscription(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut subs = self.subscribers.write().unwrap();
        subs.push(Arc::new(SubscriberEntry {
            id,
            handler: Arc::new(handler),
            filter,
            agent,
        }));
        id
    }

    pub fn unsubscribe(&self, sub: Subscription) -> bool {
        let mut subs = self.subscribers.write().unwrap();
        if let Some(pos) = subs.iter().position(|s| s.id == sub) {
            subs.swap_remove(pos);
            true
        } else {
            false
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.read().unwrap().len()
    }

    /// v2 `EventBus.listenerCounts()`: every listener, the count per event type,
    /// and the count per agent channel.
    pub fn subscriber_snapshot(
        &self,
    ) -> (
        usize,
        std::collections::BTreeMap<String, usize>,
        std::collections::BTreeMap<String, usize>,
    ) {
        let subs = self.subscribers.read().unwrap();
        let mut per_type: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut per_agent: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for sub in subs.iter() {
            if let Some(ref filter) = sub.filter {
                *per_type.entry(filter.clone()).or_insert(0) += 1;
            }
            if let Some(ref agent) = sub.agent {
                *per_agent.entry(agent.clone()).or_insert(0) += 1;
            }
        }
        (subs.len(), per_type, per_agent)
    }

    pub fn publish(&self, event: &EngineEvent) {
        let subs: Vec<Arc<SubscriberEntry>> = self.subscribers.read().unwrap().clone();
        let ev_type = event.event_type();
        let ev_agent = event.agent_id().map(str::to_string);
        for sub in subs.iter() {
            if let Some(ref filter) = sub.filter
                && filter != ev_type
            {
                continue;
            }
            if let Some(ref agent) = sub.agent
                && ev_agent.as_deref() != Some(agent.as_str())
            {
                continue;
            }
            (sub.handler)(event);
        }
    }

    pub fn publish_json(&self, value: serde_json::Value) {
        let event = EngineEvent::from_json(value);
        self.publish(&event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_event_bus_broadcast() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));

        let count_clone = count.clone();
        let sub = bus.subscribe(move |_| {
            count_clone.fetch_add(1, Ordering::Relaxed);
        });

        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 2,
        });

        assert_eq!(count.load(Ordering::Relaxed), 2);

        assert!(bus.unsubscribe(sub));
        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 3,
        });
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_filtered_subscription() {
        let bus = EventBus::new();
        let tool_events = Arc::new(AtomicUsize::new(0));

        let c = tool_events.clone();
        bus.subscribe_filtered("tool.native", move |_| {
            c.fetch_add(1, Ordering::Relaxed);
        });

        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
        assert_eq!(tool_events.load(Ordering::Relaxed), 0);

        bus.publish(&EngineEvent::ToolNative {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            tool_call_id: "tc-1".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({}),
            content: "ok".into(),
            is_error: false,
            note: None,
        });
        assert_eq!(tool_events.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_per_agent_subscription_only_receives_its_own_events() {
        let bus = EventBus::new();
        let main = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));

        let m = main.clone();
        bus.subscribe_for_agent("main", move |_| {
            m.fetch_add(1, Ordering::Relaxed);
        });
        let o = other.clone();
        bus.subscribe_for_agent("sub-1", move |_| {
            o.fetch_add(1, Ordering::Relaxed);
        });

        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: "main".to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
        assert_eq!(main.load(Ordering::Relaxed), 1);
        assert_eq!(other.load(Ordering::Relaxed), 0);

        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: "sub-1".to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
        assert_eq!(main.load(Ordering::Relaxed), 1);
        assert_eq!(other.load(Ordering::Relaxed), 1);

        let (all, per_type, per_agent) = bus.subscriber_snapshot();
        assert_eq!(all, 2);
        assert!(per_type.is_empty(), "{per_type:?}");
        assert_eq!(per_agent.get("main"), Some(&1));
        assert_eq!(per_agent.get("sub-1"), Some(&1));
    }

    #[test]
    fn a_filtered_per_agent_subscription_counts_in_both_dimensions() {
        let bus = EventBus::new();
        bus.subscribe_filtered_for_agent("main", "tool.native", |_| ());
        bus.subscribe_filtered("tool.native", |_| ());

        let (all, per_type, per_agent) = bus.subscriber_snapshot();
        assert_eq!(all, 2);
        assert_eq!(per_type.get("tool.native"), Some(&2));
        assert_eq!(per_agent.get("main"), Some(&1));
    }

    #[test]
    fn a_handler_may_subscribe_and_unsubscribe_during_publish() {        let bus = Arc::new(EventBus::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let bus_for_handler = Arc::clone(&bus);
        let calls_for_handler = Arc::clone(&calls);
        let own = bus.subscribe(move |_| {
            let nested = bus_for_handler.subscribe({
                let calls = Arc::clone(&calls_for_handler);
                move |_| {
                    calls.fetch_add(1, Ordering::Relaxed);
                }
            });
            assert!(bus_for_handler.unsubscribe(nested));
            calls_for_handler.fetch_add(1, Ordering::Relaxed);
        });

        bus.publish(&EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(bus.subscriber_count(), 1);
        assert!(bus.unsubscribe(own));
        assert_eq!(bus.subscriber_count(), 0);
    }
}
