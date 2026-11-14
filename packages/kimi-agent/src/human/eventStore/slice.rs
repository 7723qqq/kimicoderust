use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::human::eventStore::events::InternalEvent;
use crate::human::store::types::BranchRef;

pub type Effect = Box<dyn FnOnce() + Send>;

#[derive(Clone)]
pub struct Enqueue {
    raise: Arc<dyn Fn(InternalEvent) + Send + Sync>,
    effect: Arc<dyn Fn(Effect) + Send + Sync>,
}

impl Enqueue {
    pub fn new(
        raise: impl Fn(InternalEvent) + Send + Sync + 'static,
        effect: impl Fn(Effect) + Send + Sync + 'static,
    ) -> Self {
        Self {
            raise: Arc::new(raise),
            effect: Arc::new(effect),
        }
    }

    pub fn raise(&self, event: InternalEvent) {
        (self.raise)(event);
    }

    pub fn effect(&self, effect: Effect) {
        (self.effect)(effect);
    }
}

pub struct FoldContext {
    pub ref_: BranchRef,
    pub ts: u64,
    pub replaying: bool,
    pub enqueue: Enqueue,
}

pub type Reducer<S> = Arc<
    dyn Fn(&mut S, &InternalEvent, &mut FoldContext) -> Option<S> + Send + Sync,
>;

#[derive(Clone)]
pub struct Slice<S = Value> {
    pub name: String,
    pub initial_state: Arc<dyn Fn() -> S + Send + Sync>,
    pub reducers: HashMap<String, Reducer<S>>,
}

pub fn create_slice<S>(def: Slice<S>) -> Slice<S> {
    def
}
