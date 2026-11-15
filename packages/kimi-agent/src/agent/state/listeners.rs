use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use super::errors::StateError;

type Listener<A> = Arc<dyn Fn(&A) -> Result<(), StateError> + Send + Sync>;

pub struct Listeners<A: 'static> {
    next_id: AtomicU64,
    items: Mutex<Vec<(u64, Listener<A>)>>,
}

impl<A: 'static> Default for Listeners<A> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            items: Mutex::new(Vec::new()),
        }
    }
}

impl<A: 'static> Listeners<A> {
    pub fn add<F>(self: &Arc<Self>, listener: F) -> ListenerRegistration<A>
    where
        F: Fn(&A) -> Result<(), StateError> + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.items.lock().push((id, Arc::new(listener)));
        ListenerRegistration {
            owner: self.clone(),
            id,
        }
    }

    fn remove(&self, id: u64) {
        self.items.lock().retain(|(item, _)| *item != id);
    }

    pub fn fire(&self, value: &A) -> Result<(), StateError> {
        let listeners: Vec<Listener<A>> = self
            .items
            .lock()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener(value)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct ListenerRegistration<A: 'static> {
    owner: Arc<Listeners<A>>,
    id: u64,
}

impl<A: 'static> ListenerRegistration<A> {
    pub fn dispose(&self) {
        self.owner.remove(self.id);
    }
}

impl<A: 'static> Drop for ListenerRegistration<A> {
    fn drop(&mut self) {
        self.owner.remove(self.id);
    }
}
