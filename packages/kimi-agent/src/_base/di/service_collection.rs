use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::descriptors::{DynamicValue, SyncDescriptor};
use super::instantiation::{
    ServiceCollectionLike, ServiceIdentifier, refine_service_decorator,
};
use crate::_base::event::{Emitter, Subscription};

#[derive(Clone)]
pub enum ServiceValue {
    Instance(DynamicValue),
    Descriptor(SyncDescriptor<DynamicValue>),
}

#[derive(Clone)]
pub struct ServiceCollectionEntry {
    pub value: ServiceValue,
    pub uid: u64,
    pub config: Option<DynamicValue>,
    pub recipe: Option<SyncDescriptor<DynamicValue>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AvailabilityChange {
    pub old_uid: Option<u64>,
    pub new_uid: Option<u64>,
}

#[derive(Default)]
pub struct ServiceCollectionOptions {
    pub config: Option<DynamicValue>,
}

#[derive(Default)]
pub struct ServiceCollection {
    entries: Mutex<IndexMap<ServiceIdentifier<DynamicValue>, ServiceCollectionEntry>>,
    emitters: Mutex<IndexMap<ServiceIdentifier<DynamicValue>, Arc<Emitter<AvailabilityChange>>>>,
    next_uid: Mutex<u64>,
}

impl ServiceCollection {
    pub fn new(entries: Vec<(ServiceIdentifier<DynamicValue>, ServiceValue)>) -> Self {
        let collection = Self::default();
        for (id, value) in entries {
            collection.set_value(id, value, None);
        }
        collection
    }

    pub fn set_value(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        value: ServiceValue,
        options: Option<ServiceCollectionOptions>,
    ) -> Option<ServiceValue> {
        let mut next_uid = self.next_uid.lock();
        *next_uid += 1;
        let uid = *next_uid;
        drop(next_uid);
        let previous = self.entries.lock().insert(
            id.clone(),
            ServiceCollectionEntry {
                value,
                uid,
                config: options.and_then(|options| options.config),
                recipe: None,
            },
        );
        self.emitter_for(&id).fire(AvailabilityChange {
            old_uid: previous.as_ref().map(|entry| entry.uid),
            new_uid: Some(uid),
        });
        previous.map(|entry| entry.value)
    }

    pub fn set<T>(
        &self,
        id: ServiceIdentifier<T>,
        instance_or_descriptor: ServiceValue,
        options: Option<ServiceCollectionOptions>,
    ) -> Option<ServiceValue> {
        self.set_value(
            refine_service_decorator(&id),
            instance_or_descriptor,
            options,
        )
    }

    pub fn materialize<T>(&self, id: &ServiceIdentifier<T>, instance: DynamicValue) {
        let id = refine_service_decorator(id);
        let mut entries = self.entries.lock();
        let Some(previous) = entries.get(&id).cloned() else {
            return;
        };
        let ServiceValue::Descriptor(descriptor) = previous.value else {
            return;
        };
        entries.insert(
            id,
            ServiceCollectionEntry {
                value: ServiceValue::Instance(instance),
                uid: previous.uid,
                config: previous.config,
                recipe: Some(descriptor),
            },
        );
    }

    pub fn unmaterialize<T>(&self, id: &ServiceIdentifier<T>) {
        let id = refine_service_decorator(id);
        let mut entries = self.entries.lock();
        let Some(previous) = entries.get(&id).cloned() else {
            return;
        };
        let Some(recipe) = previous.recipe else {
            return;
        };
        entries.insert(
            id,
            ServiceCollectionEntry {
                value: ServiceValue::Descriptor(recipe),
                uid: previous.uid,
                config: previous.config,
                recipe: None,
            },
        );
    }

    pub fn set_config<T>(&self, id: &ServiceIdentifier<T>, config: DynamicValue) {
        if let Some(entry) = self
            .entries
            .lock()
            .get_mut(&refine_service_decorator(id))
        {
            entry.config = Some(config);
        }
    }

    pub fn delete<T>(&self, id: &ServiceIdentifier<T>) -> Option<ServiceValue> {
        let id = refine_service_decorator(id);
        let previous = self.entries.lock().shift_remove(&id)?;
        self.emitter_for(&id).fire(AvailabilityChange {
            old_uid: Some(previous.uid),
            new_uid: None,
        });
        Some(previous.value)
    }

    pub fn entry<T>(&self, id: &ServiceIdentifier<T>) -> Option<ServiceCollectionEntry> {
        self.entries
            .lock()
            .get(&refine_service_decorator(id))
            .cloned()
    }

    pub fn uid_of<T>(&self, id: &ServiceIdentifier<T>) -> Option<u64> {
        self.entry(id).map(|entry| entry.uid)
    }

    pub fn config_of<T>(&self, id: &ServiceIdentifier<T>) -> Option<DynamicValue> {
        self.entry(id).and_then(|entry| entry.config)
    }

    pub fn on_did_change<T>(
        &self,
        id: &ServiceIdentifier<T>,
        listener: impl Fn(AvailabilityChange) + Send + Sync + 'static,
    ) -> Subscription {
        self.emitter_for(&refine_service_decorator(id))
            .event()
            .subscribe(listener)
    }

    pub fn has<T>(&self, id: &ServiceIdentifier<T>) -> bool {
        self.entries
            .lock()
            .contains_key(&refine_service_decorator(id))
    }

    pub fn get_value<T>(&self, id: &ServiceIdentifier<T>) -> Option<ServiceValue> {
        self.entry(id).map(|entry| entry.value)
    }

    pub fn for_each_value(
        &self,
        mut callback: impl FnMut(ServiceIdentifier<DynamicValue>, ServiceValue),
    ) {
        let entries = self
            .entries
            .lock()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.value.clone()))
            .collect::<Vec<_>>();
        for (id, value) in entries {
            callback(id, value);
        }
    }

    pub fn dispose(&self) {
        let emitters = self
            .emitters
            .lock()
            .drain(..)
            .map(|(_, emitter)| emitter)
            .collect::<Vec<_>>();
        for emitter in emitters {
            emitter.dispose();
        }
    }

    fn emitter_for(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
    ) -> Arc<Emitter<AvailabilityChange>> {
        self.emitters
            .lock()
            .entry(id.clone())
            .or_insert_with(|| Arc::new(Emitter::new()))
            .clone()
    }
}

impl ServiceCollectionLike for ServiceCollection {
    fn set<T>(&mut self, id: ServiceIdentifier<T>, instance_or_descriptor: DynamicValue) {
        ServiceCollection::set(
            self,
            id,
            ServiceValue::Instance(instance_or_descriptor),
            None,
        );
    }

    fn get<T>(&self, id: &ServiceIdentifier<T>) -> Option<DynamicValue> {
        self.get_value(id).map(|value| match value {
            ServiceValue::Instance(value) => value,
            ServiceValue::Descriptor(descriptor) => Arc::new(descriptor),
        })
    }

    fn has<T>(&self, id: &ServiceIdentifier<T>) -> bool {
        ServiceCollection::has(self, id)
    }

    fn for_each(&self, callback: &mut dyn FnMut(&ServiceIdentifier<DynamicValue>, &DynamicValue)) {
        self.for_each_value(|id, value| {
            let value = match value {
                ServiceValue::Instance(value) => value,
                ServiceValue::Descriptor(descriptor) => Arc::new(descriptor) as DynamicValue,
            };
            callback(&id, &value);
        });
    }
}
