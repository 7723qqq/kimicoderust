use std::collections::HashMap;

use indexmap::IndexMap;

use super::descriptors::DynamicValue;
use super::instantiation::{ServiceIdentifier, refine_service_decorator};

pub type ObjectId = usize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedToken {
    pub scope: ObjectId,
    pub token: ServiceIdentifier<DynamicValue>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencyEdgeKind {
    Instance,
    Collection,
}

#[derive(Clone, Debug)]
pub struct DependencyEdge {
    pub consumer: ScopedToken,
    pub dependency: ScopedToken,
    pub kind: DependencyEdgeKind,
}

pub struct PairIndex<V> {
    map: IndexMap<ObjectId, IndexMap<ServiceIdentifier<DynamicValue>, V>>,
}

impl<V> Default for PairIndex<V> {
    fn default() -> Self {
        Self {
            map: IndexMap::new(),
        }
    }
}

impl<V> PairIndex<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<T>(
        &self,
        scope: ObjectId,
        token: &ServiceIdentifier<T>,
    ) -> Option<&V> {
        self.map
            .get(&scope)
            .and_then(|inner| inner.get(&refine_service_decorator(token)))
    }

    pub fn set<T>(&mut self, scope: ObjectId, token: &ServiceIdentifier<T>, value: V) {
        self.map
            .entry(scope)
            .or_default()
            .insert(refine_service_decorator(token), value);
    }

    pub fn delete<T>(&mut self, scope: ObjectId, token: &ServiceIdentifier<T>) {
        let token = refine_service_decorator(token);
        let remove_scope = if let Some(inner) = self.map.get_mut(&scope) {
            inner.shift_remove(&token);
            inner.is_empty()
        } else {
            false
        };
        if remove_scope {
            self.map.shift_remove(&scope);
        }
    }

    pub fn delete_scope(&mut self, scope: ObjectId) {
        self.map.shift_remove(&scope);
    }

    pub fn size(&self) -> usize {
        self.map.len()
    }

    pub fn entries(&self) -> Vec<(ScopedToken, &V)> {
        let mut out = Vec::new();
        for (scope, inner) in &self.map {
            for (token, value) in inner {
                out.push((
                    ScopedToken {
                        scope: *scope,
                        token: token.clone(),
                    },
                    value,
                ));
            }
        }
        out
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}

#[derive(Default)]
pub struct DependencyGraph {
    ref_by_instance: IndexMap<ObjectId, ScopedToken>,
    instance_by_ref: PairIndex<ObjectId>,
    out: IndexMap<ObjectId, PairIndex<DependencyEdgeKind>>,
    inbound: PairIndex<IndexMap<ObjectId, DependencyEdgeKind>>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_instance<T>(
        &mut self,
        instance: ObjectId,
        scope: ObjectId,
        token: ServiceIdentifier<T>,
    ) {
        let token = refine_service_decorator(&token);
        let reference = ScopedToken { scope, token };
        self.ref_by_instance.insert(instance, reference.clone());
        self.instance_by_ref.set(
            reference.scope,
            &reference.token,
            instance,
        );
    }

    pub fn remove_instance(&mut self, instance: ObjectId) {
        if let Some(reference) = self.ref_by_instance.shift_remove(&instance)
            && self.instance_by_ref.get(reference.scope, &reference.token) == Some(&instance)
        {
            self.instance_by_ref
                .delete(reference.scope, &reference.token);
        }
        if let Some(out) = self.out.shift_remove(&instance) {
            for (dependency, _) in out.entries() {
                let remove_inbound = if let Some(inbound) = self
                    .inbound
                    .get(dependency.scope, &dependency.token)
                    .cloned()
                {
                    let mut inbound = inbound;
                    inbound.shift_remove(&instance);
                    if inbound.is_empty() {
                        true
                    } else {
                        self.inbound.set(
                            dependency.scope,
                            &dependency.token,
                            inbound,
                        );
                        false
                    }
                } else {
                    false
                };
                if remove_inbound {
                    self.inbound
                        .delete(dependency.scope, &dependency.token);
                }
            }
        }
    }

    pub fn remove_scope(&mut self, scope: ObjectId) {
        let instances = self
            .ref_by_instance
            .iter()
            .filter(|(_, reference)| reference.scope == scope)
            .map(|(instance, _)| *instance)
            .collect::<Vec<_>>();
        for instance in instances {
            self.remove_instance(instance);
        }
        self.instance_by_ref.delete_scope(scope);
        self.inbound.delete_scope(scope);
        let consumers = self.out.keys().copied().collect::<Vec<_>>();
        for instance in consumers {
            let remove = if let Some(out) = self.out.get_mut(&instance) {
                out.delete_scope(scope);
                out.size() == 0
            } else {
                false
            };
            if remove {
                self.out.shift_remove(&instance);
            }
        }
    }

    pub fn add_edge(
        &mut self,
        consumer_instance: ObjectId,
        dependency: ScopedToken,
        kind: DependencyEdgeKind,
    ) {
        self.out
            .entry(consumer_instance)
            .or_default()
            .set(
                dependency.scope,
                &dependency.token,
                kind,
            );
        let mut inbound = self
            .inbound
            .get(dependency.scope, &dependency.token)
            .cloned()
            .unwrap_or_default();
        inbound.insert(consumer_instance, kind);
        self.inbound
            .set(dependency.scope, &dependency.token, inbound);
    }

    pub fn affected_set(&self, changed: impl IntoIterator<Item = ScopedToken>) -> Vec<ScopedToken> {
        let mut seen = PairIndex::<bool>::new();
        let mut queue = Vec::new();
        let push = |reference: ScopedToken,
                    seen: &mut PairIndex<bool>,
                    queue: &mut Vec<ScopedToken>| {
            if seen
                .get(reference.scope, &reference.token)
                .is_some()
            {
                return;
            }
            seen.set(reference.scope, &reference.token, true);
            queue.push(reference);
        };
        for reference in changed {
            push(reference, &mut seen, &mut queue);
        }
        let mut affected = Vec::new();
        while let Some(reference) = queue.pop() {
            affected.push(reference.clone());
            let Some(inbound) = self.inbound.get(reference.scope, &reference.token) else {
                continue;
            };
            for (consumer_instance, kind) in inbound {
                if *kind != DependencyEdgeKind::Instance {
                    continue;
                }
                if let Some(consumer) = self.ref_by_instance.get(consumer_instance) {
                    push(consumer.clone(), &mut seen, &mut queue);
                }
            }
        }
        affected
    }

    pub fn topo_order(&self, tokens: impl IntoIterator<Item = ScopedToken>) -> Vec<ScopedToken> {
        let mut subset = PairIndex::<ScopedToken>::new();
        for reference in tokens {
            subset.set(
                reference.scope,
                &reference.token,
                reference,
            );
        }
        let mut ordered = Vec::new();
        let mut done = PairIndex::<bool>::new();
        for (_, reference) in subset.entries() {
            self.visit_topological(reference, &subset, &mut done, &mut ordered);
        }
        ordered
    }

    fn visit_topological(
        &self,
        reference: &ScopedToken,
        subset: &PairIndex<ScopedToken>,
        done: &mut PairIndex<bool>,
        ordered: &mut Vec<ScopedToken>,
    ) {
        if done.get(reference.scope, &reference.token).is_some() {
            return;
        }
        done.set(reference.scope, &reference.token, true);
        for dependency in self.dependencies_of(reference, Some(subset)) {
            self.visit_topological(&dependency, subset, done, ordered);
        }
        ordered.push(reference.clone());
    }

    pub fn reverse_topo_order(
        &self,
        tokens: impl IntoIterator<Item = ScopedToken>,
    ) -> Vec<ScopedToken> {
        let mut ordered = self.topo_order(tokens);
        ordered.reverse();
        ordered
    }

    pub fn find_cycle(&self, label: impl Fn(&ScopedToken) -> String) -> Option<Vec<String>> {
        fn visit(
            graph: &DependencyGraph,
            reference: &ScopedToken,
            state: &mut PairIndex<VisitState>,
            path: &mut Vec<ScopedToken>,
            label: &dyn Fn(&ScopedToken) -> String,
        ) -> Option<Vec<String>> {
            state.set(reference.scope, &reference.token, VisitState::Visiting);
            path.push(reference.clone());
            for dependency in graph.dependencies_of(reference, None) {
                let mark = state.get(dependency.scope, &dependency.token);
                if mark == Some(&VisitState::Done) {
                    continue;
                }
                if mark == Some(&VisitState::Visiting) {
                    let start = path.iter().position(|entry| entry == &dependency)?;
                    let mut cycle = path[start..].to_vec();
                    cycle.push(dependency);
                    return Some(cycle.iter().map(label).collect());
                }
                let cycle = visit(graph, &dependency, state, path, label);
                if cycle.is_some() {
                    return cycle;
                }
            }
            path.pop();
            state.set(reference.scope, &reference.token, VisitState::Done);
            None
        }

        let mut state = PairIndex::new();
        let mut path = Vec::new();
        for (reference, _) in self.instance_by_ref.entries() {
            if state.get(reference.scope, &reference.token).is_some() {
                continue;
            }
            let cycle = visit(self, &reference, &mut state, &mut path, &label);
            if cycle.is_some() {
                return cycle;
            }
        }
        None
    }

    pub fn edges(&self) -> Vec<DependencyEdge> {
        let mut edges = Vec::new();
        for (consumer_instance, out) in &self.out {
            let Some(consumer) = self.ref_by_instance.get(consumer_instance) else {
                continue;
            };
            for (dependency, kind) in out.entries() {
                edges.push(DependencyEdge {
                    consumer: consumer.clone(),
                    dependency,
                    kind: *kind,
                });
            }
        }
        edges
    }

    pub fn clear(&mut self) {
        self.ref_by_instance.clear();
        self.instance_by_ref.clear();
        self.out.clear();
        self.inbound.clear();
    }

    fn dependencies_of(
        &self,
        reference: &ScopedToken,
        subset: Option<&PairIndex<ScopedToken>>,
    ) -> Vec<ScopedToken> {
        let Some(instance) = self
            .instance_by_ref
            .get(reference.scope, &reference.token)
        else {
            return Vec::new();
        };
        let Some(out) = self.out.get(instance) else {
            return Vec::new();
        };
        out.entries()
            .into_iter()
            .filter(|(dependency, kind)| {
                *kind == DependencyEdgeKind::Instance
                    && subset.is_none_or(|subset| {
                        subset
                            .get(dependency.scope, &dependency.token)
                            .is_some()
                    })
            })
            .map(|(dependency, _)| dependency)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitState {
    Visiting,
    Done,
}
