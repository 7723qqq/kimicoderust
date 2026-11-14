use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};

pub type NodeRef<T> = Rc<RefCell<Node<T>>>;

pub struct Node<T> {
    pub incoming: IndexMap<String, NodeRef<T>>,
    pub outgoing: IndexMap<String, NodeRef<T>>,
    pub key: String,
    pub data: T,
}

impl<T> Node<T> {
    pub fn new(key: String, data: T) -> NodeRef<T> {
        Rc::new(RefCell::new(Self {
            incoming: IndexMap::new(),
            outgoing: IndexMap::new(),
            key,
            data,
        }))
    }
}

pub struct Graph<T> {
    nodes: IndexMap<String, NodeRef<T>>,
    hash_fn: Arc<dyn Fn(&T) -> String>,
}

impl<T> Graph<T> {
    pub fn new(hash_fn: impl Fn(&T) -> String + 'static) -> Self {
        Self {
            nodes: IndexMap::new(),
            hash_fn: Arc::new(hash_fn),
        }
    }

    pub fn roots(&self) -> Vec<NodeRef<T>> {
        self.nodes
            .values()
            .filter(|node| node.borrow().outgoing.is_empty())
            .cloned()
            .collect()
    }

    pub fn insert_edge(&mut self, from: T, to: T) {
        let from_node = self.lookup_or_insert_node(from);
        let to_node = self.lookup_or_insert_node(to);
        if Rc::ptr_eq(&from_node, &to_node) {
            let key = from_node.borrow().key.clone();
            from_node
                .borrow_mut()
                .outgoing
                .insert(key.clone(), from_node.clone());
            from_node
                .borrow_mut()
                .incoming
                .insert(key, from_node.clone());
            return;
        }
        let from_key = from_node.borrow().key.clone();
        let to_key = to_node.borrow().key.clone();
        from_node
            .borrow_mut()
            .outgoing
            .insert(to_key, to_node.clone());
        to_node
            .borrow_mut()
            .incoming
            .insert(from_key, from_node);
    }

    pub fn remove_node(&mut self, data: &T) {
        let key = (self.hash_fn)(data);
        self.nodes.shift_remove(&key);
        for node in self.nodes.values() {
            node.borrow_mut().outgoing.shift_remove(&key);
            node.borrow_mut().incoming.shift_remove(&key);
        }
    }

    pub fn lookup_or_insert_node(&mut self, data: T) -> NodeRef<T> {
        let key = (self.hash_fn)(&data);
        if let Some(node) = self.nodes.get(&key) {
            return node.clone();
        }
        let node = Node::new(key.clone(), data);
        self.nodes.insert(key, node.clone());
        node
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn find_cycle_slow(&self) -> Option<String> {
        for (id, node) in &self.nodes {
            let mut seen = IndexSet::from([id.clone()]);
            let result = self.find_cycle(node, &mut seen);
            if result.is_some() {
                return result;
            }
        }
        None
    }

    fn find_cycle(&self, node: &NodeRef<T>, seen: &mut IndexSet<String>) -> Option<String> {
        let outgoing = node
            .borrow()
            .outgoing
            .iter()
            .map(|(id, node)| (id.clone(), node.clone()))
            .collect::<Vec<_>>();
        for (id, outgoing) in outgoing {
            if seen.contains(&id) {
                return Some(
                    seen.iter()
                        .cloned()
                        .chain(std::iter::once(id))
                        .collect::<Vec<_>>()
                        .join(" -> "),
                );
            }
            seen.insert(id.clone());
            let value = self.find_cycle(&outgoing, seen);
            if value.is_some() {
                return value;
            }
            seen.shift_remove(&id);
        }
        None
    }
}

impl<T> fmt::Display for Graph<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut data = Vec::new();
        for (key, value) in &self.nodes {
            let value = value.borrow();
            data.push(format!(
                "{key}\n\t(-> incoming)[{}]\n\t(outgoing ->)[{}]\n",
                value.incoming.keys().cloned().collect::<Vec<_>>().join(", "),
                value.outgoing.keys().cloned().collect::<Vec<_>>().join(",")
            ));
        }
        formatter.write_str(&data.join("\n"))
    }
}
