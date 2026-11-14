use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

struct Node<E> {
    element: E,
    next: Option<Arc<Mutex<Node<E>>>>,
    prev: Option<Arc<Mutex<Node<E>>>>,
}

struct LinkedListInner<E> {
    first: Option<Arc<Mutex<Node<E>>>>,
    last: Option<Arc<Mutex<Node<E>>>>,
    size: usize,
}

pub struct LinkedList<E> {
    inner: Arc<Mutex<LinkedListInner<E>>>,
}

impl<E> Clone for LinkedList<E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<E> Default for LinkedList<E> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LinkedListInner {
                first: None,
                last: None,
                size: 0,
            })),
        }
    }
}

impl<E> LinkedList<E>
where
    E: Clone + Send + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn size(&self) -> usize {
        self.inner.lock().size
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().first.is_none()
    }

    pub fn push(&self, element: E) -> Box<dyn FnOnce() + Send> {
        let node = Arc::new(Mutex::new(Node {
            element,
            next: None,
            prev: None,
        }));
        {
            let mut inner = self.inner.lock();
            if inner.first.is_none() {
                inner.first = Some(node.clone());
                inner.last = Some(node.clone());
            } else {
                let old_last = inner.last.clone().unwrap();
                node.lock().prev = Some(old_last.clone());
                old_last.lock().next = Some(node.clone());
                inner.last = Some(node.clone());
            }
            inner.size += 1;
        }
        let inner = Arc::downgrade(&self.inner);
        let did_remove = AtomicBool::new(false);
        Box::new(move || {
            if did_remove.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Some(inner) = inner.upgrade() {
                remove_node(&inner, &node);
            }
        })
    }

    pub fn shift(&self) -> Option<E> {
        let node = self.inner.lock().first.clone()?;
        remove_node(&self.inner, &node);
        Some(node.lock().element.clone())
    }

    pub fn peek(&self) -> Option<E> {
        self.inner
            .lock()
            .first
            .as_ref()
            .map(|node| node.lock().element.clone())
    }

    pub fn values(&self) -> Vec<E> {
        let mut values = Vec::new();
        let mut current = self.inner.lock().first.clone();
        while let Some(node) = current {
            let node = node.lock();
            values.push(node.element.clone());
            current = node.next.clone();
        }
        values
    }
}

fn remove_node<E>(inner: &Arc<Mutex<LinkedListInner<E>>>, node: &Arc<Mutex<Node<E>>>) {
    let mut inner = inner.lock();
    let (previous, next) = {
        let node = node.lock();
        (node.prev.clone(), node.next.clone())
    };
    match (&previous, &next) {
        (Some(previous), Some(next)) => {
            previous.lock().next = Some(next.clone());
            next.lock().prev = Some(previous.clone());
        }
        (None, None) => {
            inner.first = None;
            inner.last = None;
        }
        (Some(previous), None) => {
            previous.lock().next = None;
            inner.last = Some(previous.clone());
        }
        (None, Some(next)) => {
            next.lock().prev = None;
            inner.first = Some(next.clone());
        }
    }
    inner.size = inner.size.saturating_sub(1);
    let mut node = node.lock();
    node.prev = None;
    node.next = None;
}
